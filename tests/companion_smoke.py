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


async def exercise(binary, state):
    with receiver(binary, state, "--pair") as (port, expect):
        pin = int(expect(r"Pairing code (\d{4})")[1])
        await pair(port, (pin % 9000) + 1000, allowed=False)
        credentials = await pair(port, pin, allowed=True)
        await pair(port, pin, allowed=False)

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
                await command(client, "_hidC", {"_hidC": 9, "_hBtS": 2})
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
    print("Companion pairing, reconnect, authorization, button, pointer and click passed.")
