"""Tests for H6 + M6 Cluster A: lifecycle, locking, persistence, interactive cleanup, and keepalive races."""

from __future__ import annotations

import asyncio
import subprocess
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from httpx import ASGITransport, AsyncClient

from muse2api.accounts.keepalive import renew_account
from muse2api.accounts.model import Account, AccountStatus
from muse2api.accounts.pool import AccountPool
from muse2api.accounts.store import JsonAccountStore
from muse2api.api.deps import require_admin_key
from muse2api.app import create_app
from muse2api.auth.keys import KeyStore
from muse2api.config import Settings
from muse2api.core.matting import Matting
from muse2api.core.media import MediaStore
from muse2api.drivers.base import MuseDriver, SessionInfo
from muse2api.drivers.browser.driver import AccountBusy
from muse2api.drivers.browser.interactive import (
    InteractiveBrowserManager,
    InteractiveSession,
    _terminate_proc,
)
from muse2api.errors import NoAccountAvailable, UpstreamAuthError
from muse2api.services.container import Services
from muse2api.services.gateway import Gateway
from muse2api.services.request_log import RequestLog
from muse2api.services.tasks import TaskManager


class _DummyDriver(MuseDriver):
    name = "dummy"

    def __init__(self) -> None:
        self._busy_accounts: set[str] = set()
        self.released_accounts: list[str] = []
        self.stale_accounts: list[str] = []

    def is_account_busy(self, account_id: str) -> bool:
        return account_id in self._busy_accounts

    async def release_account_driver(self, account_id: str) -> None:
        self.released_accounts.append(account_id)

    def mark_account_stale(self, account_id: str) -> None:
        self.stale_accounts.append(account_id)

    async def chat_stream(self, account, req):
        yield "dummy"


@pytest.fixture
def svc(tmp_path: Path) -> Services:
    settings = Settings(
        data_dir=tmp_path / "data",
        media_dir=tmp_path / "media",
    )
    settings.ensure_dirs()
    driver = _DummyDriver()
    pool = AccountPool(
        JsonAccountStore(settings.accounts_file),
        strategy="round_robin",
        max_concurrency=1,
        acquire_timeout=1.0,
    )
    interactive = InteractiveBrowserManager(settings, pool, driver)
    pool.is_blocked_cb = interactive.is_session_active
    return Services(
        settings=settings,
        driver=driver,
        pool=pool,
        gateway=Gateway(pool, driver),
        tasks=TaskManager(settings.tasks_file),
        media=MediaStore(settings.media_dir),
        matting=Matting(settings.matting_model),
        keys=KeyStore(settings.keys_file),
        requests=RequestLog(settings.requests_db),
        interactive=interactive,
    )


@pytest.fixture
def client_app(svc: Services):
    app = create_app(svc.settings, svc.driver)
    app.state.services = svc
    app.dependency_overrides[require_admin_key] = lambda: None
    return app


# ── H6: PATCH /admin/accounts/{id} atomic validation & proxy=null ────────────


@pytest.mark.asyncio
async def test_patch_proxy_null_clears_proxy_and_omitted_preserves(svc: Services, client_app):
    acc = Account(
        id="acc_patch_proxy",
        label="Orig",
        proxy="http://user:pass@127.0.0.1:8888",
        cookies={"hatch_sess": "s1"},
    )
    await svc.pool.upsert(acc)

    async with AsyncClient(transport=ASGITransport(app=client_app), base_url="http://test") as client:
        # 1. Omitting proxy preserves it and does not release driver
        r_label = await client.patch(f"/admin/accounts/{acc.id}", json={"label": "Updated"})
        assert r_label.status_code == 200
        assert svc.pool.get(acc.id).proxy == "http://user:pass@127.0.0.1:8888"
        assert svc.driver.released_accounts == []

        # 2. Echoing masked proxy preserves unmasked proxy
        masked = r_label.json()["account"]["proxy"]
        assert masked == "http://***@127.0.0.1:8888"
        r_echo = await client.patch(f"/admin/accounts/{acc.id}", json={"proxy": masked})
        assert r_echo.status_code == 200
        assert svc.pool.get(acc.id).proxy == "http://user:pass@127.0.0.1:8888"
        assert svc.driver.released_accounts == []

        # 3. {"proxy": null} clears proxy and releases driver
        r_null = await client.patch(f"/admin/accounts/{acc.id}", json={"proxy": None})
        assert r_null.status_code == 200
        assert r_null.json()["account"]["proxy"] is None
        assert svc.pool.get(acc.id).proxy is None
        assert svc.driver.released_accounts == [acc.id]

        loaded = await svc.pool.store.load()
        assert loaded[0].proxy is None


