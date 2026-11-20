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
from muse2api.drivers.base import MuseDriver
from muse2api.drivers.browser.interactive import (
    InteractiveBrowserManager,
    InteractiveSession,
    _is_muse_hostname,
    _resolve_page_ws_url,
    _sanitize_view_url,
)
from muse2api.errors import NoAccountAvailable
from muse2api.services.container import Services
from muse2api.services.gateway import Gateway
from muse2api.services.request_log import RequestLog
from muse2api.services.tasks import TaskManager


class DummyDriver(MuseDriver):
    name = "dummy"

    def __init__(self):
        self._busy_accounts = set()
        self.released_accounts = []
        self.stale_accounts = []

    def is_account_busy(self, account_id: str) -> bool:
        return account_id in self._busy_accounts

    async def release_account_driver(self, account_id: str) -> None:
        self.released_accounts.append(account_id)

    def mark_account_stale(self, account_id: str) -> None:
        self.stale_accounts.append(account_id)

    async def chat_stream(self, account, req):
        yield "dummy"


@pytest.fixture
def test_services(tmp_path):
    settings = Settings(
        data_dir=tmp_path / "data",
        media_dir=tmp_path / "media",
    )
    settings.ensure_dirs()
    driver = DummyDriver()
    pool = AccountPool(
        JsonAccountStore(settings.accounts_file),
        strategy="round_robin",
        max_concurrency=1,
    )
    requests = RequestLog(settings.requests_db)
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
        requests=requests,
        interactive=interactive,
    )


@pytest.fixture
def app(test_services):
    application = create_app(test_services.settings, test_services.driver)
    application.state.services = test_services
    application.dependency_overrides[require_admin_key] = lambda: None
    return application


@pytest.mark.asyncio
async def test_account_create_with_proxy_and_empty_cookies(test_services, app):
    async with AsyncClient(transport=ASGITransport(app=app), base_url="http://test") as client:
        res = await client.post("/admin/accounts", json={
            "label": "Test Acc",
            "proxy": "http://127.0.0.1:8888",
        })
        assert res.status_code == 200
        data = res.json()
        assert data["account"]["label"] == "Test Acc"
        assert data["account"]["proxy"] == "http://127.0.0.1:8888"
        assert data["account"]["status"] == AccountStatus.INVALID.value


@pytest.mark.asyncio
async def test_account_update_proxy(test_services, app):
    acc = Account(id="acc_update_proxy", label="Acc 1", cookies={"hatch_sess": "s1"})
    await test_services.pool.upsert(acc)

    async with AsyncClient(transport=ASGITransport(app=app), base_url="http://test") as client:
        res = await client.patch(f"/admin/accounts/{acc.id}", json={
            "proxy": "socks5://127.0.0.1:1080",
        })
        assert res.status_code == 200
        assert res.json()["account"]["proxy"] == "socks5://127.0.0.1:1080"

        # Verify persisted
        refreshed = test_services.pool.get(acc.id)
        assert refreshed.proxy == "socks5://127.0.0.1:1080"


@pytest.mark.asyncio
async def test_open_login_not_found(test_services, app):
    async with AsyncClient(transport=ASGITransport(app=app), base_url="http://test") as client:
        res = await client.post("/admin/accounts/acc_nonexistent/open-login")
        assert res.status_code == 404


@pytest.mark.asyncio
async def test_open_login_busy_account_locked(test_services, app):
    acc = Account(id="acc_busy_login", label="Busy Acc", cookies={"hatch_sess": "s1"})
    await test_services.pool.upsert(acc)
    test_services.driver._busy_accounts.add(acc.id)

    async with AsyncClient(transport=ASGITransport(app=app), base_url="http://test") as client:
        res = await client.post(f"/admin/accounts/{acc.id}/open-login")
        assert res.status_code == 409
        err = res.json().get("error", {})
        assert err.get("code") == "account_busy" or "bận" in err.get("message", "")


