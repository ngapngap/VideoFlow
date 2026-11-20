from __future__ import annotations

import asyncio
import base64
import ipaddress
import struct
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from muse2api.accounts.model import Account
from muse2api.config import Settings
from muse2api.drivers.base import SessionInfo
from muse2api.drivers.browser.chromium import ChromiumProcess
from muse2api.drivers.browser.driver import BrowserDriver
from muse2api.drivers.browser.proxy_relay import (
    LocalProxyRelay,
    mask_proxy,
    needs_proxy_relay,
)


def test_mask_proxy():
    assert mask_proxy(None) == "direct"
    assert mask_proxy("") == "direct"
    assert mask_proxy("http://127.0.0.1:8080") == "http://127.0.0.1:8080"
    assert (
        mask_proxy("http://alice:s3cr3t@10.0.0.1:8188")
        == "http://alice:***@10.0.0.1:8188"
    )
    assert (
        mask_proxy("https://user:pass@proxy.example.com:443")
        == "https://user:***@proxy.example.com:443"
    )


def test_needs_proxy_relay():
    assert needs_proxy_relay("http://user:pass@10.0.0.1:8188") is True
    assert needs_proxy_relay("https://user:pass@10.0.0.1:8443") is True
    assert needs_proxy_relay("http://user@10.0.0.1:8188") is True
    assert needs_proxy_relay("http://10.0.0.1:8188") is False
    assert needs_proxy_relay("socks5://user:pass@10.0.0.1:1080") is True
    assert needs_proxy_relay("socks5://user@10.0.0.1:1080") is True
    assert needs_proxy_relay("socks5://:pass@10.0.0.1:1080") is True
    assert needs_proxy_relay("socks5://10.0.0.1:1080") is False
    assert needs_proxy_relay("socks5h://10.0.0.1:1080") is True
    assert needs_proxy_relay("socks5h://user:pass@10.0.0.1:1080") is True
    assert needs_proxy_relay("socks4://10.0.0.1:1080") is False
    assert needs_proxy_relay("") is False
    assert needs_proxy_relay(None) is False


