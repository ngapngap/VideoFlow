"""Regression and coverage tests for Round 1 Sidecar audit fixes (#5.1 - #5.10)."""

from __future__ import annotations

import asyncio
import contextlib
import json
import time
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock, MagicMock, patch

import httpx
import pytest
from httpx import ASGITransport, AsyncClient
from starlette.datastructures import FormData, Headers, UploadFile
from websockets.exceptions import ConnectionClosedOK
from websockets.frames import Close

from muse2api.accounts.model import Account, AccountStatus
from muse2api.accounts.pool import AccountPool
from muse2api.api.deps import require_admin_key
from muse2api.api.routes.images import edit_images
from muse2api.app import create_app
from muse2api.auth.keys import KeyStore
from muse2api.config import Settings
from muse2api.core import media as media_mod
from muse2api.core.media import load_image_ref
from muse2api.drivers.base import ChatRequest, MediaResult, MuseDriver
from muse2api.drivers.browser.cdp import CDPError, CDPSession
from muse2api.drivers.browser.chromium import ChromiumProcess
from muse2api.drivers.browser.driver import (
    _USED_PORTS,
    BrowserDriver,
    _Tab,
)
from muse2api.drivers.browser.proxy_relay import LocalProxyRelay, _pipe_pair
from muse2api.errors import (
    InvalidRequest,
    UpstreamAuthError,
    UpstreamError,
    UpstreamQuotaError,
    UpstreamTimeout,
    UpstreamVideoLocked,
)

_VALID_COOKIES = {
    "hatch_sess": "sess_1",
    "hatch_native_auth_device": "dev_1",
}


class _MemStore:
    def __init__(self, accounts: list[Account] | None = None) -> None:
        self._items = list(accounts or [])

    async def load(self) -> list[Account]:
        return list(self._items)

    async def save(self, accounts: list[Account]) -> None:
        self._items = [a.model_copy(deep=True) for a in accounts]


# ── #5.1: release_account_driver vs _fill_warm prewarm race ──────────────────


async def test_warming_tabs_not_busy_and_release_cancels_warming_on_delete_and_login(
    tmp_path: Path,
) -> None:
    settings = Settings(
        data_dir=tmp_path / "data",
        media_dir=tmp_path / "media",
        driver="browser",
        warm_threads=2,
        account_max_concurrency=2,
        defer_prewarm=True,
    )
    app = create_app(settings)
    app.dependency_overrides[require_admin_key] = lambda: None
    svc = app.state.services
    root_drv = svc.driver
    assert isinstance(root_drv, BrowserDriver)
    pool = svc.pool
    interactive = svc.interactive
    assert interactive is not None

    acc1 = Account(id="acc_w1", label="W1", cookies=dict(_VALID_COOKIES))
    acc2 = Account(id="acc_w2", label="W2", cookies=dict(_VALID_COOKIES))
    await pool.upsert(acc1)
    await pool.upsert(acc2)

    # 1. Warming openings and warming tabs do not trigger is_account_busy; real tabs do
    sub1 = await root_drv._ensure_account_driver(acc1)
    mock_browser1 = MagicMock(closed=False)
    mock_browser1.close = AsyncMock()
    mock_browser1.send = AsyncMock(return_value={})
    mock_chromium1 = MagicMock()
    mock_chromium1.stop = AsyncMock()
    sub1._browser = mock_browser1
    sub1._chromium = mock_chromium1

    sub1._opening[acc1.id] = 1
    sub1._opening_warming[acc1.id] = 1
    assert root_drv.is_account_busy(acc1.id) is False
    sub1._opening[acc1.id] = 0
    sub1._opening_warming[acc1.id] = 0

    warming_entered = asyncio.Event()
    release_warm = asyncio.Event()
    warm_tab = _Tab(
        account_id=acc1.id,
        context_id="ctx_1",
        target_id="tgt_1",
        session=MagicMock(closed=False, close=AsyncMock()),
        busy=True,
        warming=True,
    )
    sub1._tabs[acc1.id] = [warm_tab]

    async def fake_checkout_warming(_account: Account, pending: int = 0):
        return warm_tab if pending == 0 else None

    async def fake_warm_up(_tab: _Tab) -> None:
        warming_entered.set()
        await release_warm.wait()

    sub1._checkout_for_warming = fake_checkout_warming  # type: ignore[method-assign]
    sub1._warm_up = fake_warm_up  # type: ignore[method-assign]
    sub1._warming[acc1.id] = asyncio.create_task(sub1._fill_warm(acc1))
    await asyncio.wait_for(warming_entered.wait(), timeout=2.0)

    # While _fill_warm holds a busy warming tab, account is NOT busy for admin/interactive
    assert warm_tab.busy is True
    assert warm_tab.warming is True
    assert root_drv.is_account_busy(acc1.id) is False
    # Quota checkout also ignores warming tab and reserves a tab on the free slot
    idle_q_tab = _Tab(acc1.id, "ctx_1", "tgt_q", MagicMock(closed=False, close=AsyncMock()))
    with patch.object(sub1, "_open_tab", AsyncMock(return_value=idle_q_tab)):
        q_tab = await sub1._checkout_idle(acc1)
        assert q_tab is idle_q_tab
        await sub1._checkin(q_tab)
        await sub1._close_tab(q_tab)

    # 2. DELETE /admin/accounts/{id} succeeds while _fill_warm holds warming tab & shuts down sub1
    async with AsyncClient(transport=ASGITransport(app=app), base_url="http://test") as client:
        r_del = await client.delete(f"/admin/accounts/{acc1.id}")
        assert r_del.status_code == 200

    assert pool.get(acc1.id) is None
    assert acc1.id not in root_drv._account_drivers
    assert sub1._closed is True
    assert sub1._warming == {}
    mock_chromium1.stop.assert_awaited_once()
    assert warm_tab.busy is False

    # 3. Real request tab still marks account busy and blocks release_account_driver
    sub2 = await root_drv._ensure_account_driver(acc2)
    mock_chromium2 = MagicMock()
    mock_chromium2.stop = AsyncMock()
    sub2._chromium = mock_chromium2
    sub2._browser = MagicMock(closed=False, close=AsyncMock(), send=AsyncMock(return_value={}))
    real_tab = _Tab(
        account_id=acc2.id,
        context_id="ctx_2",
        target_id="tgt_2",
        session=MagicMock(closed=False, close=AsyncMock()),
        busy=True,
        warming=False,
    )
    sub2._tabs[acc2.id] = [real_tab]
    assert root_drv.is_account_busy(acc2.id) is True

    await root_drv.release_account_driver(acc2.id)
    assert acc2.id in root_drv._account_drivers
    mock_chromium2.stop.assert_not_awaited()

    # Once real tab checks in, open_login while warming is active releases driver cleanly
    await sub2._checkin(real_tab)
    warming_entered2 = asyncio.Event()
    warm_tab2 = _Tab(
        account_id=acc2.id,
        context_id="ctx_2",
        target_id="tgt_w2",
        session=MagicMock(closed=False, close=AsyncMock()),
        busy=True,
        warming=True,
    )
    sub2._tabs[acc2.id] = [warm_tab2]

    async def fake_checkout_warming2(_account: Account, pending: int = 0):
        return warm_tab2 if pending == 0 else None

    async def fake_warm_up2(_tab: _Tab) -> None:
        warming_entered2.set()
        await asyncio.sleep(60)

    sub2._checkout_for_warming = fake_checkout_warming2  # type: ignore[method-assign]
    sub2._warm_up = fake_warm_up2  # type: ignore[method-assign]
    sub2._warming[acc2.id] = asyncio.create_task(sub2._fill_warm(acc2))
    await asyncio.wait_for(warming_entered2.wait(), timeout=2.0)

    # open_login must succeed (not 409) and _run_login_session cancels warming + shuts down sub2
    async def fake_login_session(account: Account, _sess) -> None:
        await interactive.driver.release_account_driver(account.id)

    with patch.object(interactive, "_run_login_session", side_effect=fake_login_session):
        res_login = await interactive.open_login(acc2)
        assert res_login["ok"] is True
        await asyncio.sleep(0.05)

    assert acc2.id not in root_drv._account_drivers
    assert sub2._closed is True
    mock_chromium2.stop.assert_awaited_once()
    await root_drv.shutdown()
    await svc.requests.close()


