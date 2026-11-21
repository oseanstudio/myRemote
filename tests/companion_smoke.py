"""Exercise the built receiver with pyatv 0.18.0 and a disposable identity."""

import asyncio
from contextlib import contextmanager
from pathlib import Path
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
            assert await start_remote(client) == (identity, session_id)
            assert identity[1] == identity[2]
            await exercise_tv_remote(client, expect)
            await command(client, "_sessionStop", {"_sid": (session_id << 32) | 456})
            # Reopen Remote on the same transport after the former 15-second cutoff.
            await asyncio.sleep(16)
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


if __name__ == "__main__":
    executable = Path(sys.argv[1]).resolve(strict=True)
    # Resolve the executable before creating a short-lived pairing directory.
    with TemporaryDirectory(prefix="myremote-smoke-", dir=executable.parent) as directory:
        asyncio.run(exercise(executable, directory))
    print("Companion pairing, reconnect, authorization, and legacy and TVRemoteCore input passed.")