@pytest.mark.asyncio
async def test_local_proxy_relay_connect_tunnel():
    received_headers: list[bytes] = []
    expected_auth = (
        b"Proxy-Authorization: Basic "
        + base64.b64encode(b"alice:p@ss:word").decode("ascii").encode("ascii")
    )

    async def fake_upstream(
        reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        head = await reader.readuntil(b"\r\n\r\n")
        received_headers.append(head)
        if expected_auth in head:
            writer.write(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            await writer.drain()
            payload = await reader.read(64)
            writer.write(b"ECHO:" + payload)
            await writer.drain()
        else:
            writer.write(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n")
            await writer.drain()
        writer.close()
        await writer.wait_closed()

    upstream_srv = await asyncio.start_server(fake_upstream, "127.0.0.1", 0)
    upstream_port = upstream_srv.sockets[0].getsockname()[1]

    relay = LocalProxyRelay(f"http://alice:p%40ss%3Aword@127.0.0.1:{upstream_port}")
    try:
        local_url = await relay.start()
        assert local_url.startswith("http://127.0.0.1:")
        assert relay.local_port is not None

        reader, writer = await asyncio.open_connection("127.0.0.1", relay.local_port)
        writer.write(b"CONNECT muse.ai:443 HTTP/1.1\r\nHost: muse.ai:443\r\n\r\n")
        await writer.drain()

        resp = await reader.readuntil(b"\r\n\r\n")
        assert resp.startswith(b"HTTP/1.1 200")

        writer.write(b"hello-tunnel")
        await writer.drain()
        echoed = await reader.read(64)
        assert echoed == b"ECHO:hello-tunnel"

        writer.close()
        await writer.wait_closed()
        assert received_headers and expected_auth in received_headers[0]
    finally:
        await relay.stop()
        upstream_srv.close()
        await upstream_srv.wait_closed()


@pytest.mark.asyncio
async def test_local_proxy_relay_plain_http():
    expected_auth = (
        b"Proxy-Authorization: Basic "
        + base64.b64encode(b"bob:secret123").decode("ascii").encode("ascii")
    )
    seen: list[bytes] = []

    async def fake_upstream(
        reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        head = await reader.readuntil(b"\r\n\r\n")
        seen.append(head)
        writer.write(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK")
        await writer.drain()
        writer.close()
        await writer.wait_closed()

    upstream_srv = await asyncio.start_server(fake_upstream, "127.0.0.1", 0)
    upstream_port = upstream_srv.sockets[0].getsockname()[1]

    relay = LocalProxyRelay(f"http://bob:secret123@127.0.0.1:{upstream_port}")
    try:
        await relay.start()
        assert relay.local_port is not None
        reader, writer = await asyncio.open_connection("127.0.0.1", relay.local_port)
        writer.write(
            b"GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n"
        )
        await writer.drain()
        data = await reader.read()
        assert b"200 OK" in data
        assert data.endswith(b"OK")
        writer.close()
        await writer.wait_closed()
        assert seen and expected_auth in seen[0]
    finally:
        await relay.stop()
        upstream_srv.close()
        await upstream_srv.wait_closed()


@pytest.mark.asyncio
async def test_chromium_process_uses_local_relay_for_auth_proxy(tmp_path: Path):
    profile_dir = tmp_path / "profile_auth"
    profile_dir.mkdir(parents=True)

    proc = ChromiumProcess(
        executable="msedge.exe",
        port=19299,
        profile_dir=profile_dir,
        headless=True,
        proxy="http://namdm:supersecret@10.0.0.1:8188",
    )

    launched_args: list[str] = []

    async def fake_exec(*args, **kwargs):
        launched_args.extend(args)
        m = MagicMock()
        m.returncode = None
        m.terminate = MagicMock()
        m.wait = AsyncMock(return_value=0)
        return m

    with (
        patch("asyncio.create_subprocess_exec", side_effect=fake_exec),
        patch(
            "muse2api.drivers.browser.chromium._kill_stale_browsers",
            return_value=0,
        ),
        patch.object(
            proc,
            "browser_ws_url",
            new=AsyncMock(
                side_effect=[None, "ws://127.0.0.1:19299/devtools/browser/1"]
            ),
        ),
    ):
        ws = await proc.start()
        assert ws == "ws://127.0.0.1:19299/devtools/browser/1"

    proxy_args = [a for a in launched_args if a.startswith("--proxy-server=")]
    assert len(proxy_args) == 1
    assert proxy_args[0].startswith("--proxy-server=http://127.0.0.1:")
    assert "supersecret" not in " ".join(launched_args)
    assert proc._proxy_relay is not None

    await proc.stop()
    assert proc._proxy_relay is None


@pytest.mark.asyncio
async def test_browser_driver_renew_session_passes_account_proxy(tmp_path: Path):
    settings = Settings(data_dir=tmp_path / "data")
    acc = Account(
        id="acc_proxy_renew",
        cookies={"hatch_sess": "s", "hatch_native_auth_device": "d"},
        proxy="http://namdm:secret@10.0.0.1:8188",
    )
    drv = BrowserDriver(settings, account=acc, proxy=acc.proxy)

    with patch(
        "muse2api.drivers.browser.driver.muse.renew_session",
        new=AsyncMock(return_value=SessionInfo(ok=True, cookies=acc.cookies)),
    ) as mock_renew:
        info = await drv.renew_session(acc)
        assert info.ok is True
        mock_renew.assert_awaited_once_with(
            acc.cookies, proxy="http://namdm:secret@10.0.0.1:8188"
        )


@pytest.mark.asyncio
async def test_root_coordinator_startup_skips_unused_default_chromium(tmp_path: Path):
    settings = Settings(data_dir=tmp_path / "data")
    root_drv = BrowserDriver(settings)

    with (
        patch(
            "muse2api.drivers.browser.driver.find_chromium",
            return_value="msedge.exe",
        ),
        patch(
            "muse2api.drivers.browser.driver.ChromiumProcess.start",
            new=AsyncMock(side_effect=AssertionError("root coordinator must not spawn Chromium")),
        ),
    ):
        await root_drv.startup()
        h = await root_drv.health()
        assert h["ok"] is True
        assert h["browser_alive"] is False
        assert root_drv._chromium is None


async def _start_mock_socks5_server(
    *,
    require_auth: tuple[str, str] | None = None,
    mode: str = "echo",
):
    recorded: list[dict[str, object]] = []

    async def handle(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        try:
            ver_n = await reader.readexactly(2)
            assert ver_n[0] == 0x05
            methods = await reader.readexactly(ver_n[1])
            entry: dict[str, object] = {"methods": list(methods)}

            if require_auth is not None:
                assert 0x02 in methods
                writer.write(b"\x05\x02")
                await writer.drain()
                aver_ulen = await reader.readexactly(2)
                assert aver_ulen[0] == 0x01
                u_bytes = await reader.readexactly(aver_ulen[1])
                plen = (await reader.readexactly(1))[0]
                p_bytes = await reader.readexactly(plen)
                entry["auth"] = (u_bytes.decode("utf-8"), p_bytes.decode("utf-8"))
                if entry["auth"] != require_auth:
                    writer.write(b"\x01\x01")
                    await writer.drain()
                    recorded.append(entry)
                    return
                writer.write(b"\x01\x00")
                await writer.drain()
            else:
                assert 0x00 in methods
                writer.write(b"\x05\x00")
                await writer.drain()

            req_hdr = await reader.readexactly(4)
            assert req_hdr[:3] == b"\x05\x01\x00"
            atyp = req_hdr[3]
            if atyp == 0x01:
                raw_addr = await reader.readexactly(4)
                host_str = str(ipaddress.IPv4Address(raw_addr))
            elif atyp == 0x03:
                dlen = (await reader.readexactly(1))[0]
                host_str = (await reader.readexactly(dlen)).decode("ascii")
            elif atyp == 0x04:
                raw_addr = await reader.readexactly(16)
                host_str = str(ipaddress.IPv6Address(raw_addr))
            else:
                raise AssertionError(f"unexpected ATYP {atyp}")
            port = struct.unpack("!H", await reader.readexactly(2))[0]
            entry["atyp"] = atyp
            entry["host"] = host_str
            entry["port"] = port

            writer.write(b"\x05\x00\x00\x01\x7f\x00\x00\x01\x00\x00")
            await writer.drain()

            if mode == "echo":
                payload = await reader.read(128)
                entry["payload"] = payload
                writer.write(b"SOCKS5:" + payload)
                await writer.drain()
            elif mode == "http":
                head = await reader.readuntil(b"\r\n\r\n")
                entry["http_head"] = head
                writer.write(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nPONG"
                )
                await writer.drain()
            elif mode == "hold":
                entry["held"] = True
                await reader.read(64)
            recorded.append(entry)
        except Exception:
            recorded.append({"error": True})
        finally:
            writer.close()
            await writer.wait_closed()

    srv = await asyncio.start_server(handle, "127.0.0.1", 0)
    port = srv.sockets[0].getsockname()[1]
    return srv, port, recorded


@pytest.mark.asyncio
async def test_local_proxy_relay_socks5_connect_auth_domain_ipv4_ipv6():
    srv, socks_port, recorded = await _start_mock_socks5_server(
        require_auth=("alice", "p@ss"),
        mode="echo",
    )
    relay = LocalProxyRelay(f"socks5://alice:p%40ss@127.0.0.1:{socks_port}")
    try:
        await relay.start()
        assert relay.local_port is not None

        cases = [
            (b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n", 0x03, "example.com", 443),
            (b"CONNECT 93.184.216.34:443 HTTP/1.1\r\nHost: 93.184.216.34:443\r\n\r\n", 0x01, "93.184.216.34", 443),
            (b"CONNECT [::1]:443 HTTP/1.1\r\nHost: [::1]:443\r\n\r\n", 0x04, "::1", 443),
        ]
        for idx, (req, exp_atyp, exp_host, exp_port) in enumerate(cases):
            reader, writer = await asyncio.open_connection("127.0.0.1", relay.local_port)
            writer.write(req)
            await writer.drain()
            resp = await reader.readuntil(b"\r\n\r\n")
            assert resp.startswith(b"HTTP/1.1 200")
            writer.write(b"ping")
            await writer.drain()
            assert await reader.read(64) == b"SOCKS5:ping"
            writer.close()
            await writer.wait_closed()
            await asyncio.sleep(0.02)
            assert recorded[idx]["auth"] == ("alice", "p@ss")
            assert recorded[idx]["atyp"] == exp_atyp
            assert recorded[idx]["host"] == exp_host
            assert recorded[idx]["port"] == exp_port
    finally:
        await relay.stop()
        srv.close()
        await srv.wait_closed()


@pytest.mark.asyncio
async def test_local_proxy_relay_socks5h_no_auth_remote_dns():
    srv, socks_port, recorded = await _start_mock_socks5_server(
        require_auth=None,
        mode="echo",
    )
    relay = LocalProxyRelay(f"socks5h://127.0.0.1:{socks_port}")
    try:
        await relay.start()
        assert relay.local_port is not None
        reader, writer = await asyncio.open_connection("127.0.0.1", relay.local_port)
        writer.write(b"CONNECT muse.ai:443 HTTP/1.1\r\nHost: muse.ai:443\r\n\r\n")
        await writer.drain()
        resp = await reader.readuntil(b"\r\n\r\n")
        assert resp.startswith(b"HTTP/1.1 200")
        writer.write(b"hello")
        await writer.drain()
        assert await reader.read(64) == b"SOCKS5:hello"
        writer.close()
        await writer.wait_closed()
        await asyncio.sleep(0.02)
        assert recorded[0]["methods"] == [0x00]
        assert recorded[0]["atyp"] == 0x03
        assert recorded[0]["host"] == "muse.ai"
        assert recorded[0]["port"] == 443
    finally:
        await relay.stop()
        srv.close()
        await srv.wait_closed()


@pytest.mark.asyncio
async def test_local_proxy_relay_socks5_plain_http_and_ipv6():
    srv, socks_port, recorded = await _start_mock_socks5_server(
        require_auth=("alice", "secret"),
        mode="http",
    )
    relay = LocalProxyRelay(f"socks5h://alice:secret@127.0.0.1:{socks_port}")
    try:
        await relay.start()
        assert relay.local_port is not None

        # Domain plain HTTP
        reader, writer = await asyncio.open_connection("127.0.0.1", relay.local_port)
        writer.write(
            b"GET http://example.com:8080/hello?x=1 HTTP/1.1\r\n"
            b"Proxy-Authorization: Basic Zm9v\r\n"
            b"Proxy-Connection: keep-alive\r\n"
            b"Connection: close\r\n\r\n"
        )
        await writer.drain()
        data = await reader.read()
        assert b"200 OK" in data and data.endswith(b"PONG")
        writer.close()
        await writer.wait_closed()

        # IPv6 plain HTTP
        reader2, writer2 = await asyncio.open_connection("127.0.0.1", relay.local_port)
        writer2.write(
            b"GET http://[::1]:8080/v6 HTTP/1.1\r\n"
            b"Connection: close\r\n\r\n"
        )
        await writer2.drain()
        data2 = await reader2.read()
        assert b"200 OK" in data2 and data2.endswith(b"PONG")
        writer2.close()
        await writer2.wait_closed()

        # Non-CONNECT https:// over SOCKS5 must be rejected with 502
        reader3, writer3 = await asyncio.open_connection("127.0.0.1", relay.local_port)
        writer3.write(b"GET https://example.com/secret HTTP/1.1\r\nHost: example.com\r\n\r\n")
        await writer3.drain()
        data3 = await reader3.read()
        assert b"502 Bad Gateway" in data3
        writer3.close()
        await writer3.wait_closed()

        await asyncio.sleep(0.02)
        assert recorded[0]["atyp"] == 0x03
        assert recorded[0]["host"] == "example.com"
        assert recorded[0]["port"] == 8080
        head0 = recorded[0]["http_head"]
        assert isinstance(head0, bytes)
        assert head0.startswith(b"GET /hello?x=1 HTTP/1.1\r\n")
        assert b"Host: example.com:8080\r\n" in head0
        assert b"Proxy-Authorization" not in head0
        assert b"Proxy-Connection" not in head0

        assert recorded[1]["atyp"] == 0x04
        assert recorded[1]["host"] == "::1"
        assert recorded[1]["port"] == 8080
        head1 = recorded[1]["http_head"]
        assert isinstance(head1, bytes)
        assert head1.startswith(b"GET /v6 HTTP/1.1\r\n")
        assert b"Host: [::1]:8080\r\n" in head1
    finally:
        await relay.stop()
        srv.close()
        await srv.wait_closed()


@pytest.mark.asyncio
async def test_local_proxy_relay_socks5_validation_and_partial_creds():
    with pytest.raises(ValueError, match="255"):
        LocalProxyRelay(f"socks5://{'u' * 256}:pass@127.0.0.1:1080")
    with pytest.raises(ValueError, match="255"):
        LocalProxyRelay(f"socks5://user:{'p' * 256}@127.0.0.1:1080")

    srv, socks_port, recorded = await _start_mock_socks5_server(
        require_auth=("onlyuser", ""),
        mode="echo",
    )
    relay = LocalProxyRelay(f"socks5://onlyuser@127.0.0.1:{socks_port}")
    try:
        await relay.start()
        assert relay.local_port is not None

        # Valid partial credentials (ULEN=8, PLEN=0)
        reader, writer = await asyncio.open_connection("127.0.0.1", relay.local_port)
        writer.write(b"CONNECT muse.ai:443 HTTP/1.1\r\nHost: muse.ai:443\r\n\r\n")
        await writer.drain()
        resp = await reader.readuntil(b"\r\n\r\n")
        assert resp.startswith(b"HTTP/1.1 200")
        writer.write(b"ok")
        await writer.drain()
        assert await reader.read(64) == b"SOCKS5:ok"
        writer.close()
        await writer.wait_closed()

        # Target domain > 255 bytes -> 502 Bad Gateway
        long_domain = ("a" * 60 + ".") * 5 + "com"
        reader2, writer2 = await asyncio.open_connection("127.0.0.1", relay.local_port)
        writer2.write(f"CONNECT {long_domain}:443 HTTP/1.1\r\nHost: {long_domain}:443\r\n\r\n".encode())
        await writer2.drain()
        resp2 = await reader2.read()
        assert b"502 Bad Gateway" in resp2
        writer2.close()
        await writer2.wait_closed()

        await asyncio.sleep(0.02)
        assert recorded[0]["auth"] == ("onlyuser", "")
    finally:
        await relay.stop()
        srv.close()
        await srv.wait_closed()


@pytest.mark.asyncio
async def test_local_proxy_relay_http_keepalive_injects_auth_on_every_request():
    expected_auth = (
        b"Proxy-Authorization: Basic "
        + base64.b64encode(b"bob:secret123").decode("ascii").encode("ascii")
    )
    seen_heads: list[bytes] = []
    seen_bodies: list[bytes] = []

    async def fake_upstream(
        reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        try:
            # Request 1: GET
            head1 = await reader.readuntil(b"\r\n\r\n")
            seen_heads.append(head1)
            writer.write(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nONE")
            await writer.drain()

            # Request 2: chunked POST on same keep-alive socket
            head2 = await reader.readuntil(b"\r\n\r\n")
            seen_heads.append(head2)
            body2 = bytearray()
            while True:
                sz_line = await reader.readuntil(b"\r\n")
                sz = int(sz_line[:-2], 16)
                if sz == 0:
                    await reader.readuntil(b"\r\n")
                    break
                body2.extend(await reader.readexactly(sz))
                await reader.readexactly(2)
            seen_bodies.append(bytes(body2))
            writer.write(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
                b"3\r\nTWO\r\n0\r\n\r\n"
            )
            await writer.drain()
        finally:
            writer.close()
            await writer.wait_closed()

    upstream_srv = await asyncio.start_server(fake_upstream, "127.0.0.1", 0)
    upstream_port = upstream_srv.sockets[0].getsockname()[1]

    relay = LocalProxyRelay(f"http://bob:secret123@127.0.0.1:{upstream_port}")
    try:
        await relay.start()
        assert relay.local_port is not None
        reader, writer = await asyncio.open_connection("127.0.0.1", relay.local_port)

        writer.write(b"GET http://example.com/1 HTTP/1.1\r\nHost: example.com\r\n\r\n")
        await writer.drain()
        r1_head = await reader.readuntil(b"\r\n\r\n")
        r1_body = await reader.readexactly(3)
        assert r1_head.startswith(b"HTTP/1.1 200")
        assert r1_body == b"ONE"

        writer.write(
            b"POST http://example.com/2 HTTP/1.1\r\n"
            b"Host: example.com\r\n"
            b"Transfer-Encoding: chunked\r\n"
            b"Connection: close\r\n\r\n"
            b"5\r\nhello\r\n0\r\n\r\n"
        )
        await writer.drain()
        rest = await reader.read()
        assert b"HTTP/1.1 200 OK" in rest
        assert b"TWO" in rest

        writer.close()
        await writer.wait_closed()
        assert len(seen_heads) == 2
        assert expected_auth in seen_heads[0]
        assert expected_auth in seen_heads[1]
        assert seen_bodies == [b"hello"]
    finally:
        await relay.stop()
        upstream_srv.close()
        await upstream_srv.wait_closed()


@pytest.mark.asyncio
async def test_local_proxy_relay_smuggling_and_leading_crlf():
    seen: list[bytes] = []

    async def fake_upstream(
        reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        head = await reader.readuntil(b"\r\n\r\n")
        seen.append(head)
        writer.write(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK")
        await writer.drain()
        writer.close()
        await writer.wait_closed()

    upstream_srv = await asyncio.start_server(fake_upstream, "127.0.0.1", 0)
    upstream_port = upstream_srv.sockets[0].getsockname()[1]

    relay = LocalProxyRelay(f"http://bob:secret@127.0.0.1:{upstream_port}")
    try:
        await relay.start()
        assert relay.local_port is not None

        # Leading CRLF before request must succeed (not return b"\r\n" early)
        r1, w1 = await asyncio.open_connection("127.0.0.1", relay.local_port)
        w1.write(
            b"\r\nGET http://example.com/ HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n"
        )
        await w1.drain()
        d1 = await r1.read()
        assert b"200 OK" in d1 and d1.endswith(b"OK")
        w1.close()
        await w1.wait_closed()

        # Conflicting Content-Length must be rejected with 400
        r2, w2 = await asyncio.open_connection("127.0.0.1", relay.local_port)
        w2.write(
            b"POST http://example.com/ HTTP/1.1\r\nHost: example.com\r\n"
            b"Content-Length: 5\r\nContent-Length: 10\r\n\r\n"
        )
        await w2.drain()
        d2 = await r2.read()
        assert d2.startswith(b"HTTP/1.1 400")
        w2.close()
        await w2.wait_closed()

        # Content-Length + Transfer-Encoding smuggling must be rejected with 400
        r3, w3 = await asyncio.open_connection("127.0.0.1", relay.local_port)
        w3.write(
            b"POST http://example.com/ HTTP/1.1\r\nHost: example.com\r\n"
            b"Content-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n"
        )
        await w3.drain()
        d3 = await r3.read()
        assert d3.startswith(b"HTTP/1.1 400")
        w3.close()
        await w3.wait_closed()

        assert len(seen) == 1
    finally:
        await relay.stop()
        upstream_srv.close()
        await upstream_srv.wait_closed()


@pytest.mark.asyncio
async def test_local_proxy_relay_idle_timeout_and_stop_no_deadlock():
    srv, socks_port, _ = await _start_mock_socks5_server(require_auth=None, mode="hold")

    # 1. Idle timeout closes active tunnel automatically
    relay_idle = LocalProxyRelay(
        f"socks5h://127.0.0.1:{socks_port}",
        idle_timeout=0.15,
        close_timeout=0.5,
    )
    try:
        await relay_idle.start()
        assert relay_idle.local_port is not None
        r1, w1 = await asyncio.open_connection("127.0.0.1", relay_idle.local_port)
        w1.write(b"CONNECT muse.ai:443 HTTP/1.1\r\nHost: muse.ai:443\r\n\r\n")
        await w1.drain()
        resp = await r1.readuntil(b"\r\n\r\n")
        assert resp.startswith(b"HTTP/1.1 200")
        eof_data = await asyncio.wait_for(r1.read(), timeout=1.0)
        assert eof_data == b""
        w1.close()
        await w1.wait_closed()
    finally:
        await relay_idle.stop()

    # 2. stop() cancels active tunnel tasks BEFORE wait_closed() (no deadlock)
    relay_stop = LocalProxyRelay(
        f"socks5h://127.0.0.1:{socks_port}",
        idle_timeout=30.0,
        close_timeout=0.5,
    )
    await relay_stop.start()
    assert relay_stop.local_port is not None
    r2, w2 = await asyncio.open_connection("127.0.0.1", relay_stop.local_port)
    w2.write(b"CONNECT muse.ai:443 HTTP/1.1\r\nHost: muse.ai:443\r\n\r\n")
    await w2.drain()
    resp2 = await r2.readuntil(b"\r\n\r\n")
    assert resp2.startswith(b"HTTP/1.1 200")

    await asyncio.wait_for(relay_stop.stop(), timeout=1.0)
    w2.close()
    await w2.wait_closed()
    srv.close()
    await srv.wait_closed()
