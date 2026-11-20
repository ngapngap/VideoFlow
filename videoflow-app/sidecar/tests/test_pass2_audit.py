"""Regression tests for Pass-2 adversarial audit findings."""

from __future__ import annotations

import asyncio
import contextlib
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import httpx
import pytest
from httpx import ASGITransport, AsyncClient
from pydantic import ValidationError

from muse2api.accounts.keepalive import renew_account
from muse2api.accounts.model import Account, AccountStatus, mask_proxy_url
from muse2api.api.schemas import AccountCreate
from muse2api.app import create_app
from muse2api.config import Settings
from muse2api.core import media as media_mod
from muse2api.core.prompt import flatten_messages
from muse2api.drivers.base import SessionInfo
from muse2api.drivers.browser.driver import BrowserDriver, _Tab, verify_and_save_download
from muse2api.drivers.browser.interactive import InteractiveBrowserManager
from muse2api.drivers.browser.proxy_relay import (
    LocalProxyRelay,
    _HttpValidationError,
    _parse_http_headers,
    _pipe_pair,
    mask_proxy,
)
from muse2api.errors import InvalidRequest, NotFound, UpstreamError


@pytest.mark.asyncio
async def test_ffmpeg_timeout_raises_upstream_error_not_fallback():
    mp4_bytes = b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom"

    class HangingProc:
        returncode = None

        async def communicate(self):
            raise asyncio.TimeoutError("hang")

        def kill(self):
            self.returncode = -9

        async def wait(self):
            self.returncode = -9
            return -9

    proc = HangingProc()
    with (
        patch(
            "muse2api.drivers.browser.driver.asyncio.create_subprocess_exec",
            new=AsyncMock(return_value=proc),
        ),
        pytest.raises(UpstreamError, match="timed out"),
    ):
        await verify_and_save_download(mp4_bytes, "video")


@pytest.mark.asyncio
async def test_recover_browser_from_warming_task_and_closed_during_startup(tmp_path: Path):
    acc = Account(id="acc_rec", label="rec", cookies={"hatch_sess": "s"})
    settings = Settings(data_dir=tmp_path / "data", driver="mock", headless=True)
    drv = BrowserDriver(settings, account=acc, port=19501, profile_dir=tmp_path / "p1")

    tab = _Tab(account_id=acc.id, context_id="c1", target_id="t1", session=MagicMock(closed=False))
    drv._tabs[acc.id] = [tab]

    calls = {"startup": 0, "shutdown": 0}

    async def fake_startup():
        calls["startup"] += 1
        drv._closed = True  # closed concurrently during startup

    async def fake_shutdown():
        calls["shutdown"] += 1

    drv.startup = fake_startup  # type: ignore[method-assign]
    drv.shutdown = fake_shutdown  # type: ignore[method-assign]

    async def run_from_warming():
        await drv._recover_browser()
        return "survived"

    with patch("muse2api.drivers.browser.driver._kill_stale_browsers", return_value=0):
        t = asyncio.create_task(run_from_warming())
        drv._warming[acc.id] = t
        res = await t

    assert res == "survived"
    assert calls["startup"] == 1
    assert calls["shutdown"] == 1
    assert tab.closed is True
    assert drv._tabs == {}


@pytest.mark.asyncio
async def test_fill_warm_releases_checked_out_tab_on_failure(tmp_path: Path):
    acc = Account(
        id="acc_warm",
        label="warm",
        cookies={"hatch_sess": "s", "hatch_native_auth_device": "d"},
    )
    settings = Settings(data_dir=tmp_path / "data", driver="mock", warm_threads=2)
    drv = BrowserDriver(settings, account=acc, port=19502, profile_dir=tmp_path / "p2")

    tab1 = _Tab(account_id=acc.id, context_id="c1", target_id="t1", session=MagicMock(closed=False), busy=True)
    drv._tabs[acc.id] = [tab1]

    seq = {"n": 0}

    async def fake_checkout(account: Account, pending: int = 0):
        seq["n"] += 1
        if seq["n"] == 1:
            return tab1
        raise RuntimeError("checkout boom")

    drv._checkout_for_warming = fake_checkout  # type: ignore[method-assign]
    # Non-cancel errors are logged and swallowed; the already checked-out tab must be released.
    await drv._fill_warm(acc)
    assert seq["n"] == 2
    assert tab1.busy is False

    # CancelledError must still propagate after releasing the batch.
    tab1.busy = True
    seq["n"] = 0

    async def cancel_checkout(account: Account, pending: int = 0):
        seq["n"] += 1
        if seq["n"] == 1:
            return tab1
        raise asyncio.CancelledError()

    drv._checkout_for_warming = cancel_checkout  # type: ignore[method-assign]
    with pytest.raises(asyncio.CancelledError):
        await drv._fill_warm(acc)
    assert seq["n"] == 2
    assert tab1.busy is False