@pytest.mark.asyncio
async def test_open_login_success(test_services, app):
    acc = Account(id="acc_login_ok", label="Login Acc", proxy="http://proxy:8080")
    await test_services.pool.upsert(acc)

    with patch.object(test_services.interactive, "_run_login_session", new_callable=AsyncMock):
        async with AsyncClient(transport=ASGITransport(app=app), base_url="http://test") as client:
            res = await client.post(f"/admin/accounts/{acc.id}/open-login")
            assert res.status_code == 200
            data = res.json()
            assert data.get("ok") is True


@pytest.mark.asyncio
async def test_open_view_not_found_and_busy(test_services, app):
    async with AsyncClient(transport=ASGITransport(app=app), base_url="http://test") as client:
        res = await client.post("/admin/accounts/acc_missing/open-view")
        assert res.status_code == 404

    acc = Account(id="acc_busy_view", label="Busy Acc View", cookies={"hatch_sess": "s1"})
    await test_services.pool.upsert(acc)
    test_services.driver._busy_accounts.add(acc.id)

    async with AsyncClient(transport=ASGITransport(app=app), base_url="http://test") as client:
        res = await client.post(f"/admin/accounts/{acc.id}/open-view")
        assert res.status_code == 409


@pytest.mark.asyncio
async def test_open_view_success(test_services, app):
    acc = Account(id="acc_view_ok", label="View Acc", meta={"last_thread_url": "https://muse.ai/thread/123"})
    await test_services.pool.upsert(acc)

    with patch.object(test_services.interactive, "_run_view_session", new_callable=AsyncMock):
        async with AsyncClient(transport=ASGITransport(app=app), base_url="http://test") as client:
            res = await client.post(f"/admin/accounts/{acc.id}/open-view")
            assert res.status_code == 200
            data = res.json()
            assert data.get("ok") is True


@pytest.mark.asyncio
async def test_token_and_cookie_harvest_persists(test_services):
    acc = Account(id="acc_harvest", label="Harvest Acc", status=AccountStatus.INVALID)
    await test_services.pool.upsert(acc)

    # Simulate harvesting cookies and token
    new_cookies = {"hatch_sess": "harvested_sess_123", "hatch_native_auth_device": "dev_456"}
    new_expires = {"hatch_sess": 1800000000}
    jwt_tokens = {"auth_token": "bearer_xyz789"}

    target = test_services.pool.get(acc.id)
    target.cookies.update(new_cookies)
    target.cookie_expires.update(new_expires)
    target.status = AccountStatus.ACTIVE
    target.meta["auth_tokens"] = jwt_tokens
    await test_services.pool.upsert(target)

    # Verify persisted in pool
    loaded = test_services.pool.get(acc.id)
    assert loaded.status == AccountStatus.ACTIVE
    assert loaded.cookies["hatch_sess"] == "harvested_sess_123"
    assert loaded.cookies["hatch_native_auth_device"] == "dev_456"
    assert loaded.cookie_expires["hatch_sess"] == 1800000000
    assert loaded.meta["auth_tokens"]["auth_token"] == "bearer_xyz789"


def test_is_muse_hostname_and_sanitize_view_url():
    assert _is_muse_hostname("muse.ai") is True
    assert _is_muse_hostname("app.muse.ai") is True
    assert _is_muse_hostname("MUSE.AI.") is True
    assert _is_muse_hostname(".muse.ai") is False
    assert _is_muse_hostname("evil-muse.ai") is False
    assert _is_muse_hostname("evil.com") is False
    assert _is_muse_hostname("") is False

    base = "https://muse.ai"
    assert _sanitize_view_url("https://muse.ai/thread/123", base) == "https://muse.ai/thread/123"
    assert _sanitize_view_url("https://sub.muse.ai/workspace", base) == "https://sub.muse.ai/workspace"
    assert _sanitize_view_url("http://muse.ai/thread/123", base) == base
    assert _sanitize_view_url("https://evil.com@muse.ai/thread", base) == base
    assert _sanitize_view_url("https://evil.com\\@muse.ai/thread", base) == base
    assert _sanitize_view_url("javascript:alert(1)", base) == base
    assert _sanitize_view_url("", base) == base


