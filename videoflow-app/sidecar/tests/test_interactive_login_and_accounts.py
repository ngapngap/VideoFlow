import asyncio
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from httpx import ASGITransport, AsyncClient

from muse2api.accounts.model import Account, AccountStatus
from muse2api.accounts.pool import AccountPool
from muse2api.accounts.store import JsonAccountStore
from muse2api.app import create_app
from muse2api.api.deps import require_admin_key
from muse2api.auth.keys import KeyStore
from muse2api.config import Settings
from muse2api.core.matting import Matting
from muse2api.core.media import MediaStore
from muse2api.drivers.base import MuseDriver
from muse2api.drivers.browser.interactive import InteractiveBrowserManager
from muse2api.services.container import Services
from muse2api.services.gateway import Gateway
from muse2api.services.request_log import RequestLog
from muse2api.services.tasks import TaskManager


class DummyDriver(MuseDriver):
    name = "dummy"

    def __init__(self):
        self._busy_accounts = set()

    def is_account_busy(self, account_id: str) -> bool:
        return account_id in self._busy_accounts

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

    with patch.object(test_services.interactive, "_run_login_session", new_callable=AsyncMock) as mock_run:
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

    with patch.object(test_services.interactive, "_run_view_session", new_callable=AsyncMock) as mock_run:
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
