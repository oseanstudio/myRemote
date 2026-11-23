"""Exercise the built receiver with pyatv 0.18.0 and a disposable identity."""

import asyncio
from contextlib import contextmanager
from pathlib import Path
import plistlib
from queue import Empty, Queue
import re
import subprocess
import sys
from tempfile import TemporaryDirectory
from threading import Thread

from pyatv import exceptions
from pyatv.auth.hap_srp import SRPAuthHandler
from pyatv.conf import ManualService
from pyatv.const import Protocol
from pyatv.protocols.companion.auth import CompanionPairSetupProcedure
from pyatv.protocols.companion.connection import CompanionConnection, FrameType
from pyatv.protocols.companion.plist_payloads.rti_text_operations import (
    get_rti_clear_text_payload,
    get_rti_input_text_payload,
)
from pyatv.protocols.companion.protocol import CompanionProtocol


@contextmanager
def receiver(binary, state, *options):
    process = subprocess.Popen(
        [binary, "--dry-run", "--verbose", "--ip", "127.0.0.1",
         "--name", "myRemote smoke test", "--state-dir", state, *options],
        stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        text=True, encoding="utf-8",
    )
    output = Queue()

    def collect():
        for line in process.stdout:
            output.put(line.strip())
        output.put(None)

    def expect(pattern):
        while True:
            try:
                line = output.get(timeout=10)
            except Empty as error:
                raise AssertionError(f"No output matching {pattern}") from error
            assert line is not None, f"Receiver exited before output matching {pattern}"
            if match := re.search(pattern, line):
                return match

    Thread(target=collect, daemon=True).start()
    try:
        port = int(expect(r"companion (\d+)")[1])
        yield port, expect
    finally:
        process.terminate()
        process.wait(timeout=10)
        process.stdout.close()


def protocol(port, credentials=None):
    connection = CompanionConnection(asyncio.get_running_loop(), "127.0.0.1", port)
    service = ManualService(None, Protocol.Companion, port, {}, credentials=credentials)
    return CompanionProtocol(connection, SRPAuthHandler(), service)


async def pair(port, pin, *, allowed):
    client = protocol(port)
    pairing = CompanionPairSetupProcedure(client, client.srp)
    try:
        await pairing.start_pairing()
        credentials = await pairing.finish_pairing("Pair-Setup", pin, "myRemote smoke test")
    except exceptions.AuthenticationError:
        assert not allowed, "Valid PIN was rejected"
        return None
    finally:
        client.stop()
    assert allowed, "Pair setup succeeded with an invalid PIN or closed pairing window"
    return str(credentials)


async def command(client, name, values, frame=FrameType.E_OPACK):
    return await client.exchange_opack(frame, {"_i": name, "_t": 2, "_c": values}, timeout=2.0)


async def start_remote(client):
    reply = await client.exchange_opack(
        FrameType.E_OPACK,
        {
            "model": "_systemInfo",
            "_i": None,
            "_t": 2,
            "_c": {"model": "iPhone17,1", "name": "Test phone", "_clFl": 128},
        },
        timeout=2.0,
    )
    assert set(reply) == {"_t", "_x", "_c"}
    assert reply["_c"]["_stA"] == ["com.apple.tvremoteservices"]
    assert reply["_c"]["model"] == "AppleTV14,1"
    assert reply["_c"]["name"] == "myRemote smoke test"
    assert reply["_c"]["_lP"] == client.connection.port
    device = reply["_c"]

    # Extra frames and keepalives can arrive after verification. Nonempty encrypted
    # frames consume a nonce even when their contents are unused by the Remote.
    client.connection.send(FrameType.NoOp, b"")
    client.connection.send(FrameType.SessionData, b"optional session data")
    client.connection.send(FrameType.E_OPACK, b"\xff")
    reply = await command(
        client, "_sessionStart", {"_srvT": "com.apple.tvremoteservices", "_sid": 456}
    )
    assert set(reply) == {"_t", "_x", "_c"}
    session_id = reply["_c"]["_sid"]
    assert 0 < session_id <= 0xFFFFFFFF
    return (device["_pubID"], device["_spID"], device.get("_mrID")), session_id


