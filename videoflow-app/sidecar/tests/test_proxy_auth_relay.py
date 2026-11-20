from __future__ import annotations

import asyncio
import base64
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
    assert needs_proxy_relay("socks5://user:pass@10.0.0.1:1080") is False
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
