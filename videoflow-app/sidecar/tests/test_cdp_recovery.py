"""Tests for CDP Reconnect and Browser Recovery (Issue #6 [P0-1])."""

from __future__ import annotations

import asyncio
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from muse2api.accounts.model import Account
from muse2api.accounts.pool import AccountPool
from muse2api.config import Settings
from muse2api.drivers.browser.cdp import CDPError
from muse2api.drivers.browser.driver import BrowserDriver
from muse2api.errors import UpstreamError
from muse2api.services.gateway import Gateway


class _MemStore:
    def __init__(self, accounts: list[Account] | None = None) -> None:
        self._items = accounts or []

    async def load(self) -> list[Account]:
        return list(self._items)

    async def save(self, accounts: list[Account]) -> None:
        self._items = list(accounts)


@pytest.fixture
def settings(tmp_path: Path) -> Settings:
    s = Settings(data_dir=tmp_path, cdp_port=19210)
    s.profile_dir.mkdir(parents=True, exist_ok=True)
    return s


@pytest.fixture
def dummy_account() -> Account:
    return Account(
        id="acc_1",
        label="Test Account",
        cookies={
            "hatch_sess": "dummy_session",
            "cf_clearance": "dummy_clearance",
            "hatch_native_auth_device": "dummy_device",
        },
    )


@pytest.mark.asyncio
async def test_browser_health_and_browser_alive_flag(settings: Settings):
    driver = BrowserDriver(settings)
    assert driver.browser_alive is False
    h = await driver.health()
    assert h["ok"] is False
    assert h["browser_alive"] is False

    mock_browser = MagicMock()
    mock_browser.closed = False
    driver._browser = mock_browser

    assert driver.browser_alive is True
    h2 = await driver.health()
    assert h2["ok"] is True
    assert h2["browser_alive"] is True


@pytest.mark.asyncio
async def test_recover_browser_cleans_locks_and_calls_startup(settings: Settings):
    driver = BrowserDriver(settings)

    # Create dummy lock files
    for lock in ("SingletonLock", "SingletonCookie", "DevToolsActivePort", "SingletonSocket", "lockfile"):
        (settings.profile_dir / lock).write_text("lock")

    driver._tabs["acc_1"] = [MagicMock()]
    driver._contexts["acc_1"] = "ctx_1"

    with patch("muse2api.drivers.browser.driver._kill_stale_browsers") as mock_kill, \
         patch("muse2api.drivers.browser.driver._is_port_in_use", return_value=False), \
         patch.object(driver, "startup", new_callable=AsyncMock) as mock_startup:

        await driver._recover_browser()

        assert mock_kill.called
        assert mock_startup.called
        assert len(driver._tabs) == 0
        assert len(driver._contexts) == 0

        # Check lock files removed
        for lock in ("SingletonLock", "SingletonCookie", "DevToolsActivePort", "SingletonSocket", "lockfile"):
            assert not (settings.profile_dir / lock).exists()


@pytest.mark.asyncio
async def test_context_recovers_on_cdperror(settings: Settings, dummy_account: Account):
    driver = BrowserDriver(settings)
    mock_browser = MagicMock()
    mock_browser.closed = False

    # First send fails with CDPError, second succeeds
    mock_browser.send = AsyncMock(side_effect=[CDPError("cdp connection closed"), {"browserContextId": "new_ctx"}])
    driver._browser = mock_browser

    with patch.object(driver, "_recover_browser", new_callable=AsyncMock) as mock_recover:
        ctx_id = await driver._context(dummy_account)
        assert ctx_id == "new_ctx"
        assert mock_recover.called


@pytest.mark.asyncio
async def test_open_tab_recovers_on_cdperror(settings: Settings, dummy_account: Account):
    driver = BrowserDriver(settings)
    mock_browser = MagicMock()
    mock_browser.closed = False
    driver._browser = mock_browser
    driver._chromium = MagicMock()
    driver._chromium.port = 19210

    call_count = 0

    async def mock_send(method, *args, **kwargs):
        nonlocal call_count
        call_count += 1
        if method == "Target.createTarget":
            if call_count == 1:
                raise CDPError("connection reset")
            return {"targetId": "t1"}
        return {}

    mock_browser.send = AsyncMock(side_effect=mock_send)

    mock_session = MagicMock()
    mock_session.send = AsyncMock(return_value={})

    with patch.object(driver, "_context", new_callable=AsyncMock, return_value="ctx_1"), \
         patch("muse2api.drivers.browser.driver.CDPSession.connect", new_callable=AsyncMock, return_value=mock_session), \
         patch.object(driver, "_set_cookies", new_callable=AsyncMock), \
         patch.object(driver, "_recover_browser", new_callable=AsyncMock) as mock_recover:

        tab = await driver._open_tab(dummy_account)
        assert tab.target_id == "t1"
        assert mock_recover.called


@pytest.mark.asyncio
async def test_gateway_retries_on_cdperror(dummy_account: Account):
    pool = AccountPool(_MemStore([dummy_account]), acquire_timeout=5.0)
    await pool.load()
    driver = MagicMock()
    gateway = Gateway(pool, driver, max_failover=2)

    call_count = 0

    async def flaky_call(acc):
        nonlocal call_count
        call_count += 1
        if call_count == 1:
            raise CDPError("cdp connection closed")
        return "success"

    res = await gateway._run(flaky_call)
    assert res == "success"
    assert call_count == 2


@pytest.mark.asyncio
async def test_admin_status_contains_browser_alive():
    from muse2api.api.routes.admin import status
    mock_svc = MagicMock()
    mock_svc.driver.health = AsyncMock(return_value={"ok": True, "driver": "browser"})
    mock_svc.driver.browser_alive = True
    mock_svc.driver.capabilities.__dict__ = {}
    mock_svc.pool.stats.return_value = {}

    res = await status(mock_svc)
    assert "browser_alive" in res
    assert res["browser_alive"] is True


@pytest.mark.asyncio
async def test_gateway_retries_on_upstream_caused_by_cdp(dummy_account: Account):
    pool = AccountPool(_MemStore([dummy_account]), acquire_timeout=5.0)
    await pool.load()
    driver = MagicMock()
    gateway = Gateway(pool, driver, max_failover=2)

    call_count = 0

    async def flaky_call(acc):
        nonlocal call_count
        call_count += 1
        if call_count == 1:
            err = UpstreamError("browser error: cdp closed")
            err.__cause__ = CDPError("cdp closed")
            raise err
        return "success"

    res = await gateway._run(flaky_call)
    assert res == "success"
    assert call_count == 2