@pytest.mark.asyncio
async def test_resolve_page_ws_url_selects_page_target():
    proc = MagicMock()
    proc.returncode = None

    class FakeResp:
        def __init__(self, status_code, payload):
            self.status_code = status_code
            self._payload = payload

        def json(self):
            return self._payload

    class FakeHttpClient:
        def __init__(self, *args, **kwargs):
            self.calls = 0

        async def __aenter__(self):
            return self

        async def __aexit__(self, *args):
            pass

        async def get(self, url):
            self.calls += 1
            assert url.endswith("/json/list")
            if self.calls == 1:
                return FakeResp(200, [{"type": "service_worker", "webSocketDebuggerUrl": "ws://sw"}])
            return FakeResp(
                200,
                [
                    {"type": "service_worker", "webSocketDebuggerUrl": "ws://sw"},
                    {
                        "type": "page",
                        "url": "https://muse.ai/login",
                        "webSocketDebuggerUrl": "ws://127.0.0.1:9222/devtools/page/PAGE1",
                    },
                ],
            )

    with patch("muse2api.drivers.browser.interactive.httpx.AsyncClient", FakeHttpClient):
        ws_url = await _resolve_page_ws_url(9222, proc, attempts=5, interval=0.01)
        assert ws_url == "ws://127.0.0.1:9222/devtools/page/PAGE1"


@pytest.mark.asyncio
async def test_run_login_session_harvests_and_deduplicates_writes(test_services):
    acc = Account(
        id="acc_login_flow",
        label="Flow Acc",
        cookies={"__Host-token": "init_host", "hatch_sess": "old_sess"},
        status=AccountStatus.INVALID,
    )
    await test_services.pool.upsert(acc)

    class FakeProc:
        def __init__(self):
            self.returncode = None
            self.get_cookies_calls = 0

        def terminate(self):
            self.returncode = 0

        def kill(self):
            self.returncode = 0

        async def wait(self):
            self.returncode = 0
            return 0

    proc = FakeProc()

    class FakeCDPSession:
        def __init__(self, ws_url):
            self.ws_url = ws_url
            self.calls = []
            self.handlers = {}
            self.closed = False

        async def close(self):
            self.closed = True

        def on(self, event, handler):
            self.handlers[event] = handler

        async def send(self, method, params=None, timeout=15.0):
            self.calls.append((method, params))
            if method == "Network.getCookies":
                # Emit an authenticated muse.ai request event on first poll
                if proc.get_cookies_calls == 0:
                    handler = self.handlers.get("Network.requestWillBeSent")
                    if handler:
                        handler({
                            "request": {
                                "url": "https://muse.ai/api/user",
                                "headers": {
                                    "Authorization": "Bearer header_jwt_999",
                                    "X-evil": "ignored",
                                },
                            }
                        })
                proc.get_cookies_calls += 1
                if proc.get_cookies_calls >= 2:
                    proc.returncode = 0
                return {
                    "cookies": [
                        {
                            "name": "hatch_sess",
                            "value": "new_sess_abc",
                            "domain": ".muse.ai",
                            "expires": 1900000000,
                        }
                    ]
                }
            return {}

        async def evaluate(self, expression, **kwargs):
            self.calls.append(("Runtime.evaluate", expression))
            return {"access_token": "ls_jwt_123456789012345"}

    fake_cdp_instances = []

    async def fake_connect(ws_url):
        inst = FakeCDPSession(ws_url)
        fake_cdp_instances.append(inst)
        return inst

    upsert_spy = AsyncMock(wraps=test_services.pool.upsert)
    sess = InteractiveSession(acc.id, "login")

    with (
        patch("muse2api.drivers.browser.interactive.find_chromium", return_value="C:/fake/msedge.exe"),
        patch("muse2api.drivers.browser.interactive.find_free_port", return_value=19333),
        patch(
            "muse2api.drivers.browser.interactive.asyncio.create_subprocess_exec",
            new=AsyncMock(return_value=proc),
        ) as mock_exec,
        patch(
            "muse2api.drivers.browser.interactive._resolve_page_ws_url",
            new=AsyncMock(return_value="ws://127.0.0.1:19333/devtools/page/P1"),
        ),
        patch("muse2api.drivers.browser.interactive.CDPSession.connect", side_effect=fake_connect),
        patch("muse2api.drivers.browser.interactive.asyncio.sleep", new=AsyncMock()),
        patch.object(test_services.pool, "upsert", upsert_spy),
    ):
        await test_services.interactive._run_login_session(acc, sess)

    # Verify release_account_driver was called before launching, and mark_account_stale in finally
    assert acc.id in test_services.driver.released_accounts
    assert acc.id in test_services.driver.stale_accounts

    # Verify --remote-debugging-address=127.0.0.1 was passed to create_subprocess_exec
    exec_args = mock_exec.call_args[0]
    assert "--remote-debugging-address=127.0.0.1" in exec_args

    # Verify __Host- cookie injection had no domain and had secure=True
    assert len(fake_cdp_instances) == 1
    set_cookie_calls = [p for m, p in fake_cdp_instances[0].calls if m == "Network.setCookie"]
    host_cookie = next(c for c in set_cookie_calls if c["name"] == "__Host-token")
    assert "domain" not in host_cookie
    assert host_cookie["secure"] is True
    normal_cookie = next(c for c in set_cookie_calls if c["name"] == "hatch_sess")
    assert normal_cookie["domain"] == ".muse.ai"
    assert normal_cookie["secure"] is True

    # Two identical loop iterations should deduplicate to only 1 pool.upsert call
    assert proc.get_cookies_calls == 2
    assert upsert_spy.call_count == 1

    updated = test_services.pool.get(acc.id)
    assert updated.status == AccountStatus.ACTIVE
    assert updated.cookies["hatch_sess"] == "new_sess_abc"
    assert updated.meta["auth_tokens"]["access_token"] == "ls_jwt_123456789012345"
    assert updated.meta["auth_headers"]["authorization"] == "Bearer header_jwt_999"