async def test_fill_warm_exits_when_account_removed_from_pool(tmp_path: Path) -> None:
    settings = Settings(data_dir=tmp_path / "data", driver="browser", warm_threads=1)
    pool = AccountPool(_MemStore(), max_concurrency=2)
    acc = Account(id="acc_rm", label="RM", cookies=dict(_VALID_COOKIES))
    await pool.upsert(acc)

    drv = BrowserDriver(settings, account=acc)
    drv._browser = MagicMock(closed=False)
    drv.set_account_guards(lookup=pool.get)

    tab = _Tab(acc.id, "ctx", "tgt", MagicMock(closed=False))
    checkout_calls = 0

    async def fake_checkout(_account: Account, pending: int = 0):
        nonlocal checkout_calls
        checkout_calls += 1
        return tab if pending == 0 else None

    async def fake_warm_up(_tab: _Tab) -> None:
        # Simulate account deleted while warming round runs
        pool._accounts.pop(acc.id, None)
        raise UpstreamTimeout("transient")

    sleep_spy = AsyncMock()
    with (
        patch.object(drv, "_checkout_for_warming", side_effect=fake_checkout),
        patch.object(drv, "_warm_up", side_effect=fake_warm_up),
        patch.object(drv, "_checkin", AsyncMock()),
        patch("muse2api.drivers.browser.driver.asyncio.sleep", sleep_spy),
    ):
        await drv._fill_warm(acc)

    assert checkout_calls == 2
    sleep_spy.assert_not_awaited()


# ── #5.2: AccountPool._apply_result status & cooldown preservation ────────────


async def test_pool_apply_result_preserves_terminal_status_and_max_cooldown() -> None:
    # 1. INVALID account is never resurrected by retryable/quota errors or trailing None
    acc_inv = Account(id="acc_inv", cookies=dict(_VALID_COOKIES))
    pool = AccountPool(_MemStore([acc_inv]), max_concurrency=4, cooldown=120.0)
    await pool.load()

    r1 = await pool.acquire()
    r2 = await pool.acquire()
    await pool.release(r1, UpstreamAuthError("session expired"))
    await pool.release(r2, UpstreamError("browser error: cdp closed"))
    assert pool.get("acc_inv").status == AccountStatus.INVALID
    assert "session expired" in (pool.get("acc_inv").last_error or "")

    await pool.release(acc_inv, UpstreamQuotaError("quota"))
    await pool.release(acc_inv, UpstreamTimeout("timeout"))
    await pool.release(acc_inv, None)
    assert pool.get("acc_inv").status == AccountStatus.INVALID
    assert "session expired" in (pool.get("acc_inv").last_error or "")

    # 2. VIDEO_LOCKED account is never overwritten to COOLING and keeps last_error on None
    acc_vl = Account(id="acc_vl", cookies=dict(_VALID_COOKIES))
    pool_vl = AccountPool(_MemStore([acc_vl]), max_concurrency=4, cooldown=120.0)
    await pool_vl.load()

    v1 = await pool_vl.acquire()
    v2 = await pool_vl.acquire()
    await pool_vl.release(v1, UpstreamVideoLocked("video locked"))
    await pool_vl.release(v2, UpstreamError("transient cdp"))
    await pool_vl.release(acc_vl, UpstreamQuotaError("quota"))
    await pool_vl.release(acc_vl, None)
    assert pool_vl.get("acc_vl").status == AccountStatus.VIDEO_LOCKED
    assert "video locked" in (pool_vl.get("acc_vl").last_error or "")

    # 3. Shorter retryable cooldown never truncates longer quota cooldown
    acc_q = Account(id="acc_q", cookies=dict(_VALID_COOKIES))
    pool_q = AccountPool(_MemStore([acc_q]), max_concurrency=4, cooldown=120.0)
    await pool_q.load()

    q1 = await pool_q.acquire()
    q2 = await pool_q.acquire()
    now = time.time()
    await pool_q.release(q1, UpstreamQuotaError("out of quota"))
    await pool_q.release(q2, UpstreamError("browser error"))
    live_q = pool_q.get("acc_q")
    assert live_q.status == AccountStatus.COOLING
    assert live_q.cooldown_until >= now + 3500