@pytest.mark.asyncio
async def test_patch_validates_busy_and_duplicate_before_mutating_ram(svc: Services, client_app):
    other = Account(id="acc_other", label="Other", cookies={"hatch_sess": "taken_sess"})
    target = Account(
        id="acc_target",
        label="OriginalLabel",
        enabled=True,
        proxy="http://127.0.0.1:8000",
        cookies={"hatch_sess": "orig_sess"},
    )
    await svc.pool.upsert(other)
    await svc.pool.upsert(target)

    async with AsyncClient(transport=ASGITransport(app=client_app), base_url="http://test") as client:
        # 1. Busy account rejects proxy change WITHOUT mutating label/enabled/cookies
        svc.driver._busy_accounts.add(target.id)
        r_busy = await client.patch(
            f"/admin/accounts/{target.id}",
            json={
                "label": "MutatedWhenBusy",
                "enabled": False,
                "cookies": {"hatch_sess": "new_sess"},
                "proxy": "http://127.0.0.1:9000",
            },
        )
        assert r_busy.status_code == 409
        live = svc.pool.get(target.id)
        assert live.label == "OriginalLabel"
        assert live.enabled is True
        assert live.cookies["hatch_sess"] == "orig_sess"
        assert live.proxy == "http://127.0.0.1:8000"
        svc.driver._busy_accounts.discard(target.id)

        # 2. Duplicate hatch_sess rejects WITHOUT mutating label/enabled
        r_dup = await client.patch(
            f"/admin/accounts/{target.id}",
            json={
                "label": "MutatedWhenDup",
                "enabled": False,
                "cookies": {"hatch_sess": "taken_sess"},
            },
        )
        assert r_dup.status_code == 400
        live = svc.pool.get(target.id)
        assert live.label == "OriginalLabel"
        assert live.enabled is True
        assert live.cookies["hatch_sess"] == "orig_sess"


# ── H6: DELETE /admin/accounts/{id} busy guard & stop_session ────────────────


@pytest.mark.asyncio
async def test_delete_account_blocks_busy_and_stops_session_when_idle(svc: Services, client_app):
    acc = Account(id="acc_del_guard", label="Del", cookies={"hatch_sess": "s1"})
    await svc.pool.upsert(acc)

    async with AsyncClient(transport=ASGITransport(app=client_app), base_url="http://test") as client:
        # 1. Driver busy -> 409
        svc.driver._busy_accounts.add(acc.id)
        assert (await client.delete(f"/admin/accounts/{acc.id}")).status_code == 409
        assert svc.pool.get(acc.id) is not None
        svc.driver._busy_accounts.discard(acc.id)

        # 2. Pool inflight -> 409
        acquired = await svc.pool.acquire()
        assert acquired.id == acc.id
        assert (await client.delete(f"/admin/accounts/{acc.id}")).status_code == 409
        assert svc.pool.get(acc.id) is not None
        await svc.pool.release(acquired)

        # 3. Active interactive session -> 409
        sess = InteractiveSession(acc.id, "login")
        fake_task = MagicMock()
        fake_task.done.return_value = False
        sess.task = fake_task
        svc.interactive._sessions[acc.id] = sess
        assert (await client.delete(f"/admin/accounts/{acc.id}")).status_code == 409
        svc.interactive._sessions.pop(acc.id, None)

        # 4. Idle -> deletes, calls stop_session, persists, and releases driver
        with patch.object(svc.interactive, "stop_session", new_callable=AsyncMock) as stop_spy:
            r_ok = await client.delete(f"/admin/accounts/{acc.id}")
            assert r_ok.status_code == 200
            stop_spy.assert_awaited_once_with(acc.id)
        assert svc.pool.get(acc.id) is None
        assert acc.id in svc.driver.released_accounts
        assert await svc.pool.store.load() == []