def touch_event(client, phase, **coordinates):
    client.send_opack(
        FrameType.E_OPACK,
        {
            "_i": "HIDTouch",
            "_t": 1,
            "_c": {"TouchPhase": phase, "TouchFinger": 1, **coordinates},
        },
    )


async def exercise_tv_remote(client, expect):
    start = await command(client, "TVRCSessionStart", {"ProtocolVersionKey": "1.2"})
    assert start["_c"]["ProtocolVersionKey"] == "1.2"
    apps = await command(client, "FetchLaunchableApplicationsEvent", {})
    assert apps["_c"] == {"AppCountKey": 0, "AppInfos": {}}
    start_touch = await command(
        client, "HIDTouchStart", {"Width": 1000, "Height": 1000, "TouchFlags": 0}
    )
    assert start_touch["_c"]["Identifier"] == 1

    for phase, x, y in [(1, 350, 340), (2, 394, 355), (3, 433, 380)]:
        touch_event(client, phase, CoordinateX=x, CoordinateY=y)
    await command(client, "FetchAttentionState", {})
    expect(r"^Received pointer motion \(44.0, 15.0\)$")
    expect(r"^Received pointer motion \(39.0, 25.0\)$")

    touch_event(client, 1, CoordinateX=120, CoordinateY=120)
    touch_event(client, 4)
    touch_event(client, 3, CoordinateX=120, CoordinateY=120)
    for state in [1, 2]:
        await command(client, "HIDCommand", {"HIDCommand": 5, "HIDButtonState": state})
    assert expect(r"^Received (.*)$")[1] == "button menu"

    touch_event(client, 1, CoordinateX=125, CoordinateY=120)
    touch_event(client, 3, CoordinateX=125, CoordinateY=120)
    await command(client, "HIDTouchStop", {})
    assert expect(r"^Received (.*)$")[1] == "click"
    await command(
        client, "MediaControlCommand", {"MediaControlCommand": 6, "MessageKeyVolume": 0.25}
    )
    volume = await command(client, "MediaControlCommand", {"MediaControlCommand": 5})
    assert volume["_c"]["MessageKeyVolume"] == 0.25
    legacy = await command(client, "_mcc", {"_mcc": 5})
    assert legacy["_c"]["_vol"] == 0.25


class RemoteEvents:
    def __init__(self, client, *names):
        self.events = asyncio.Queue()
        self.names = set(names)
        client.listener = self

    def event_received(self, name, content):
        if name in self.names:
            self.events.put_nowait((name, content))

    async def expect(self, name):
        event, content = await asyncio.wait_for(self.events.get(), timeout=5)
        assert event == name, (event, content)
        return content


def keyboard_id(content):
    archive = plistlib.loads(content["_tiD"])
    objects = archive["$objects"]
    top = archive["$top"]
    document = objects[top["documentState"].data]
    context = objects[document["docSt"].data]
    for key in ("contextBeforeInput", "contextAfterInput", "selectedText"):
        assert objects[context[key].data] == ""
    identifier = objects[top["sessionUUID"].data]["NS.uuidbytes"]
    assert len(identifier) == 16
    return identifier


def keyboard_edit(client, payload):
    client.send_opack(
        FrameType.E_OPACK,
        {"_i": "_tiC", "_t": 1, "_c": {"_tiV": 1, "_tiD": payload}},
    )