# ── #5.3: CDPSession.connect, BrowserDriver.startup(), _closed guards ────────


@pytest.mark.parametrize(
    "exc",
    [
        TimeoutError("timed out"),
        asyncio.TimeoutError("async timed out"),
        OSError("connection refused"),
        ConnectionClosedOK(Close(1000, "closed"), None),
    ],
)
async def test_cdp_session_connect_wraps_transport_errors(exc: BaseException) -> None:
    with (
        patch("muse2api.drivers.browser.cdp.connect", side_effect=exc),
        pytest.raises(CDPError, match="cdp connect failed"),
    ):
        await CDPSession.connect("ws://127.0.0.1:19210/devtools/browser/1")


async def test_browser_driver_startup_cleans_up_chromium_and_port_on_failure(tmp_path: Path) -> None:
    acc = Account(id="acc_st", cookies=dict(_VALID_COOKIES))
    settings = Settings(data_dir=tmp_path / "data", cdp_port=19560)
    drv = BrowserDriver(settings, account=acc, port=19560, profile_dir=tmp_path / "p_st")
    _USED_PORTS.add(19560)

    stop_mock = AsyncMock()
    with (
        patch("muse2api.drivers.browser.driver.find_chromium", return_value="msedge.exe"),
        patch("muse2api.drivers.browser.driver.ChromiumProcess.start", AsyncMock(return_value="ws://127.0.0.1:19561/b")),
        patch("muse2api.drivers.browser.driver.ChromiumProcess.stop", stop_mock),
        patch("muse2api.drivers.browser.driver.CDPSession.connect", AsyncMock(side_effect=CDPError("connect boom"))),
    ):
        # Simulate port reallocation inside start()
        async def fake_start_realloc(self):
            self.port = 19561
            return "ws://127.0.0.1:19561/b"

        with patch("muse2api.drivers.browser.driver.ChromiumProcess.start", fake_start_realloc):
            with pytest.raises(CDPError, match="connect boom"):
                await drv.startup()

    stop_mock.assert_awaited_once()
    assert drv._chromium is None
    assert drv._browser is None
    assert 19560 not in _USED_PORTS
    assert 19561 not in _USED_PORTS


async def test_open_tab_and_context_guard_closed_without_assertion_or_startup(
    tmp_path: Path,
) -> None:
    acc = Account(id="acc_cl", cookies=dict(_VALID_COOKIES))
    settings = Settings(data_dir=tmp_path / "data")
    drv = BrowserDriver(settings, account=acc, port=19570, profile_dir=tmp_path / "p_cl")
    await drv.shutdown()

    startup_spy = AsyncMock()
    recover_spy = AsyncMock()
    with (
        patch.object(drv, "startup", startup_spy),
        patch.object(drv, "_recover_browser", recover_spy),
    ):
        with pytest.raises(UpstreamError, match="closed"):
            await drv._context(acc)
        with pytest.raises(UpstreamError, match="closed"):
            await drv._open_tab(acc)

    startup_spy.assert_not_awaited()
    recover_spy.assert_not_awaited()

    # Race: shutdown() happens while startup() is running inside _open_tab()
    drv2 = BrowserDriver(settings, account=acc, port=19571, profile_dir=tmp_path / "p_cl2")

    async def startup_then_close() -> None:
        drv2._closed = True

    with (
        patch.object(drv2, "startup", side_effect=startup_then_close),
        pytest.raises(UpstreamError, match="closed"),
    ):
        await drv2._open_tab(acc)


# ── #5.4: _pipe_pair real loopback StreamWriter error/timeout & half-close ───