# ── H6 + M6: Interactive cleanup on CancelledError / Exception & navigate ────


@pytest.mark.asyncio
async def test_interactive_login_cleans_up_on_cancellation_and_double_cancel(svc: Services):
    acc = Account(id="acc_cancel", proxy="http://user:pass@10.0.0.1:8080")
    await svc.pool.upsert(acc)

    class _FakeRelay:
        instances: list[_FakeRelay] = []

        def __init__(self, proxy: str) -> None:
            self.proxy = proxy
            self.stopped = False
            _FakeRelay.instances.append(self)

        async def start(self) -> str:
            return "http://127.0.0.1:44444"

        async def stop(self) -> None:
            self.stopped = True

    class _FakeProc:
        def __init__(self) -> None:
            self.returncode = None
            self.terminated = False
            self.killed = False

        def terminate(self) -> None:
            self.terminated = True
            self.returncode = 0

        def kill(self) -> None:
            self.killed = True
            self.returncode = -9

        async def wait(self) -> int:
            self.returncode = 0
            return 0

    class _CancellingCDP:
        def __init__(self) -> None:
            self.close_called = False

        async def close(self) -> None:
            self.close_called = True

        def on(self, _evt, _cb) -> None:
            pass

        async def send(self, _method, _params=None):
            return {}

    # Case 1: CancelledError during create_subprocess_exec cleans up proxy_relay and _sessions
    sess1 = InteractiveSession(acc.id, "login")
    svc.interactive._sessions[acc.id] = sess1
    with (
        patch("muse2api.drivers.browser.interactive.find_chromium", return_value="msedge"),
        patch("muse2api.drivers.browser.interactive.find_free_port", return_value=19444),
        patch("muse2api.drivers.browser.interactive.needs_proxy_relay", return_value=True),
        patch("muse2api.drivers.browser.interactive.LocalProxyRelay", _FakeRelay),
        patch(
            "muse2api.drivers.browser.interactive.asyncio.create_subprocess_exec",
            side_effect=asyncio.CancelledError,
        ),
    ):
        with pytest.raises(asyncio.CancelledError):
            await svc.interactive._run_login_session(acc, sess1)

    assert _FakeRelay.instances[-1].stopped is True
    assert acc.id in svc.driver.stale_accounts
    assert acc.id not in svc.interactive._sessions

    # Case 2: CancelledError during wait loop + second CancelledError inside cdp.close()
    svc.driver.stale_accounts.clear()
    proc2 = _FakeProc()
    cdp2 = _CancellingCDP()
    sess2 = InteractiveSession(acc.id, "login")
    svc.interactive._sessions[acc.id] = sess2

    async def _cancel_wait(*_a, **_kw):
        raise asyncio.CancelledError

    shield_calls = 0

    async def _flaky_shield(coro) -> None:
        nonlocal shield_calls
        shield_calls += 1
        if shield_calls == 1:
            if hasattr(coro, "close"):
                coro.close()
            raise asyncio.CancelledError
        await coro

    with (
        patch("muse2api.drivers.browser.interactive.find_chromium", return_value="msedge"),
        patch("muse2api.drivers.browser.interactive.find_free_port", return_value=19445),
        patch("muse2api.drivers.browser.interactive.needs_proxy_relay", return_value=True),
        patch("muse2api.drivers.browser.interactive.LocalProxyRelay", _FakeRelay),
        patch("muse2api.drivers.browser.interactive.asyncio.create_subprocess_exec", new=AsyncMock(return_value=proc2)),
        patch("muse2api.drivers.browser.interactive._resolve_page_ws_url", new=AsyncMock(return_value="ws://127.0.0.1:1/p")),
        patch("muse2api.drivers.browser.interactive.CDPSession.connect", new=AsyncMock(return_value=cdp2)),
        patch("muse2api.drivers.browser.interactive._shield_quiet", side_effect=_flaky_shield),
        patch("muse2api.drivers.browser.interactive.asyncio.sleep", side_effect=_cancel_wait),
    ):
        with pytest.raises(asyncio.CancelledError):
            await svc.interactive._run_login_session(acc, sess2)

    assert proc2.terminated is True or proc2.killed is True
    assert _FakeRelay.instances[-1].stopped is True
    assert acc.id in svc.driver.stale_accounts
    assert acc.id not in svc.interactive._sessions