async def exercise_two_phones(port, credentials, second_credentials, expect):
    first = protocol(port, credentials)
    second = protocol(port, second_credentials)
    try:
        await asyncio.gather(first.start(), second.start())
        (_, first_session), (_, second_session) = await asyncio.gather(
            start_remote(first), start_remote(second)
        )
        assert first_session != second_session
        first_events = RemoteEvents(first, "_tiStarted", "_tiStopped")
        second_events = RemoteEvents(second, "_tiStarted", "_tiStopped")
        for client in (first, second):
            assert (await command(client, "_tiStart", {}))["_c"] == {}

        # A dry-run click simulates an editable field getting focus.
        await command(second, "HIDCommand", {"HIDCommand": 6, "HIDButtonState": 2})
        expect(r"^Received click$")
        first_id = keyboard_id(await first_events.expect("_tiStarted"))
        second_id = keyboard_id(await second_events.expect("_tiStarted"))
        assert first_id != second_id
        assert keyboard_id((await command(second, "_tiStart", {}))["_c"]) == second_id

        keyboard_edit(first, b"not a plist")
        keyboard_edit(first, get_rti_input_text_payload(second_id, "wrong session"))
        keyboard_edit(first, get_rti_clear_text_payload(first_id))
        keyboard_edit(first, get_rti_input_text_payload(first_id, "é🙂"))
        await command(first, "FetchAttentionState", {})
        assert expect(r"^Received (.*)$")[1] == "keyboard edit (0 backspaces, 0 deletes, 2 characters)"

        payload = plistlib.loads(get_rti_input_text_payload(second_id, ""))
        operations = payload["$objects"][payload["$top"]["textOperations"].data]
        keyboard_output = payload["$objects"][operations["keyboardOutput"].data]
        keyboard_output["deletionCount"] = 1
        keyboard_edit(second, plistlib.dumps(payload, fmt=plistlib.FMT_BINARY))
        await command(second, "FetchAttentionState", {})
        assert expect(r"^Received (.*)$")[1] == "keyboard edit (1 backspaces, 0 deletes, 0 characters)"

        # A completed swipe can arrive as a whole word, followed by a
        # correction that removes the old word and inserts the replacement.
        keyboard_edit(second, get_rti_input_text_payload(second_id, "swpie "))
        payload = plistlib.loads(get_rti_input_text_payload(second_id, "swipe "))
        operations = payload["$objects"][payload["$top"]["textOperations"].data]
        keyboard_output = payload["$objects"][operations["keyboardOutput"].data]
        keyboard_output["deletionCount"] = len("swpie ")
        keyboard_edit(second, plistlib.dumps(payload, fmt=plistlib.FMT_BINARY))
        await command(second, "FetchAttentionState", {})
        assert expect(r"^Received (.*)$")[1] == "keyboard edit (0 backspaces, 0 deletes, 6 characters)"
        assert expect(r"^Received (.*)$")[1] == "keyboard edit (6 backspaces, 0 deletes, 6 characters)"

        # Native keyboard edits can commit text through a named output field
        # and leave their default deletion count as a nil archive reference.
        keyboard_output["deletionCount"] = plistlib.UID(0)
        keyboard_output["textToCommit"] = keyboard_output.pop("insertionText")
        await command(second, "_tiC", {
            "_tiD": plistlib.dumps(payload, fmt=plistlib.FMT_BINARY),
        })
        assert expect(r"^Received (.*)$")[1] == "keyboard edit (0 backspaces, 0 deletes, 6 characters)"

        # A wrapped archive can carry the field UUID alongside the operations
        # and an NSDictionary with a boxed deletion count and attributed commit.
        wrapper = len(payload["$objects"])
        payload["$objects"].append({
            "textOperations": payload["$top"]["textOperations"],
            "sessionUUID": operations.pop("targetSessionUUID"),
        })
        payload["$top"] = {"inputOperation": plistlib.UID(wrapper)}
        text_value = keyboard_output["textToCommit"]
        box = len(payload["$objects"])
        payload["$objects"].extend([{"NS.intval": 6}, {"NSString": text_value}])
        keyboard_output.clear()
        keyboard_output.update({
            "NS.keys": ["deletionCount", "textToCommit"],
            "NS.objects": [plistlib.UID(box), plistlib.UID(box + 1)],
        })
        await command(second, "_tiC", {
            "_tiV": 2,
            "_tiD": plistlib.dumps(payload, fmt=plistlib.FMT_BINARY),
        })
        assert expect(r"^Received (.*)$")[1] == "keyboard edit (6 backspaces, 0 deletes, 6 characters)"

        await command(first, "_sessionStop", {"_sid": (first_session << 32) | 456})
        await first_events.expect("_tiStopped")
        keyboard_edit(first, get_rti_input_text_payload(first_id, "closed"))
        await command(first, "FetchAttentionState", {})
        first.stop()

        keyboard_edit(second, get_rti_input_text_payload(second_id, "still connected"))
        await command(second, "FetchAttentionState", {})
        assert expect(r"^Received (.*)$")[1] == "keyboard edit (0 backspaces, 0 deletes, 15 characters)"
        await asyncio.sleep(0.55)
        await command(second, "HIDCommand", {"HIDCommand": 6, "HIDButtonState": 2})
        expect(r"^Received click$")
        await second_events.expect("_tiStopped")
        replacement_id = keyboard_id(await second_events.expect("_tiStarted"))
        assert replacement_id != second_id
        keyboard_edit(second, get_rti_input_text_payload(second_id, "old focus"))
        keyboard_edit(second, get_rti_input_text_payload(replacement_id, "new focus"))
        await command(second, "FetchAttentionState", {})
        assert expect(r"^Received (.*)$")[1] == "keyboard edit (0 backspaces, 0 deletes, 9 characters)"
        await command(second, "HIDCommand", {"HIDCommand": 5, "HIDButtonState": 2})
        expect(r"^Received button menu$")
        await second_events.expect("_tiStopped")
        keyboard_edit(second, get_rti_input_text_payload(replacement_id, "no focus"))
        await command(second, "HIDCommand", {"HIDCommand": 4, "HIDButtonState": 2})
        assert expect(r"^Received (.*)$")[1] == "button right"
    finally:
        first.stop()
        second.stop()


