"""Verify that Apple's Bonjour resolves the receiver's native advertisements."""

import asyncio
from pathlib import Path
import re
import sys
from tempfile import TemporaryDirectory
from uuid import uuid4


async def stop(process):
    if process.returncode is None:
        process.terminate()
        try:
            await asyncio.wait_for(process.wait(), 3)
        except TimeoutError:
            process.kill()
            await process.wait()


async def find(process, pattern):
    while line := await process.stdout.readline():
        if match := re.search(pattern, line.decode(errors="replace")):
            return match
    raise AssertionError(f"Process ended before printing {pattern}")


async def lookup(name, service, expected_port):
    browser = await asyncio.create_subprocess_exec(
        "/usr/bin/dns-sd", "-L", name, service, "local.",
        stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT,
    )
    try:
        result = await asyncio.wait_for(find(browser, r"can be reached at [^\s]+:(\d+)"), 15)
        assert int(result[1]) == expected_port, (service, result[1], expected_port)
    finally:
        await stop(browser)


async def browse_peer_airplay(name):
    browser = await asyncio.create_subprocess_exec(
        "/usr/bin/dns-sd", "-B", "_airplay-p2p._tcp", "local.",
        stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT,
    )
    try:
        match = await asyncio.wait_for(
            find(browser, rf"\bAdd\b.*?([0-9A-F]{{12}}@{re.escape(name)} · Nearby)\s*$"), 15,
        )
        return match[1]
    finally:
        await stop(browser)


async def smoke(binary, directory):
    name = f"myRemote-Bonjour-{uuid4().hex[:8]}"
    server = await asyncio.create_subprocess_exec(
        str(binary), "--name", name, "--dry-run", "--verbose", "--state-dir", directory,
        "--nearby",
        stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT,
    )
    try:
        endpoints = r".*discovery (\d+)\s+companion (\d+)\s+AirPlay discovery (\d+)"
        nearby = await asyncio.wait_for(
            find(server, r"^Nearby ports" + endpoints + r"\s+AirPlay peer discovery (\d+)"), 12,
        )
        ports = await asyncio.wait_for(find(server, r"^TCP ports" + endpoints), 10)
        for instance, match in [(name, ports), (name + " · Nearby", nearby)]:
            for service, port in zip(
                ["_mediaremotetv._tcp", "_companion-link._tcp", "_airplay._tcp"], match.groups()
            ):
                await lookup(instance, service, int(port))
        peer_port = int(nearby[4])
        await lookup(await browse_peer_airplay(name), "_airplay-p2p._tcp", peer_port)
        reader, writer = await asyncio.open_connection("127.0.0.1", peer_port)
        try:
            writer.write(b"GET /info HTTP/1.1\r\nHost: localhost\r\n\r\n")
            await writer.drain()
            headers = await asyncio.wait_for(reader.readuntil(b"\r\n\r\n"), 5)
            assert headers.startswith(b"HTTP/1.1 200 OK\r\n")
            assert await asyncio.wait_for(reader.readexactly(2), 5) == b"OK"
        finally:
            writer.close()
            await writer.wait_closed()
        for address in ["127.0.0.1", "::1"]:
            _, writer = await asyncio.open_connection(address, int(ports[2]))
            writer.close()
            await writer.wait_closed()
    finally:
        await stop(server)


if __name__ == "__main__":
    with TemporaryDirectory(prefix="myremote-bonjour-") as directory:
        asyncio.run(smoke(Path(sys.argv[1]).resolve(strict=True), directory))
    print("Apple Bonjour resolved all LAN and Nearby services, including _airplay-p2p._tcp.")