@pytest.mark.asyncio
async def test_view_session_navigates_after_about_blank_even_without_cookies(svc: Services):
    acc = Account(
        id="acc_view_nav",
        cookies={},  # empty cookies!
        meta={"last_thread_url": "https://muse.ai/thread/abc"},
    )
    await svc.pool.upsert(acc)

    calls: list[tuple[str, dict | None]] = []

    class _FakeCDP:
        def __init__(self) -> None:
            self.closed = False

        async def send(self, method: str, params: dict | None = None):
            calls.append((method, params))
            return {}

        async def close(self) -> None:
            self.closed = True

    class _FakeProc:
        def __init__(self) -> None:
            self.returncode = None

        def terminate(self) -> None:
            self.returncode = 0

        def kill(self) -> None:
            self.returncode = -9

        async def wait(self) -> int:
            self.returncode = 0
            return 0

    fake_cdp = _FakeCDP()
    fake_proc = _FakeProc()
    sess = InteractiveSession(acc.id, "view")
    svc.interactive._sessions[acc.id] = sess

    with (
        patch("muse2api.drivers.browser.interactive.find_chromium", return_value="msedge"),
        patch("muse2api.drivers.browser.interactive.find_free_port", return_value=19555),
        patch(
            "muse2api.drivers.browser.interactive.asyncio.create_subprocess_exec",
            new=AsyncMock(return_value=fake_proc),
        ) as mock_exec,
        patch(
            "muse2api.drivers.browser.interactive._resolve_page_ws_url",
            new=AsyncMock(return_value="ws://127.0.0.1:19555/devtools/page/1"),
        ),
        patch("muse2api.drivers.browser.interactive.CDPSession.connect", new=AsyncMock(return_value=fake_cdp)),
    ):
        await svc.interactive._run_view_session(acc, sess)

    assert mock_exec.call_args[0][-1] == "about:blank"
    assert ("Page.navigate", {"url": "https://muse.ai/thread/abc"}) in calls
    assert fake_cdp.closed is True
    assert acc.id not in svc.interactive._sessions


@pytest.mark.asyncio
async def test_terminate_proc_windows_taskkill_and_cancelled_error():
    class _WinProc:
        def __init__(self, pid: int) -> None:
            self.pid = pid
            self.returncode = None
            self.killed = False
            self.terminated = False

        def terminate(self) -> None:
            self.terminated = True

        def kill(self) -> None:
            self.killed = True
            self.returncode = -9

        async def wait(self) -> int:
            self.returncode = 0
            return 0

    proc = _WinProc(pid=43210)
    fake_run = MagicMock(return_value=MagicMock(returncode=0))
    with (
        patch("muse2api.drivers.browser.interactive.os.name", "nt"),
        patch("muse2api.drivers.browser.interactive.subprocess.run", fake_run),
    ):
        await _terminate_proc(proc)  # type: ignore[arg-type]

    fake_run.assert_called_once()
    args, kwargs = fake_run.call_args
    assert args[0] == ["taskkill", "/PID", "43210", "/T", "/F"]
    assert kwargs["creationflags"] == getattr(subprocess, "CREATE_NO_WINDOW", 0x08000000)

    # CancelledError during proc.wait() kills proc and re-raises
    class _HangingProc:
        pid = None
        returncode = None
        killed = False

        def terminate(self) -> None:
            pass

        def kill(self) -> None:
            self.killed = True

        async def wait(self) -> int:
            raise asyncio.CancelledError

    hp = _HangingProc()
    with pytest.raises(asyncio.CancelledError):
        await _terminate_proc(hp)  # type: ignore[arg-type]
    assert hp.killed is True