async def exercise_reconnect(port, credentials, second_credentials, expect):
    original = protocol(port, credentials)
    replacement = protocol(port, credentials)
    other = protocol(port, second_credentials)
    try:
        await original.start()
        await other.start()
        _, original_id = await start_remote(original)
        await start_remote(other)
        events = RemoteEvents(original, "_tiStarted", "_tiStopped")
        await command(original, "_tiStart", {})
        await command(original, "HIDCommand", {"HIDCommand": 6, "HIDButtonState": 2})
        expect(r"^Received click$")
        field = keyboard_id(await events.expect("_tiStarted"))

        await replacement.start()
        await command(replacement, "_systemInfo", {"_clFl": 128}, FrameType.P_OPACK)
        assert original.connection.connected
        await command(replacement, "_tiC", {
            "_tiV": 1,
            "_tiD": get_rti_input_text_payload(field, "alternate connection"),
        }, FrameType.P_OPACK)
        assert expect(r"^Received (.*)$")[1] == "keyboard edit (0 backspaces, 0 deletes, 20 characters)"

        _, replacement_id = await start_remote(replacement)
        assert original_id == replacement_id

        async def wait_for_replacement():
            while original.connection.connected:
                await asyncio.sleep(0.01)

        await asyncio.wait_for(wait_for_replacement(), timeout=1)
        assert other.connection.connected

        # A replaced socket's field must no longer accept edits.
        keyboard_edit(replacement, get_rti_input_text_payload(field, "expired"))
        new_field = keyboard_id((await command(replacement, "_tiStart", {}))["_c"])
        assert new_field != field
        keyboard_edit(replacement, get_rti_input_text_payload(new_field, "reconnected"))
        await command(replacement, "FetchAttentionState", {})
        assert expect(r"^Received (.*)$")[1] == "keyboard edit (0 backspaces, 0 deletes, 11 characters)"

        replacement_events = RemoteEvents(replacement, "_tiStarted", "_tiStopped")
        await command(replacement, "TVRCSessionStop", {})
        await replacement_events.expect("_tiStopped")
        await command(replacement, "TVRCSessionStart", {"ProtocolVersionKey": "1.2"})
        reopened_field = keyboard_id(await replacement_events.expect("_tiStarted"))
        assert reopened_field != new_field
        keyboard_edit(replacement, get_rti_input_text_payload(reopened_field, "warm reopen"))
        await command(replacement, "FetchAttentionState", {})
        assert expect(r"^Received (.*)$")[1] == "keyboard edit (0 backspaces, 0 deletes, 11 characters)"
        await command(other, "HIDCommand", {"HIDCommand": 4, "HIDButtonState": 2})
        expect(r"^Received button right$")
    finally:
        original.stop()
        replacement.stop()
        other.stop()