def test_is_account_busy_checks_opening_quota_and_video_locks(tmp_path: Path):
    acc = Account(id="acc_busy", label="busy", cookies={"hatch_sess": "s"})
    settings = Settings(data_dir=tmp_path / "data", driver="mock")
    drv = BrowserDriver(settings, account=acc, port=19503, profile_dir=tmp_path / "p3")

    assert drv.is_account_busy(acc.id) is False
    drv._opening[acc.id] = 1
    assert drv.is_account_busy(acc.id) is True
    drv._opening[acc.id] = 0
    assert drv.is_account_busy(acc.id) is False

    drv._quota_locks[acc.id] = asyncio.Lock()
    asyncio.run(_assert_lock_makes_busy(drv, drv._quota_locks[acc.id], acc.id))

    drv._video_locks[acc.id] = asyncio.Lock()
    asyncio.run(_assert_lock_makes_busy(drv, drv._video_locks[acc.id], acc.id))


async def _assert_lock_makes_busy(drv: BrowserDriver, lock: asyncio.Lock, acc_id: str) -> None:
    async with lock:
        assert drv.is_account_busy(acc_id) is True
    assert drv.is_account_busy(acc_id) is False


@pytest.mark.asyncio
async def test_keepalive_defers_tab_sync_until_commit(tmp_path: Path):
    acc = Account(id="acc_ka", label="ka", cookies={"hatch_sess": "s1"})
    drv = BrowserDriver(Settings(data_dir=tmp_path / "data", driver="mock"))
    ensure_spy = AsyncMock()
    drv._ensure_account_driver = ensure_spy  # type: ignore[method-assign]
    drv.sync_renewed_cookies = AsyncMock()  # type: ignore[method-assign]

    class FakePool:
        def __init__(self, reject_commit: bool):
            self.reject = reject_commit
            self.is_blocked_cb = None
            self.acc = acc

        async def snapshot(self, aid: str):
            return self.acc.model_copy(deep=True) if aid == self.acc.id else None

        async def try_update(self, aid: str, fn):
            if self.reject:
                # Simulate concurrent cookie replacement before fn checks orig_cookies
                stale = self.acc.model_copy(update={"cookies": {"hatch_sess": "replaced"}})
                return True, fn(stale)[1]
            _, res = fn(self.acc)
            return True, res

    with patch("muse2api.drivers.browser.driver.muse.renew_session",
               new=AsyncMock(return_value=SessionInfo(ok=True, cookies={"hatch_sess": "s2"}))):
        res_skip = await renew_account(FakePool(reject_commit=True), drv, acc.id)  # type: ignore[arg-type]
        assert ensure_spy.await_count == 0
        assert res_skip["ok"] is False
        assert drv.sync_renewed_cookies.await_count == 0

        res_ok = await renew_account(FakePool(reject_commit=False), drv, acc.id)  # type: ignore[arg-type]
        assert ensure_spy.await_count == 0
        assert res_ok["ok"] is True
        assert drv.sync_renewed_cookies.await_count == 1


