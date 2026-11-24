"""Session renewal and Settings -> Usage quota reader mixin for BrowserDriver."""

from __future__ import annotations

import asyncio
import contextlib
import logging
import sys
import time
from typing import TYPE_CHECKING, Any

from ...accounts.model import Account
from ...errors import UpstreamError
from ..base import SessionInfo
from . import dom
from .cdp import CDPError
from .media_verify import AccountBusy, quota_cache_seconds, resolve_proxy

if TYPE_CHECKING:
    from .driver import _Tab

log = logging.getLogger(__name__)


def _drv() -> Any:
    return sys.modules["muse2api.drivers.browser.driver"]


class QuotaSessionMixin:
    # ------------------------------------------------------------ session
    async def renew_session(self, account: Account) -> SessionInfo:
        if self.account is None:
            drv = self._account_drivers.get(account.id)
            proxy = (drv.proxy if drv is not None else None) or resolve_proxy(
                account, self.settings.browser_proxy
            ) or None
            return await _drv().muse.renew_session(account.cookies, proxy=proxy)
        proxy = self.proxy or resolve_proxy(account, self.settings.browser_proxy) or None
        return await _drv().muse.renew_session(account.cookies, proxy=proxy)

    async def sync_renewed_cookies(self, account: Account) -> None:
        """Apply freshly committed cookies to already-running tabs of ``account``."""
        if self.account is None:
            drv = self._account_drivers.get(account.id)
            if drv is not None:
                await drv.sync_renewed_cookies(account)
            return
        async with self._tab_cond:
            live = [t for t in self._tabs.get(account.id, [])
                    if not (t.closed or t.stale or t.session.closed)]
        if live:
            with contextlib.suppress(CDPError, TimeoutError):
                await self._set_cookies(live[0].session, account)
                return
        # Otherwise retire the account's tabs so new ones load the rotated cookies; a
        # tab in the middle of a request is closed when it is checked back in.
        async with self._tab_cond:
            tabs = list(self._tabs.get(account.id, []))
            for tab in tabs:
                tab.stale = True
            idle = [t for t in tabs if not t.busy]
        for tab in idle:
            await self._close_tab(tab)

    # ------------------------------------------------------------ quota
    _QUOTA_CLICK_TIMEOUT = 8.0
    _QUOTA_READ_TIMEOUT = 12.0
    _ESCAPE_PAUSE = 0.1
    """Lets the UI finish closing before the next key or click."""

    def forget_quota(self, account_id: str) -> None:
        """Drop the cached snapshot so the next ``quota`` call reads the page again."""
        self._quota_cache.pop(account_id, None)
        if drv := self._account_drivers.get(account_id):
            drv._quota_cache.pop(account_id, None)

    def _cached_quota(self, account_id: str) -> dict[str, Any] | None:
        hit = self._quota_cache.get(account_id)
        if hit and time.time() - hit["fetched_at"] < quota_cache_seconds():
            return dict(hit)
        return None

    async def quota(self, account: Account) -> dict[str, Any]:
        """Read Settings -> Usage on an idle tab of ``account``.

        Returns ``plan``, ``weekly_used_pct``, ``resets_at``, ``extra_left``, ``found``,
        ``source`` ("browser") and ``fetched_at``. Cached per account for
        ``MUSE2API_QUOTA_CACHE_S`` (30 min) so the dashboard cannot hammer muse.ai;
        ``forget_quota`` bypasses it. Raises ``AccountBusy`` rather than touch a tab
        that is serving a request."""
        if self.account is None:
            drv = await self._ensure_account_driver(account)
            return await drv.quota(account)
        if hit := self._cached_quota(account.id):
            return hit
        async with self._quota_locks.setdefault(account.id, asyncio.Lock()):
            if hit := self._cached_quota(account.id):
                return hit
            tab = await self._checkout_idle(account)
            try:
                parsed = await self._read_quota(tab)
            except CDPError as exc:
                await self._close_tab(tab)
                raise UpstreamError(f"browser error: {exc}") from exc
            finally:
                await self._checkin(tab)
            snapshot = {**parsed, "source": "browser", "fetched_at": time.time()}
            self._quota_cache[account.id] = snapshot
            return dict(snapshot)

    async def _checkout_idle(self, account: Account) -> _Tab:
        """An idle tab of ``account`` without waiting; a fresh tab only when the account
        has none open or opening. Raises ``AccountBusy`` when every tab is in use."""
        async with self._tab_cond:
            tabs = [t for t in self._tabs.get(account.id, []) if not t.closed]
            idle = [t for t in tabs if not (t.busy or t.stale or t.session.closed)]
            if idle:
                # Prefer a tab without a conversation; the dialog does not navigate anyway.
                tab = next((t for t in idle if not t.has_state), idle[0])
                tab.busy = True
                tab.warming = False
                return tab
            non_warming_opening = max(
                0,
                self._opening.get(account.id, 0) - self._opening_warming.get(account.id, 0),
            )
            if any(t.busy and not getattr(t, "warming", False) for t in tabs) or non_warming_opening > 0:
                raise AccountBusy(f"account '{account.id}' is serving a request; "
                                  "try the quota again when it is idle")
        return await self._checkout(account, prefer=lambda t: not t.has_state)

    async def _click_js(self, tab: _Tab, expr: str) -> bool:
        """Click the centre of the element ``expr`` locates, waiting for it to appear."""
        deadline = _drv().time.monotonic() + self._QUOTA_CLICK_TIMEOUT
        while True:
            pt = await tab.session.evaluate(expr)
            if isinstance(pt, dict) and "x" in pt and "y" in pt:
                for kind in ("mousePressed", "mouseReleased"):
                    await tab.session.send("Input.dispatchMouseEvent", {
                        "type": kind, "x": pt["x"], "y": pt["y"], "button": "left",
                        "clickCount": 1})
                return True
            if _drv().time.monotonic() >= deadline:
                return False
            await _drv().asyncio.sleep(0.15)

    async def _escape(self, tab: _Tab) -> None:
        for _ in range(2):
            for kind in ("keyDown", "keyUp"):
                await tab.session.send("Input.dispatchKeyEvent", {
                    "type": kind, "key": "Escape", "code": "Escape",
                    "windowsVirtualKeyCode": 27, "nativeVirtualKeyCode": 27})
            await _drv().asyncio.sleep(self._ESCAPE_PAUSE)

    async def _read_quota(self, tab: _Tab) -> dict[str, Any]:
        page = await tab.session.evaluate(dom.PAGE_STATE) or {}
        if not page.get("hasInput"):  # a fresh tab is still on about:blank
            await self._new_thread(tab)
        await self._escape(tab)  # a menu left open would swallow the first click
        try:
            if not await self._click_js(tab, dom.QUOTA_SETTINGS_ENTRY):
                return {**dom.parse_usage_text(""), "error": "settings entry not found"}
            if not await self._click_js(tab, dom.QUOTA_MENU_ITEM):
                return {**dom.parse_usage_text(""), "error": "settings menu item not found"}
            deadline = _drv().time.monotonic() + self._QUOTA_READ_TIMEOUT
            while True:
                parsed = dom.parse_usage_text(await tab.session.evaluate(dom.QUOTA_DIALOG_TEXT))
                if parsed["found"] or _drv().time.monotonic() >= deadline:
                    return parsed
                await _drv().asyncio.sleep(0.2)
        finally:
            # Never hand a tab back with the dialog open: close it, and retire the
            # tab when that did not work.
            try:
                await self._escape(tab)
                if await tab.session.evaluate(dom.QUOTA_DIALOG_TEXT):
                    tab.stale = True
            except Exception:  # noqa: BLE001
                tab.stale = True
            if tab.stale:
                log.warning("settings dialog stayed open on a tab of %s; retiring it",
                            tab.account_id)