async def exercise_gestures(port, credentials, second_credentials, expect):
    client = protocol(port, credentials)
    other = protocol(port, second_credentials)

    def touch(phase, x=120, y=120):
        touch_event(client, phase, CoordinateX=x, CoordinateY=y)

    def tap():
        touch(1)
        touch(3)

    async def flush_input():
        await command(client, "FetchAttentionState", {})

    try:
        await asyncio.gather(client.start(), other.start())
        await start_remote(client)
        await start_remote(other)

        tap()
        tap()
        # Some versions send HID Select as well as the touch sequence.
        await command(client, "HIDCommand", {"HIDCommand": 6, "HIDButtonState": 2})
        assert expect(r"^Received (.*)$")[1] == "click"
        assert expect(r"^Received (.*)$")[1] == "double-click"

        # Some Remote layouts send only HID Select. A native double tap
        # must work on this path as well as the touch-event path.
        for _ in range(2):
            await command(client, "HIDCommand", {"HIDCommand": 6, "HIDButtonState": 2})
        assert expect(r"^Received (.*)$")[1] == "click"
        assert expect(r"^Received (.*)$")[1] == "double-click"

        touch_event(client, 1, CoordinateX=120, CoordinateY=120, Nanoseconds=1_000_000_000)
        touch_event(client, 3, Nanoseconds=1_050_000_000)
        await flush_input()
        assert expect(r"^Received (.*)$")[1] == "click"
        # Arrival times may be delayed or batched on Wi-Fi. Phone timestamps
        # still put these taps inside the double-click interval.
        await asyncio.sleep(0.55)
        touch_event(client, 1, CoordinateX=120, CoordinateY=120, Nanoseconds=1_300_000_000)
        touch_event(client, 3, Nanoseconds=1_350_000_000)
        await flush_input()
        assert expect(r"^Received (.*)$")[1] == "double-click"

        touch(1)
        await flush_input()
        assert expect(r"^Received (.*)$")[1] == "right-click"
        touch(3)
        await command(client, "HIDCommand", {"HIDCommand": 6, "HIDButtonState": 2})

        tap()
        touch(1)
        await flush_input()
        assert expect(r"^Received (.*)$")[1] == "click"
        assert expect(r"^Received (.*)$")[1] == "drag start"
        touch(2, x=162, y=135)
        touch(3, x=162, y=135)
        await flush_input()
        assert expect(r"^Received (.*)$")[1] == "drag motion (42.0, 15.0)"
        expect(r"^Received drag end$")

        tap()
        touch(1)
        await flush_input()
        assert expect(r"^Received (.*)$")[1] == "click"
        expect(r"^Received drag start$")
        client.stop()
        await command(other, "FetchAttentionState", {})
        expect(r"^Received drag end$")
        await command(other, "HIDCommand", {"HIDCommand": 6, "HIDButtonState": 2})
        assert expect(r"^Received (.*)$")[1] == "click"
        for code, button in [(14, "play_pause"), (7, "home"), (5, "menu")]:
            await command(other, "HIDCommand", {"HIDCommand": code, "HIDButtonState": 2})
            assert expect(r"^Received (.*)$")[1] == f"button {button}"
    finally:
        client.stop()
        other.stop()