@pytest.mark.asyncio
async def test_stop_session_cancels_task_kills_proc_and_evicts(svc: Services):
    acc = Account(id="acc_stop_sess", cookies={"hatch_sess": "s1"})
    await svc.pool.upsert(acc)

    started = asyncio.Event()

    async def _hang() -> None:
        started.set()
        await asyncio.sleep(60)

    proc = MagicMock()
    proc.returncode = None
    proc.pid = None
    proc.terminate = MagicMock(side_effect=lambda: setattr(proc, "returncode", 0))
    proc.wait = AsyncMock(return_value=0)

    sess = InteractiveSession(acc.id, "login")
    sess.proc = proc
    sess.task = asyncio.create_task(_hang())
    await started.wait()
    svc.interactive._sessions[acc.id] = sess

    assert await svc.interactive.stop_session(acc.id) is True
    assert sess.task.done()
    assert acc.id not in svc.interactive._sessions
    assert await svc.interactive.stop_session(acc.id) is False


# ── H6: TOCTOU between pool.acquire() and interactive.open_login() ──────────


@pytest.mark.asyncio
async def test_toctou_acquire_vs_open_login_never_both_succeed(svc: Services):
    acc = Account(id="acc_race", cookies={"hatch_sess": "s1"})
    await svc.pool.upsert(acc)

    gate = asyncio.Event()

    async def _fake_login(_acc, _sess):
        await gate.wait()

    with patch.object(svc.interactive, "_run_login_session", side_effect=_fake_login):
        acq_res, login_res = await asyncio.gather(
            svc.pool.acquire(),
            svc.interactive.open_login(acc),
            return_exceptions=True,
        )
        gate.set()
        await svc.interactive.stop_session(acc.id)

    # Exactly one succeeded and the other was blocked by NoAccountAvailable or AccountBusy
    if isinstance(acq_res, Account):
        assert isinstance(login_res, AccountBusy)
        await svc.pool.release(acq_res)
    else:
        assert isinstance(acq_res, NoAccountAvailable)
        assert isinstance(login_res, dict) and login_res["ok"] is True


# ── H6: AccountPool.persist() serialization & deep-copy isolation ────────────


@pytest.mark.asyncio
async def test_pool_persist_serializes_and_deep_copies(tmp_path: Path):
    saved_snapshots: list[list[Account]] = []
    first_save_entered = asyncio.Event()
    allow_first_save = asyncio.Event()
    save_calls = 0
    armed = False

    class _SlowStore:
        async def load(self) -> list[Account]:
            return []

        async def save(self, accounts: list[Account]) -> None:
            nonlocal save_calls
            if not armed:
                return
            save_calls += 1
            if save_calls == 1:
                first_save_entered.set()
                await allow_first_save.wait()
            saved_snapshots.append([a.model_copy(deep=True) for a in accounts])

    pool = AccountPool(_SlowStore())  # type: ignore[arg-type]
    acc = Account(id="acc_p", label="v1", cookies={"hatch_sess": "c1"})
    await pool.upsert(acc)
    armed = True

    # Reset and run two concurrent persists where the first is slow
    saved_snapshots.clear()
    save_calls = 0
    first_save_entered.clear()
    allow_first_save.clear()

    t1 = asyncio.create_task(pool.persist())
    await first_save_entered.wait()

    # Mutate in-RAM cookie right after snapshot 1 was taken, and trigger update + persist 2
    acc.label = "in_ram_should_not_corrupt_snap1"
    t2 = asyncio.create_task(
        pool.try_update("acc_p", lambda a: (setattr(a, "label", "v2") or True, None))
    )
    await asyncio.sleep(0)
    allow_first_save.set()
    await asyncio.gather(t1, t2)

    assert len(saved_snapshots) == 2
    assert saved_snapshots[0][0].label == "v1"
    assert saved_snapshots[1][0].label == "v2"


@pytest.mark.asyncio
async def test_pool_release_applies_to_live_account_even_if_detached(tmp_path: Path):
    pool = AccountPool(JsonAccountStore(tmp_path / "accounts.json"))
    live = Account(id="acc_rel", label="live", cookies={"hatch_sess": "s1"})
    await pool.upsert(live)

    leased = await pool.acquire()
    # Replace entry in _accounts with a fresh copy (simulating concurrent upsert/reload)
    replacement = live.model_copy(deep=True)
    replacement.label = "updated_label"
    await pool.upsert(replacement)

    await pool.release(leased, UpstreamAuthError("expired"))
    assert pool.get("acc_rel").status == AccountStatus.INVALID
    assert pool.get("acc_rel").label == "updated_label"