async def test_pipe_pair_loopback_error_timeout_and_normal_half_close() -> None:
    async def make_loopback_pair():
        accepted: asyncio.Future[tuple[asyncio.StreamReader, asyncio.StreamWriter]] = (
            asyncio.get_running_loop().create_future()
        )
        srv = await asyncio.start_server(
            lambda r, w: accepted.set_result((r, w)) if not accepted.done() else None,
            "127.0.0.1",
            0,
        )
        port = srv.sockets[0].getsockname()[1]
        c_r, c_w = await asyncio.open_connection("127.0.0.1", port)
        s_r, s_w = await accepted
        return srv, c_r, c_w, s_r, s_w

    # 1. Peer A reader hits ConnectionResetError while peer B stays silent -> exits < 1s (not 30s)
    srv_a, r_a, w_a, peer_a_r, peer_a_w = await make_loopback_pair()
    srv_b, r_b, w_b, peer_b_r, peer_b_w = await make_loopback_pair()
    try:
        assert w_a.can_write_eof() is True
        assert w_b.can_write_eof() is True

        async def fail_read(_n: int = 65536) -> bytes:
            await asyncio.sleep(0.03)
            raise ConnectionResetError("reset by peer")

        with patch.object(r_a, "read", side_effect=fail_read):
            t0 = time.monotonic()
            await asyncio.wait_for(
                _pipe_pair(r_a, w_b, r_b, w_a, idle_timeout=30.0),
                timeout=1.0,
            )
            elapsed = time.monotonic() - t0
            assert elapsed < 0.5
    finally:
        for w in (w_a, peer_a_w, w_b, peer_b_w):
            with contextlib.suppress(Exception):
                w.close()
        for s in (srv_a, srv_b):
            s.close()
            await s.wait_closed()

    # 2. Idle timeout expires promptly when both loopback peers stay silent
    srv_c, r_c, w_c, peer_c_r, peer_c_w = await make_loopback_pair()
    srv_d, r_d, w_d, peer_d_r, peer_d_w = await make_loopback_pair()
    try:
        t0 = time.monotonic()
        await asyncio.wait_for(
            _pipe_pair(r_c, w_d, r_d, w_c, idle_timeout=0.12),
            timeout=1.0,
        )
        assert time.monotonic() - t0 < 0.6
    finally:
        for w in (w_c, peer_c_w, w_d, peer_d_w):
            with contextlib.suppress(Exception):
                w.close()
        for s in (srv_c, srv_d):
            s.close()
            await s.wait_closed()

    # 3. Clean TCP half-close (write_eof) allows peer B to finish sending response
    srv_e, r_e, w_e, peer_e_r, peer_e_w = await make_loopback_pair()
    srv_f, r_f, w_f, peer_f_r, peer_f_w = await make_loopback_pair()
    try:
        peer_e_w.write(b"REQ")
        await peer_e_w.drain()
        peer_e_w.write_eof()

        async def delayed_reply() -> None:
            got = await peer_f_r.read()
            assert got == b"REQ"
            await asyncio.sleep(0.05)
            peer_f_w.write(b"RESP")
            await peer_f_w.drain()
            peer_f_w.close()

        replier = asyncio.create_task(delayed_reply())
        await asyncio.wait_for(
            _pipe_pair(r_e, w_f, r_f, w_e, idle_timeout=2.0),
            timeout=2.0,
        )
        await replier
        assert await asyncio.wait_for(peer_e_r.read(), timeout=1.0) == b"RESP"
    finally:
        for w in (w_e, peer_e_w, w_f, peer_f_w):
            with contextlib.suppress(Exception):
                w.close()
        for s in (srv_e, srv_f):
            s.close()
            await s.wait_closed()


# ── #5.5: LocalProxyRelay framing/smuggling, 502 on bad upstream, no Basic Og== ─