async def exercise_media(port, credentials, expect):
    client = protocol(port, credentials)

    def now_playing_values(content):
        archive = plistlib.loads(content["NowPlayingInfoKey"])
        assert archive["$archiver"] == "NSKeyedArchiver"
        objects = archive["$objects"]
        info = objects[archive["$top"]["root"].data]
        assert objects[info["$class"].data]["$classname"] == "TVRCNowPlayingInfo"
        values = {key: objects[info[key].data] for key in
                  ("playerIdentifier", "playbackRate", "hasValidCaptionOptions", "captionsEnabled")}
        metadata = objects[info["metadata"].data]
        if isinstance(metadata, dict):
            values["duration"] = objects[metadata["duration"].data]
        return values

    try:
        await client.start()
        await start_remote(client)
        events = RemoteEvents(client, "MediaControlStatus", "_iMC", "NowPlayingInfo")

        async def state(flags, expected_player, caption_options, captions_enabled):
            assert (await events.expect("MediaControlStatus"))["MediaControlFlags"] == flags
            assert (await events.expect("_iMC"))["_mcF"] == flags
            info = now_playing_values(await events.expect("NowPlayingInfo"))
            assert info["playerIdentifier"] == expected_player
            assert info["hasValidCaptionOptions"] == caption_options
            assert info["captionsEnabled"] == captions_enabled
            return info

        assert (await command(client, "FetchMediaControlStatus", {}))["_c"]["MediaControlFlags"] == 0x0100
        await command(client, "_mcc", {"_mcc": 7, "_skpS": 10.0})
        await command(client, "HIDCommand", {"HIDCommand": 14, "HIDButtonState": 2})
        assert expect(r"^Received (.*)$")[1] == "button play_pause"
        media = await state(0x0733, "myRemote.YouTube", True, False)
        assert media["playbackRate"] == 1.0 and media["duration"] == 600

        await command(client, "_mcc", {"_mcc": 7, "_skpS": float("inf")})
        await command(client, "_mcc", {"_mcc": 7, "_skpS": 0.0})
        await command(client, "_mcc", {"_mcc": 7, "_skpS": 10.0})
        await command(client, "MediaControlCommand",
                      {"MediaControlCommand": 7, "MessageKeySkipSeconds": -10.0})
        assert expect(r"^Received (.*)$")[1] == "playback YouTube SkipForward"
        assert expect(r"^Received (.*)$")[1] == "playback YouTube SkipBackward"
        await command(client, "ToggleCaptions", {})
        assert expect(r"^Received (.*)$")[1] == "playback YouTube ToggleCaptions"
        await state(0x0733, "myRemote.YouTube", True, True)
        assert (await command(client, "_mcc", {"_mcc": 12}))["_c"]["_cse"] is True

        await command(client, "FetchCurrentNowPlayingInfoEvent", {})
        await state(0x0733, "myRemote.YouTube", True, True)
        await command(client, "HIDCommand", {"HIDCommand": 7, "HIDButtonState": 2})
        expect(r"^Received button home$")
        await state(0x0733, "myRemote.Jellyfin", False, "$null")
        await command(client, "_mcc", {"_mcc": 7, "_skpS": -10.0})
        assert expect(r"^Received (.*)$")[1] == "playback Jellyfin SkipBackward"

        await command(client, "HIDCommand", {"HIDCommand": 5, "HIDButtonState": 2})
        expect(r"^Received button menu$")
        await state(0x0100, "$null", "$null", "$null")
        await command(client, "_mcc", {"_mcc": 7, "_skpS": float("inf")})
        await command(client, "_mcc", {"_mcc": 7, "_skpS": -10.0})
        await command(client, "HIDCommand", {"HIDCommand": 4, "HIDButtonState": 2})
        assert expect(r"^Received (.*)$")[1] == "button right"
    finally:
        client.stop()