# ── M6: keepalive.renew_account lost-update & concurrent mutation races ──────


@pytest.mark.asyncio
async def test_keepalive_aborts_on_concurrent_cookie_change_or_delete(tmp_path: Path):
    pool = AccountPool(JsonAccountStore(tmp_path / "accounts.json"))
    acc = Account(id="acc_ka", label="Init", cookies={"hatch_sess": "old_sess"})
    await pool.upsert(acc)

    entered = asyncio.Event()
    release_renew = asyncio.Event()
    mode = "success"

    class _SlowRenewDriver(MuseDriver):
        name = "slow_renew"

        async def chat_stream(self, account, req):
            yield ""

        async def renew_session(self, account: Account) -> SessionInfo:
            entered.set()
            await release_renew.wait()
            if mode == "auth_err":
                raise UpstreamAuthError("old cookie rejected")
            return SessionInfo(
                ok=True,
                cookies={"hatch_sess": "stale_rotated"},
                meta={"vm_wake_ok": True},
            )

    drv = _SlowRenewDriver()

    # 1. Concurrent cookie replacement -> success path must NOT overwrite new cookies
    t1 = asyncio.create_task(renew_account(pool, drv, acc.id))
    await entered.wait()
    await pool.try_update(acc.id, lambda a: (a.cookies.__setitem__("hatch_sess", "fresh_patch") or True, None))
    release_renew.set()
    res1 = await t1
    assert res1["ok"] is False
    assert pool.get(acc.id).cookies["hatch_sess"] == "fresh_patch"

    # 2. Concurrent cookie replacement -> UpstreamAuthError must NOT mark account INVALID
    entered.clear()
    release_renew.clear()
    mode = "auth_err"
    t2 = asyncio.create_task(renew_account(pool, drv, acc.id))
    await entered.wait()
    await pool.try_update(acc.id, lambda a: (a.cookies.__setitem__("hatch_sess", "newest_login") or True, None))
    release_renew.set()
    res2 = await t2
    assert res2["ok"] is False
    assert pool.get(acc.id).status == AccountStatus.ACTIVE
    assert pool.get(acc.id).cookies["hatch_sess"] == "newest_login"

    # 3. Concurrent label update (cookies unchanged) -> label preserved while cookies merge
    entered.clear()
    release_renew.clear()
    mode = "success"
    t3 = asyncio.create_task(renew_account(pool, drv, acc.id))
    await entered.wait()
    await pool.try_update(acc.id, lambda a: (setattr(a, "label", "ConcurrentLabel") or True, None))
    release_renew.set()
    res3 = await t3
    assert res3["ok"] is True
    assert pool.get(acc.id).label == "ConcurrentLabel"
    assert pool.get(acc.id).cookies["hatch_sess"] == "stale_rotated"

    # 4. Concurrent delete -> returns account deleted without resurrecting
    entered.clear()
    release_renew.clear()
    t4 = asyncio.create_task(renew_account(pool, drv, acc.id))
    await entered.wait()
    await pool.remove(acc.id)
    release_renew.set()
    res4 = await t4
    assert res4 == {"ok": False, "error": "account deleted"}
    assert pool.get(acc.id) is None

    # 5. Interactive session starts mid-flight -> aborts mutation
    acc2 = Account(id="acc_ka2", cookies={"hatch_sess": "s2"})
    await pool.upsert(acc2)
    active_interactive = False
    pool.is_blocked_cb = lambda aid: active_interactive and aid == acc2.id
    entered.clear()
    release_renew.clear()
    t5 = asyncio.create_task(renew_account(pool, drv, acc2.id))
    await entered.wait()
    active_interactive = True
    release_renew.set()
    res5 = await t5
    assert res5["ok"] is False
    assert pool.get(acc2.id).cookies["hatch_sess"] == "s2"