async def test_local_proxy_relay_smuggling_upstream_502_and_no_auth_header() -> None:
    seen_upstream: list[bytes] = []
    upstream_mode = "ok"

    async def fake_upstream(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        try:
            head = await reader.readuntil(b"\r\n\r\n")
            seen_upstream.append(head)
            if upstream_mode == "bad_cl":
                writer.write(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Length: 5\r\n\r\n12345"
                )
            elif upstream_mode == "bad_lf":
                writer.write(b"HTTP/1.1 200 OK\r\nX-Bad: 1\nEvil: 2\r\nContent-Length: 2\r\n\r\nOK")
            else:
                writer.write(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK")
            await writer.drain()
        finally:
            writer.close()
            await writer.wait_closed()

    upstream_srv = await asyncio.start_server(fake_upstream, "127.0.0.1", 0)
    upstream_port = upstream_srv.sockets[0].getsockname()[1]

    relay = LocalProxyRelay(f"http://127.0.0.1:{upstream_port}")
    try:
        await relay.start()
        assert relay.local_port is not None

        # 1. Bare \n, bare \r, \x00, compound TE, and duplicate TE -> 400 without hitting upstream
        bad_requests = [
            b"GET http://example.com/ HTTP/1.1\r\nX-Foo: 1\nTransfer-Encoding: chunked\r\n\r\n",
            b"GET http://example.com/ HTTP/1.1\r\nX-Foo: a\rb\r\n\r\n",
            b"GET http://example.com/ HTTP/1.1\r\nX-Foo: a\x00b\r\n\r\n",
            b"POST http://example.com/ HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: gzip, chunked\r\n\r\n0\r\n\r\n",
            b"POST http://example.com/ HTTP/1.1\r\nHost: example.com\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
        ]
        for raw in bad_requests:
            r, w = await asyncio.open_connection("127.0.0.1", relay.local_port)
            w.write(raw)
            await w.drain()
            resp = await asyncio.wait_for(r.read(), timeout=2.0)
            assert resp.startswith(b"HTTP/1.1 400")
            w.close()
            await w.wait_closed()
        assert seen_upstream == []

        # 2. Malformed upstream response headers -> 502 Bad Gateway (never 400)
        for mode in ("bad_cl", "bad_lf"):
            upstream_mode = mode
            r, w = await asyncio.open_connection("127.0.0.1", relay.local_port)
            w.write(b"GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n")
            await w.drain()
            resp = await asyncio.wait_for(r.read(), timeout=2.0)
            assert resp.startswith(b"HTTP/1.1 502")
            w.close()
            await w.wait_closed()

        # 3. Unauthenticated relay must NOT inject Proxy-Authorization: Basic Og==
        upstream_mode = "ok"
        seen_upstream.clear()
        r, w = await asyncio.open_connection("127.0.0.1", relay.local_port)
        w.write(b"GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n")
        await w.drain()
        resp = await asyncio.wait_for(r.read(), timeout=2.0)
        assert b"200 OK" in resp and resp.endswith(b"OK")
        w.close()
        await w.wait_closed()
        assert len(seen_upstream) == 1
        assert b"proxy-authorization" not in seen_upstream[0].lower()
        assert b"Basic Og==" not in seen_upstream[0]
    finally:
        await relay.stop()
        upstream_srv.close()
        await upstream_srv.wait_closed()


# ── #5.6: _new_thread exits immediately on closed CDP session ────────────────


async def test_new_thread_exits_immediately_on_closed_session(tmp_path: Path) -> None:
    settings = Settings(data_dir=tmp_path / "data", page_ready_timeout=45.0)
    drv = BrowserDriver(settings)

    class ClosingSession:
        def __init__(self) -> None:
            self.closed = False

        async def send(self, _method: str, _params=None):
            return {}

        async def evaluate(self, _expr: str, **_kwargs):
            self.closed = True
            raise CDPError("cdp connection closed")

    tab = _Tab("acc_1", "ctx_1", "tgt_1", ClosingSession())  # type: ignore[arg-type]
    t0 = time.monotonic()
    with (
        patch.object(drv, "_soft_new_thread", AsyncMock(return_value=False)),
        pytest.raises(CDPError, match="cdp connection closed"),
    ):
        await drv._new_thread(tab)
    assert time.monotonic() - t0 < 1.0


# ── #5.7: ChromiumProcess.stop kills proc & stops relay on CancelledError ────


async def test_chromium_process_stop_kills_proc_and_stops_relay_on_cancel(
    tmp_path: Path,
) -> None:
    # 1. Cancelled while awaiting proc.wait()
    cp = ChromiumProcess("msedge.exe", 19600, tmp_path / "p_stop", headless=True)
    wait_started = asyncio.Event()
    kill_calls = 0

    class HangingProc:
        pid = 99999
        returncode = None

        def terminate(self) -> None:
            return

        def kill(self) -> None:
            nonlocal kill_calls
            kill_calls += 1
            self.returncode = -9

        async def wait(self) -> int:
            wait_started.set()
            await asyncio.Future()
            return 0

    relay = MagicMock()
    relay.stop = AsyncMock()
    cp.proc = HangingProc()  # type: ignore[assignment]
    cp._proxy_relay = relay

    with patch("muse2api.drivers.browser.chromium.os.name", "posix"):
        task = asyncio.create_task(cp.stop())
        await wait_started.wait()
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task

    assert kill_calls == 1
    assert cp.proc is None
    assert cp._proxy_relay is None
    relay.stop.assert_awaited_once()

    # 2. Cancelled during Windows taskkill thread call
    cp2 = ChromiumProcess("msedge.exe", 19601, tmp_path / "p_stop2", headless=True)
    proc2 = MagicMock()
    proc2.pid = 88888
    proc2.returncode = None
    proc2.kill = MagicMock()
    relay2 = MagicMock()
    relay2.stop = AsyncMock()
    cp2.proc = proc2
    cp2._proxy_relay = relay2

    taskkill_started = asyncio.Event()

    async def fake_to_thread(*_args, **_kwargs):
        taskkill_started.set()
        await asyncio.Future()

    with (
        patch("muse2api.drivers.browser.chromium.os.name", "nt"),
        patch("muse2api.drivers.browser.chromium.asyncio.to_thread", side_effect=fake_to_thread),
    ):
        task2 = asyncio.create_task(cp2.stop())
        await taskkill_started.wait()
        task2.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task2

    proc2.kill.assert_called_once()
    assert cp2.proc is None
    assert cp2._proxy_relay is None
    relay2.stop.assert_awaited_once()


# ── #5.8: _fill_warm batch cleanup on double CancelledError ──────────────────


async def test_fill_warm_batch_cleanup_releases_all_tabs_on_double_cancel(
    tmp_path: Path,
) -> None:
    acc = Account(id="acc_batch", cookies=dict(_VALID_COOKIES))
    settings = Settings(data_dir=tmp_path / "data", warm_threads=2)
    drv = BrowserDriver(settings, account=acc)
    drv._browser = MagicMock(closed=False)

    tab1 = _Tab(acc.id, "ctx", "t1", MagicMock(closed=False), busy=True, warming=True)
    tab2 = _Tab(acc.id, "ctx", "t2", MagicMock(closed=False), busy=True, warming=True)
    drv._tabs[acc.id] = [tab1, tab2]

    step = 0

    async def fake_checkout(_account: Account, pending: int = 0):
        nonlocal step
        step += 1
        if step == 1:
            return tab1
        if step == 2:
            return tab2
        raise asyncio.CancelledError

    checkin_calls: list[str] = []

    async def flaky_checkin(tab: _Tab) -> None:
        checkin_calls.append(tab.target_id)
        if tab is tab1:
            raise asyncio.CancelledError

    with (
        patch.object(drv, "_checkout_for_warming", side_effect=fake_checkout),
        patch.object(drv, "_checkin", side_effect=flaky_checkin),
        pytest.raises(asyncio.CancelledError),
    ):
        await drv._fill_warm(acc)

    assert tab1.busy is False
    assert tab2.busy is False
    assert tab1.warming is False
    assert tab2.warming is False
    assert checkin_calls == ["t1", "t2"]


# ── #5.9: Validation/normalization regressions ───────────────────────────────


async def test_patch_partial_cookie_clear_marks_account_invalid(tmp_path: Path) -> None:
    settings = Settings(data_dir=tmp_path / "data", api_key="k", admin_key="k", driver="mock")
    app = create_app(settings)
    acc = Account(id="acc_part", label="Part", cookies=dict(_VALID_COOKIES))
    await app.state.services.pool.store.save([acc])

    async with app.router.lifespan_context(app):
        async with AsyncClient(transport=ASGITransport(app=app), base_url="http://test") as client:
            res = await client.patch(
                f"/admin/accounts/{acc.id}",
                headers={"Authorization": "Bearer k"},
                json={"cookies": {"hatch_sess": ""}},
            )
            assert res.status_code == 200
            body = res.json()["account"]
            assert body["status"] == "invalid"
            assert "hatch_sess" in (body["last_error"] or "")


async def test_responses_invalid_content_or_text_types_return_400(tmp_path: Path) -> None:
    settings = Settings(data_dir=tmp_path / "data", api_key="k", driver="mock")
    app = create_app(settings)
    headers = {"Authorization": "Bearer k"}

    bad_payloads = [
        {"model": "gpt-4o", "input": [{"role": "user", "content": 123}]},
        {"model": "gpt-4o", "input": [{"role": "user", "content": {"text": "hi"}}]},
        {"model": "gpt-4o", "input": [{"role": "user", "content": [{"type": "input_text", "text": 456}]}]},
        {"model": "gpt-4o", "input": [{"role": "user", "content": [{"type": "input_text", "text": None}]}]},
        {"model": "gpt-4o", "input": [{"role": 123, "content": "hi"}]},
    ]
    async with app.router.lifespan_context(app):
        async with AsyncClient(transport=ASGITransport(app=app), base_url="http://test") as client:
            for payload in bad_payloads:
                res = await client.post("/v1/responses", headers=headers, json=payload)
                assert res.status_code == 400, payload
                assert res.json()["error"]["code"] == "invalid_request"


async def test_media_rejects_empty_http_body_and_handles_uppercase_scheme(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    class FakeResp:
        def __init__(self, headers: dict[str, str], body: bytes) -> None:
            self.status_code = 200
            self.headers = httpx.Headers(headers)
            self._body = body

        def raise_for_status(self) -> None:
            return

        async def aiter_bytes(self, _n: int):
            if self._body:
                yield self._body

    class FakeClient:
        def __init__(self, **_kwargs) -> None:
            pass

        async def __aenter__(self):
            return self

        async def __aexit__(self, *_args):
            return False

        def stream(self, _method: str, _url: str, **_kwargs):
            return contextlib.asynccontextmanager(
                lambda: _ayield(FakeResp({"content-type": "image/png"}, b""))
            )()

    async def _ayield(val):
        yield val

    with (
        patch.object(media_mod, "_resolve_public_ips", AsyncMock(return_value=["93.184.216.34"])),
        patch.object(media_mod.httpx, "AsyncClient", FakeClient),
        pytest.raises(InvalidRequest, match="empty"),
    ):
        await media_mod._fetch_public_image("https://example.com/empty.png", timeout=5.0)

    # Uppercase HTTPS:// routes to _fetch_public_image instead of failing base64 decode
    fake_fetch = AsyncMock(return_value=(b"\x89PNG", "image/png"))
    monkeypatch.setattr(media_mod, "_fetch_public_image", fake_fetch)
    data, mime = await load_image_ref("HTTPS://example.com/pic.png")
    assert data == b"\x89PNG"
    assert mime == "image/png"
    fake_fetch.assert_awaited_once()


async def test_edit_images_negative_content_length_and_mime_normalization() -> None:
    svc = MagicMock()
    svc.settings.image_timeout = 30.0
    svc.gateway.generate_image = AsyncMock(
        return_value=[MediaResult(data=b"out", mime="image/png", kind="image", revised_prompt="ok")]
    )
    svc.media.save = MagicMock(return_value="img_ok.png")

    bad_req = MagicMock()
    bad_req.headers = {"content-length": "-5"}
    with pytest.raises(InvalidRequest, match="Content-Length"):
        await edit_images(bad_req, svc)

    uf = UploadFile(
        filename="photo.bin",
        file=MagicMock(),
        size=4,
        headers=Headers({"content-type": "IMAGE/PNG; charset=utf-8"}),
    )
    uf.read = AsyncMock(return_value=b"\x89PNG")  # type: ignore[method-assign]
    form = FormData([("prompt", "draw"), ("image", uf)])
    ok_req = MagicMock()
    ok_req.headers = {}
    ok_req.form = AsyncMock(return_value=form)
    ok_req.state = SimpleNamespace()
    ok_req.base_url = "http://localhost:8000/"

    res = await edit_images(ok_req, svc)
    assert len(res["data"]) == 1
    sent_req = svc.gateway.generate_image.await_args.args[0]
    assert sent_req.reference_images[0].mime == "image/png"


# ── #5.10: Additional coverage gaps ──────────────────────────────────────────


async def test_streaming_heartbeat_keepalive_empty_error_and_disconnect(tmp_path: Path) -> None:
    class ScriptedDriver(MuseDriver):
        name = "scripted"
        requires_account = False

        def __init__(self) -> None:
            self.mode = "slow"
            self.last_cancel: asyncio.Event | None = None
            self.closed_event = asyncio.Event()

        async def chat_stream(self, account: Account, req: ChatRequest):
            self.last_cancel = req.cancel
            try:
                if self.mode == "slow":
                    await asyncio.sleep(0.06)
                    yield "hello"
                    yield " world"
                elif self.mode == "empty":
                    await asyncio.sleep(0.04)
                    if False:
                        yield ""
                elif self.mode == "err_before_first":
                    await asyncio.sleep(0.04)
                    raise UpstreamError("upstream failed early")
                elif self.mode == "err_mid_stream":
                    yield "part1"
                    raise UpstreamError("upstream broke mid")
                elif self.mode == "hang":
                    await asyncio.sleep(60)
                    yield "never"
            finally:
                self.closed_event.set()

    drv = ScriptedDriver()
    settings = Settings(
        data_dir=tmp_path / "data",
        api_key="k",
        driver="mock",
        stream_heartbeat_interval=0.015,
    )
    app = create_app(settings, driver=drv)
    headers = {"Authorization": "Bearer k"}

    async with app.router.lifespan_context(app):
        async with AsyncClient(transport=ASGITransport(app=app), base_url="http://test") as client:
            for endpoint, payload in [
                ("/v1/chat/completions", {"model": "gpt-4o", "stream": True, "messages": [{"role": "user", "content": "hi"}]}),
                ("/v1/responses", {"model": "gpt-4o", "stream": True, "input": "hi"}),
            ]:
                # 1. Slow first token -> emits keepalive comments before payload
                drv.mode = "slow"
                drv.closed_event.clear()
                async with client.stream("POST", endpoint, headers=headers, json=payload) as resp:
                    assert resp.status_code == 200
                    text = (await resp.aread()).decode()
                assert ": keepalive" in text
                assert "hello" in text and " world" in text

                # 2. Empty stream (StopAsyncIteration) -> keepalive then clean completion
                drv.mode = "empty"
                drv.closed_event.clear()
                async with client.stream("POST", endpoint, headers=headers, json=payload) as resp:
                    assert resp.status_code == 200
                    text = (await resp.aread()).decode()
                assert ": keepalive" in text
                if endpoint.endswith("completions"):
                    assert "[DONE]" in text
                else:
                    assert "response.completed" in text

                # 3. Muse2APIError before first token and mid-stream -> SSE error JSON
                for err_mode in ("err_before_first", "err_mid_stream"):
                    drv.mode = err_mode
                    drv.closed_event.clear()
                    async with client.stream("POST", endpoint, headers=headers, json=payload) as resp:
                        assert resp.status_code == 200
                        text = (await resp.aread()).decode()
                    assert '"error"' in text

                # 4. Client disconnect while waiting for first token cancels pending task & closes stream
                drv.mode = "hang"
                drv.closed_event.clear()
                sent_chunks = await _stream_and_disconnect(app, endpoint, payload)
                await asyncio.wait_for(drv.closed_event.wait(), timeout=2.0)
                assert any(b": keepalive" in c for c in sent_chunks)
                assert drv.last_cancel is not None and drv.last_cancel.is_set()


async def _stream_and_disconnect(app, endpoint: str, payload: dict) -> list[bytes]:
    keepalive_seen = asyncio.Event()
    disconnect_trigger = asyncio.Event()
    sent_chunks: list[bytes] = []
    raw_body = json.dumps(payload).encode()
    body_sent = False

    async def asgi_receive():
        nonlocal body_sent
        if not body_sent:
            body_sent = True
            return {"type": "http.request", "body": raw_body, "more_body": False}
        await disconnect_trigger.wait()
        return {"type": "http.disconnect"}

    async def asgi_send(message):
        if message["type"] == "http.response.body":
            body_b = message.get("body", b"")
            if body_b:
                sent_chunks.append(body_b)
            if b": keepalive" in body_b:
                keepalive_seen.set()

    scope = {
        "type": "http",
        "asgi": {"version": "3.0"},
        "http_version": "1.1",
        "method": "POST",
        "scheme": "http",
        "path": endpoint,
        "raw_path": endpoint.encode(),
        "query_string": b"",
        "headers": [
            (b"authorization", b"Bearer k"),
            (b"content-type", b"application/json"),
            (b"content-length", str(len(raw_body)).encode()),
        ],
        "client": ("127.0.0.1", 12345),
        "server": ("test", 80),
    }
    asgi_task = asyncio.create_task(app(scope, asgi_receive, asgi_send))
    await asyncio.wait_for(keepalive_seen.wait(), timeout=2.0)
    disconnect_trigger.set()
    await asyncio.wait_for(asgi_task, timeout=2.0)
    return sent_chunks


async def test_sync_renewed_cookies_cdp_success_and_fallback(tmp_path: Path) -> None:
    settings = Settings(data_dir=tmp_path / "data")
    acc = Account(id="acc_sync", cookies={"hatch_sess": "new_s", "hatch_native_auth_device": "new_d"})

    # 1. Live tab succeeds -> calls _set_cookies and keeps tabs alive/not stale
    sub = BrowserDriver(settings, account=acc)
    live_sess = MagicMock(closed=False)
    live_sess.send = AsyncMock(return_value={})
    live_tab = _Tab(acc.id, "ctx", "t1", live_sess, busy=False)
    sub._tabs[acc.id] = [live_tab]

    await sub.sync_renewed_cookies(acc)
    assert live_sess.send.await_count == 2
    assert live_tab.stale is False
    assert live_tab.closed is False

    # 2. _set_cookies raises CDPError -> falls back to marking stale and closing idle tabs only
    failing_sess = MagicMock(closed=False, close=AsyncMock())
    failing_sess.send = AsyncMock(side_effect=CDPError("dead tab"))
    idle_tab = _Tab(acc.id, "ctx", "t_idle", failing_sess, busy=False)
    busy_tab = _Tab(acc.id, "ctx", "t_busy", MagicMock(closed=False, close=AsyncMock()), busy=True)
    sub._tabs[acc.id] = [idle_tab, busy_tab]
    sub._browser = MagicMock(closed=False, send=AsyncMock(return_value={}))

    await sub.sync_renewed_cookies(acc)
    assert idle_tab.stale is True
    assert idle_tab.closed is True
    assert busy_tab.stale is True
    assert busy_tab.closed is False

    # 3. Root coordinator delegates to sub-driver (and is a no-op if sub-driver absent)
    root = BrowserDriver(settings)
    await root.sync_renewed_cookies(acc)  # no-op
    mock_sub = MagicMock()
    mock_sub.sync_renewed_cookies = AsyncMock()
    root._account_drivers[acc.id] = mock_sub
    await root.sync_renewed_cookies(acc)
    mock_sub.sync_renewed_cookies.assert_awaited_once_with(acc)


async def test_local_proxy_relay_connect_timeout_refused_and_non_200_connect() -> None:
    # 1. Upstream TCP connect error -> 502 Bad Gateway
    orig_open = asyncio.open_connection

    async def selective_refuse(host, port, *args, **kwargs):
        if port == 19998:
            raise ConnectionRefusedError("connection refused")
        return await orig_open(host, port, *args, **kwargs)

    relay_refused = LocalProxyRelay("http://u:p@127.0.0.1:19998", connect_timeout=1.0)
    await relay_refused.start()
    try:
        with patch(
            "muse2api.drivers.browser.proxy_relay.asyncio.open_connection",
            side_effect=selective_refuse,
        ):
            r, w = await orig_open("127.0.0.1", relay_refused.local_port)
            w.write(b"GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n")
            await w.drain()
            resp = await asyncio.wait_for(r.read(), timeout=2.0)
            assert resp.startswith(b"HTTP/1.1 502 Bad Gateway")
            w.close()
            await w.wait_closed()
    finally:
        await relay_refused.stop()

    # 2. Upstream TCP connect timeout -> 504 Gateway Timeout
    relay_to = LocalProxyRelay("http://u:p@127.0.0.1:19999", connect_timeout=0.05)

    async def hanging_open_connection(host, port, *args, **kwargs):
        if port == 19999:
            await asyncio.sleep(5.0)
        return await orig_open(host, port, *args, **kwargs)

    await relay_to.start()
    try:
        with patch(
            "muse2api.drivers.browser.proxy_relay.asyncio.open_connection",
            side_effect=hanging_open_connection,
        ):
            r, w = await orig_open("127.0.0.1", relay_to.local_port)
            w.write(b"GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n")
            await w.drain()
            resp = await asyncio.wait_for(r.read(), timeout=2.0)
            assert resp.startswith(b"HTTP/1.1 504 Gateway Timeout")
            w.close()
            await w.wait_closed()
    finally:
        await relay_to.stop()

    # 3. Upstream non-200 CONNECT response (e.g. 407) -> forwarded and tunnel closed
    async def reject_connect(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        await reader.readuntil(b"\r\n\r\n")
        writer.write(b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic\r\n\r\n")
        await writer.drain()
        # Verify relay closes without piping subsequent bytes
        rest = await asyncio.wait_for(reader.read(64), timeout=1.0)
        assert rest == b""
        writer.close()
        await writer.wait_closed()

    up_srv = await asyncio.start_server(reject_connect, "127.0.0.1", 0)
    up_port = up_srv.sockets[0].getsockname()[1]
    relay_407 = LocalProxyRelay(f"http://u:p@127.0.0.1:{up_port}")
    await relay_407.start()
    try:
        r, w = await asyncio.open_connection("127.0.0.1", relay_407.local_port)
        w.write(b"CONNECT muse.ai:443 HTTP/1.1\r\nHost: muse.ai:443\r\n\r\n")
        await w.drain()
        resp = await asyncio.wait_for(r.read(), timeout=2.0)
        assert resp.startswith(b"HTTP/1.1 407 Proxy Authentication Required")
        w.close()
        await w.wait_closed()
    finally:
        await relay_407.stop()
        up_srv.close()
        await up_srv.wait_closed()


async def test_keystore_flush_failure_recovery_and_middleware_streaming_error(
    tmp_path: Path,
) -> None:
    # 1. KeyStore flush failure keeps _dirty=True and recovers on next flush
    keys_path = tmp_path / "keys.json"
    ks = KeyStore(keys_path)
    key, _plaintext = await ks.create("client-1")
    assert ks._dirty is False

    ks.touch(key)
    assert ks._dirty is True
    touched_at = key.last_used_at
    assert touched_at > 0

    with patch.object(ks, "_atomic_write", side_effect=OSError("disk full")):
        await ks.flush()
    assert ks._dirty is True

    await ks.flush()
    assert ks._dirty is False
    loaded = KeyStore(keys_path)
    await loaded.load()
    assert loaded.get(key.id) is not None
    assert loaded.get(key.id).last_used_at == pytest.approx(touched_at)

    # 2. RequestLogMiddleware records mid-stream SSE error on HTTP 200 streaming responses
    class FailingStreamDriver(MuseDriver):
        name = "failing_stream"
        requires_account = False

        async def chat_stream(self, account: Account, req: ChatRequest):
            await asyncio.sleep(0.01)
            raise UpstreamError("midstream SSE failure")
            yield ""

    settings = Settings(
        data_dir=tmp_path / "reqdata",
        api_key="k",
        driver="mock",
        stream_heartbeat_interval=0.005,
    )
    app = create_app(settings, driver=FailingStreamDriver())
    async with app.router.lifespan_context(app):
        async with AsyncClient(transport=ASGITransport(app=app), base_url="http://test") as client:
            res = await client.post(
                "/v1/chat/completions",
                headers={"Authorization": "Bearer k"},
                json={
                    "model": "gpt-4o",
                    "stream": True,
                    "messages": [{"role": "user", "content": "hi"}],
                },
            )
            assert res.status_code == 200
            assert "midstream SSE failure" in res.text

        rows = (await app.state.services.requests.query(limit=10))["data"]
        assert len(rows) == 1
        assert rows[0]["status_code"] == 200
        assert rows[0]["stream"] is True
        assert rows[0]["error"] == "midstream SSE failure"