@pytest.mark.asyncio
async def test_proxy_relay_socks5_auth_failure_and_smuggling_and_masking():
    # 1. SOCKS5 auth rejection produces HTTP 502 Bad Gateway instead of silent drop
    async def handle_socks5(reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
        try:
            await reader.readexactly(4)  # VER NMETHODS METHODS
            writer.write(b"\x05\x02")  # user/pass auth
            await writer.drain()
            await reader.readexactly(3)
            hlen = (await reader.read(256))
            _ = hlen
            writer.write(b"\x01\x01")  # auth rejected
            await writer.drain()
        except Exception:
            pass
        finally:
            writer.close()

    srv = await asyncio.start_server(handle_socks5, "127.0.0.1", 0)
    s5_port = srv.sockets[0].getsockname()[1]
    relay = LocalProxyRelay(f"socks5h://user:badpw@127.0.0.1:{s5_port}")
    await relay.start()
    try:
        r, w = await asyncio.open_connection("127.0.0.1", relay.local_port)
        w.write(b"GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n")
        await w.drain()
        resp = await asyncio.wait_for(r.read(4096), timeout=3.0)
        assert resp.startswith(b"HTTP/1.1 502 Bad Gateway")
        w.close()
    finally:
        await relay.stop()
        srv.close()
        await srv.wait_closed()

    # 2. Framing validation: header whitespace & non-chunked request TE
    with pytest.raises(_HttpValidationError):
        _parse_http_headers(b"GET / HTTP/1.1\r\nHost: a\r\nX-Test : 1\r\n\r\n", is_request=True)
    with pytest.raises(_HttpValidationError):
        _parse_http_headers(
            b"POST / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: gzip\r\n\r\n", is_request=True
        )
    parsed_resp = _parse_http_headers(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\n\r\n", is_request=False
    )
    assert parsed_resp.has_transfer_encoding is True
    assert parsed_resp.is_chunked is False

    # 3. Masking, port 0 & SOCKS5 255B auth schema validation
    assert mask_proxy("http://1.2.3.4/path@secret") == "***"
    assert mask_proxy_url("http://u:p@1.2.3.4:8080/") == "http://***@1.2.3.4:8080"
    assert mask_proxy_url("http://1.2.3.4/p?x=@secret") == "***"
    with pytest.raises(ValueError):
        LocalProxyRelay("http://127.0.0.1:0")
    with pytest.raises(ValidationError):
        AccountCreate(label="x", proxy=f"socks5://{'u' * 256}:pw@127.0.0.1:1080")


@pytest.mark.asyncio
async def test_pipe_pair_keeps_live_bidirectional_tunnel_open():
    r_a = asyncio.StreamReader()
    r_b = asyncio.StreamReader()

    class MemWriter:
        def __init__(self, target_reader: asyncio.StreamReader):
            self.target = target_reader
            self.closed = False

        def write(self, data: bytes):
            self.target.feed_data(data)

        async def drain(self):
            return

        def can_write_eof(self):
            return False

        def close(self):
            self.closed = True

    sink_a = asyncio.StreamReader()
    sink_b = asyncio.StreamReader()
    w_a = MemWriter(sink_a)
    w_b = MemWriter(sink_b)

    async def pump_a():
        for i in range(4):
            await asyncio.sleep(0.1)
            r_a.feed_data(f"{i}".encode())
        await asyncio.sleep(0.05)
        r_a.feed_eof()

    pumper = asyncio.create_task(pump_a())
    await _pipe_pair(r_a, w_b, r_b, w_a, idle_timeout=0.22)  # type: ignore[arg-type]
    await pumper
    out = await asyncio.wait_for(sink_b.readexactly(4), timeout=1.0)
    assert out == b"0123"


@pytest.mark.asyncio
async def test_media_gzip_dualstack_and_image_route_limits(tmp_path: Path):
    # 1. Dual-stack connect fallback & GZip rejection
    class FakeResp:
        def __init__(self, headers: dict[str, str], body: bytes):
            self.status_code = 200
            self.headers = httpx.Headers(headers)
            self._body = body

        def raise_for_status(self):
            return

        async def aiter_bytes(self, _n: int):
            yield self._body

    calls: list[str] = []

    class FakeClient:
        def __init__(self, **_kwargs):
            pass

        async def __aenter__(self):
            return self

        async def __aexit__(self, *_args):
            return False

        def stream(self, _method: str, url: str, **_kwargs):
            calls.append(url)
            if "2001:db8::1" in url:
                raise httpx.ConnectError("v6 unreachable")
            return contextlib.asynccontextmanager(
                lambda: _async_yield(FakeResp({"content-type": "image/png"}, b"PNGDATA"))
            )()

    async def _async_yield(val):
        yield val

    with (
        patch.object(media_mod, "_resolve_public_ips", new=AsyncMock(return_value=["2001:db8::1", "198.51.100.2"])),
        patch.object(media_mod.httpx, "AsyncClient", FakeClient),
    ):
        data, mime = await media_mod._fetch_public_image("https://example.com/a.png", timeout=5.0)
        assert data == b"PNGDATA"
        assert mime == "image/png"
        assert len(calls) == 2

    class GzipClient(FakeClient):
        def stream(self, _method: str, url: str, **_kwargs):
            return contextlib.asynccontextmanager(
                lambda: _async_yield(FakeResp({"content-encoding": "gzip", "content-type": "image/png"}, b"gz"))
            )()

    with (
        patch.object(media_mod, "_resolve_public_ips", new=AsyncMock(return_value=["198.51.100.2"])),
        patch.object(media_mod.httpx, "AsyncClient", GzipClient),
        pytest.raises(InvalidRequest, match="compressed"),
    ):
        await media_mod._fetch_public_image("https://example.com/gz.png", timeout=5.0)

    # 2. Prompt non-string image_url rejection
    with pytest.raises(InvalidRequest, match="string"):
        flatten_messages([{"role": "user", "content": [{"type": "image_url", "image_url": {"url": 123}}]}])

    # 3. Chat & Responses >4 images + /v1/images/edits empty file rejection
    settings = Settings(data_dir=tmp_path / "data", api_key="k", driver="mock")
    app = create_app(settings)
    b64_png = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="
    headers = {"Authorization": "Bearer k"}
    async with app.router.lifespan_context(app):
        transport = ASGITransport(app=app)
        async with AsyncClient(transport=transport, base_url="http://test") as client:
            r_chat = await client.post(
                "/v1/chat/completions",
                headers=headers,
                json={
                    "model": "gpt-4o",
                    "messages": [{
                        "role": "user",
                        "content": [{"type": "image_url", "image_url": {"url": b64_png}} for _ in range(5)],
                    }],
                },
            )
            assert r_chat.status_code == 400

            r_resp = await client.post(
                "/v1/responses",
                headers=headers,
                json={
                    "model": "gpt-4o",
                    "input": [{
                        "role": "user",
                        "content": [{"type": "input_image", "image_url": b64_png} for _ in range(5)],
                    }],
                },
            )
            assert r_resp.status_code == 400

            r_empty = await client.post(
                "/v1/images/edits",
                headers=headers,
                data={"prompt": "test"},
                files={"image": ("empty.png", b"", "image/png")},
            )
            assert r_empty.status_code == 400


@pytest.mark.asyncio
async def test_interactive_open_session_raises_not_found_when_deleted(tmp_path: Path):
    acc = Account(id="acc_missing", label="gone", cookies={"hatch_sess": "s"})

    class EmptyPool:
        _cond = asyncio.Condition()

        def get(self, _aid: str):
            return None

    drv_mock = MagicMock()
    drv_mock.is_account_busy.return_value = False
    mgr = InteractiveBrowserManager(
        Settings(data_dir=tmp_path / "data", driver="mock"),
        EmptyPool(),  # type: ignore[arg-type]
        drv_mock,
    )
    with pytest.raises(NotFound):
        await mgr.open_login(acc)


@pytest.mark.asyncio
async def test_defer_prewarm_admin_prewarm_quota_exhausted_and_cookie_status(tmp_path: Path):
    settings = Settings(
        data_dir=tmp_path / "data",
        api_key="k",
        admin_key="k",
        driver="mock",
        defer_prewarm=True,
    )
    app = create_app(settings)
    svc = app.state.services
    acc = Account(
        id="acc_pw",
        label="PW",
        cookies={"hatch_sess": "s1", "hatch_native_auth_device": "d1"},
        status=AccountStatus.ACTIVE,
        meta={"quota": {"found": True, "weekly_used_pct": 100, "extra_left": "0 tokens", "resets_at": "Dec 31, 2099"}},
    )
    await svc.pool.store.save([acc])
    svc.driver.prewarm = MagicMock()

    headers = {"Authorization": "Bearer k"}
    async with app.router.lifespan_context(app):
        assert svc.driver.prewarm.call_count == 0
        transport = ASGITransport(app=app)
        async with AsyncClient(transport=transport, base_url="http://test") as client:
            r_list = await client.get("/admin/accounts", headers=headers)
            assert r_list.status_code == 200
            pub = r_list.json()["data"][0]
            assert pub["quota_exhausted"] is True

            r_clear = await client.patch(
                f"/admin/accounts/{acc.id}",
                headers=headers,
                json={"cookies": {"hatch_sess": "", "hatch_native_auth_device": ""}},
            )
            assert r_clear.status_code == 200
            assert r_clear.json()["account"]["status"] == "invalid"

            # Restore complete cookies & reset quota so POST /admin/prewarm picks it up
            svc.pool.get(acc.id).meta["quota"] = {"found": True, "weekly_used_pct": 10}
            r_restore = await client.patch(
                f"/admin/accounts/{acc.id}",
                headers=headers,
                json={"cookies": {"hatch_sess": "s2", "hatch_native_auth_device": "d2"}},
            )
            assert r_restore.status_code == 200
            assert r_restore.json()["account"]["status"] == "active"

            r_pw = await client.post("/admin/prewarm", headers=headers)
            assert r_pw.status_code == 200
            assert r_pw.json()["prewarmed"] == 1
            assert svc.driver.prewarm.call_count == 1