async def exercise(binary, state):
    with receiver(binary, state, "--pair") as (port, expect):
        pin = int(expect(r"Pairing code (\d{4})")[1])
        await pair(port, (pin % 9000) + 1000, allowed=False)
        credentials = await pair(port, pin, allowed=True)
        await pair(port, pin, allowed=False)
        client = protocol(port, credentials)
        try:
            await client.start()
            identity, session_id = await start_remote(client)
        finally:
            client.stop()

    with receiver(binary, state, "--pair") as (port, expect):
        pin = int(expect(r"Pairing code (\d{4})")[1])
        second_credentials = await pair(port, pin, allowed=True)

    with receiver(binary, state) as (port, expect):
        plaintext = protocol(port)
        try:
            await plaintext.start()
            try:
                await command(plaintext, "_hidC", {"_hidC": 9, "_hBtS": 2}, FrameType.U_OPACK)
            except asyncio.TimeoutError:
                pass
            else:
                raise AssertionError("Receiver accepted a plaintext input command")
        finally:
            plaintext.stop()

        unknown = credentials.split(":")
        unknown[1] = "55" * 32  # Replace controller signing key, keep its identifier.
        client = protocol(port, ":".join(unknown))
        try:
            # pyatv 0.18 enables its cipher even after a Pair Verify error reply.
            await client.start()
            try:
                await command(client, "HIDCommand", {"HIDCommand": 9, "HIDButtonState": 2})
            except (asyncio.TimeoutError, exceptions.ConnectionLostError):
                pass
            else:
                raise AssertionError("Receiver accepted an unverified input command")
        except exceptions.AuthenticationError:
            pass
        finally:
            client.stop()

        client = protocol(port, credentials)
        try:
            await client.start()
            # A background probe is already verified. It can become the
            # foreground Remote later without another Pair Verify.
            client._xid |= 0x80000000
            probe = await command(client, "_systemInfo", {"_clFl": 128})
            assert probe["_c"]["_lP"] == port
            await asyncio.sleep(16)
            assert await start_remote(client) == (identity, session_id)
            assert identity[1] == identity[2]
            await exercise_tv_remote(client, expect)
            await command(client, "TVRCSessionStop", {})
            assert await start_remote(client) == (identity, session_id)
            await command(client, "_sessionStop", {"_sid": (session_id << 32) | 456})
            assert await start_remote(client) == (identity, session_id)
            await command(client, "HIDCommand", {"HIDCommand": 4, "HIDButtonState": 2})
            expect(r"^Received button right$")
        finally:
            client.stop()

        client = protocol(port, credentials)
        try:
            await client.start()
            await command(client, "_sessionStart", {"_sid": 123})
            await command(client, "_hidC", {"_hidC": 1, "_hBtS": 2})
            expect(r"^Received button up$")
            for phase, x, y in [(1, 100, 100), (2, 140, 115), (4, 210, 130)]:
                await command(client, "_hidT", {"_tPh": phase, "_cx": x, "_cy": y})
            expect(r"^Received pointer motion \(40.0, 15.0\)$")
            expect(r"^Received pointer motion \(70.0, 15.0\)$")
            await command(client, "_hidC", {"_hidC": 6, "_hBtS": 2})
            expect(r"^Received click$")
        finally:
            client.stop()

    with receiver(binary, state) as (port, expect):
        await exercise_two_phones(port, credentials, second_credentials, expect)

    with receiver(binary, state) as (port, expect):
        await exercise_reconnect(port, credentials, second_credentials, expect)

    with receiver(binary, state) as (port, expect):
        await exercise_gestures(port, credentials, second_credentials, expect)

    with receiver(binary, state) as (port, expect):
        await exercise_media(port, credentials, expect)


if __name__ == "__main__":
    executable = Path(sys.argv[1]).resolve(strict=True)
    # Resolve the executable before creating a short-lived pairing directory.
    with TemporaryDirectory(prefix="myremote-smoke-", dir=executable.parent) as directory:
        asyncio.run(exercise(executable, directory))
    print("Companion pairing, reconnect, simultaneous controllers, keyboard, gestures, and media passed.")