@pytest.mark.asyncio
async def test_interactive_session_blocks_pool_keepalive_and_quota(test_services, app):
    acc = Account(
        id="acc_active_interactive",
        label="Interactive Active",
        cookies={"hatch_sess": "s1"},
        status=AccountStatus.ACTIVE,
    )
    await test_services.pool.upsert(acc)

    # Simulate active interactive session
    sess = InteractiveSession(acc.id, "login")
    fake_task = MagicMock()
    fake_task.done.return_value = False
    sess.task = fake_task
    test_services.interactive._sessions[acc.id] = sess
    assert test_services.interactive.is_session_active(acc.id) is True

    # 1. Pool lease skips the account while interactive session is open
    with pytest.raises(NoAccountAvailable):
        async with test_services.pool.lease():
            pass

    # 2. Keepalive renew_account skips the account while interactive session is open
    renew_res = await renew_account(test_services.pool, test_services.driver, acc.id)
    assert renew_res["ok"] is False

    # 3. Admin quota endpoint returns 409 AccountBusy instead of stealing the profile
    async with AsyncClient(transport=ASGITransport(app=app), base_url="http://test") as client:
        res = await client.get(f"/admin/accounts/{acc.id}/quota")
        assert res.status_code == 409

    # Cleanup
    test_services.interactive._sessions.pop(acc.id, None)


@pytest.mark.asyncio
async def test_interactive_shutdown_terminates_active_sessions(test_services):
    acc = Account(id="acc_shutdown", label="Shutdown Acc", cookies={"hatch_sess": "s1"})
    await test_services.pool.upsert(acc)

    import asyncio

    started = asyncio.Event()

    async def fake_long_session():
        started.set()
        await asyncio.sleep(60)

    sess = InteractiveSession(acc.id, "login")
    sess.task = asyncio.create_task(fake_long_session())
    await started.wait()
    test_services.interactive._sessions[acc.id] = sess

    await test_services.interactive.shutdown()
    assert sess.task.cancelled() or sess.task.done()
    assert test_services.interactive.is_session_active(acc.id) is False

