"""Browser driver: operates the muse.ai web client in headless Chromium over CDP.

Design notes
------------
* One Chromium process; each account gets its own *browser context* (isolated
  cookie jar), so switching accounts never requires clearing cookies.
* One tab per account, reused across requests. Per-account serialisation is
  guaranteed by the account pool (``MUSE2API_ACCOUNT_MAX_CONCURRENCY=1``).
* A follow-up from the same user stays on that tab when the new ``messages``
  continue the conversation already on the page. Anything else opens a fresh
  thread, so two users never share a page.
"""

from __future__ import annotations

import asyncio
import base64
import contextlib
import logging
import socket
import time
import weakref
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit

from ...accounts.model import Account, mask_proxy_url
from ...config import Settings
from ...errors import (
    NotFound,
    UpstreamAuthError,
    UpstreamError,
    UpstreamTimeout,
    UpstreamVmDisabled,
)
from ...upstream import muse
from ..base import (
    ChatRequest,
    DriverCapabilities,
    InputImage,
    MuseDriver,
)
from . import dom
from .cdp import CDPError, CDPSession
from .chromium import ChromiumProcess, _kill_stale_browsers, find_chromium
from .generation_mixin import GenerationMixin
from .media_verify import (
    AccountBusy as AccountBusy,
)
from .media_verify import (
    _infer_mime_from_bytes as _infer_mime_from_bytes,
)
from .media_verify import (
    is_thread_url as is_thread_url,
)
from .media_verify import (
    quota_cache_seconds as quota_cache_seconds,
)
from .media_verify import (
    resolve_account_profile_dir as resolve_account_profile_dir,
)
from .media_verify import (
    resolve_proxy as resolve_proxy,
)
from .media_verify import (
    verify_and_save_download as verify_and_save_download,
)
from .media_verify import (
    verify_magic_bytes as verify_magic_bytes,
)
from .proxy_relay import mask_proxy
from .quota_mixin import QuotaSessionMixin

log = logging.getLogger(__name__)


def _is_port_in_use(port: int) -> bool:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.settimeout(0.1)
        return s.connect_ex(("127.0.0.1", port)) == 0


_USED_PORTS: set[int] = set()
_PORT_OWNERS: weakref.WeakValueDictionary[int, BrowserDriver] = weakref.WeakValueDictionary()
_UNSET = object()


def allocate_cdp_port(base_port: int) -> int:
    for port in range(base_port, base_port + 200):
        if port not in _USED_PORTS and not _is_port_in_use(port):
            _USED_PORTS.add(port)
            return port
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        p = int(s.getsockname()[1])
    _USED_PORTS.add(p)
    return p


def release_cdp_port(port: int) -> None:
    _USED_PORTS.discard(port)
    _PORT_OWNERS.pop(port, None)

POLL_INTERVAL = 0.15
STABLE_POLLS_DONE = 4
STABLE_POLLS_FORCE = 12
# Past this many assistant bubbles the page gets slow and a new thread is cheaper.
HOT_BUBBLE_LIMIT = 16
# A new thread often shows "Connecting..." for 10-30s and then answers normally.
STALL_GIVE_UP = 45.0
QUOTA_CACHE_SECONDS = 1800.0


@dataclass
class _Tab:
    account_id: str
    context_id: str
    target_id: str
    session: CDPSession
    hint: str | None = None
    turns: list[tuple[str, str]] = field(default_factory=list)
    image_count: int = 0
    thread_url: str = ""
    quota_note: str = ""
    """Last ignored quota-hint snippet, so a page polled every 0.6s logs it once."""
    warm: bool = False
    """The page shows a started thread holding only the warm-up exchange."""
    warming: bool = False
    """Checked out by background ``_fill_warm``, not a user/API request."""
    busy: bool = False
    stale: bool = False
    """Cookies were renewed; close this tab once it is idle."""
    closed: bool = False

    @property
    def has_state(self) -> bool:
        return bool(self.turns or self.thread_url)


class BrowserDriver(GenerationMixin, QuotaSessionMixin, MuseDriver):
    name = "browser"
    capabilities = DriverCapabilities(
        chat=True, chat_images=True, image=True, image_edit=True, video=True, renew_session=True,
        quota=True,
    )

    def __init__(
        self,
        settings: Settings,
        *,
        account: Account | None = None,
        port: int | None = None,
        profile_dir: Path | None = None,
        proxy: str | None = None,
    ) -> None:
        self.settings = settings
        self.account = account
        self.account_id = account.id if account else None
        self.port = port or settings.cdp_port
        self.profile_dir = profile_dir or (
            resolve_account_profile_dir(settings, account.id) if account else settings.profile_dir
        )
        self.proxy = proxy if proxy is not None else resolve_proxy(account, settings.browser_proxy)

        self._chromium: ChromiumProcess | None = None
        self._browser: CDPSession | None = None
        # One browser context (cookie jar) per account, holding up to
        # account_max_concurrency tabs; each request checks one tab out.
        self._contexts: dict[str, str] = {}
        self._tabs: dict[str, list[_Tab]] = {}
        self._opening: dict[str, int] = {}
        self._opening_warming: dict[str, int] = {}
        self._tab_cond = asyncio.Condition()
        self._ctx_lock = asyncio.Lock()
        # Last quota snapshot per account (see ``quota``) and the lock serialising reads.
        self._quota_cache: dict[str, dict[str, Any]] = {}
        self._quota_locks: dict[str, asyncio.Lock] = {}
        self._video_locks: dict[str, asyncio.Lock] = {}
        self._warming: dict[str, asyncio.Task] = {}
        self._recovery_lock = asyncio.Lock()
        self._coordinator_ready = False
        self._closed = False
        self._port_reserved = False
        self._account_lookup: Callable[[str], Account | None] | None = None
        self._account_blocked_cb: Callable[[str], bool] | None = None

        # Multi-driver registry per account (when self.account is None)
        self._account_drivers: dict[str, BrowserDriver] = {}
        self._driver_locks: dict[str, asyncio.Lock] = {}
        self._prewarm_tasks: dict[str, asyncio.Task] = {}

    def set_account_guards(
        self,
        *,
        lookup: Callable[[str], Account | None] | None = None,
        is_blocked: Callable[[str], bool] | None = None,
    ) -> None:
        self._account_lookup = lookup
        self._account_blocked_cb = is_blocked
        for drv in self._account_drivers.values():
            drv.set_account_guards(lookup=lookup, is_blocked=is_blocked)

    @property
    def account_drivers(self) -> dict[str, BrowserDriver]:
        return self._account_drivers

    async def _ensure_account_driver(self, account: Account) -> BrowserDriver:
        """Async per-account driver lookup with serialized proxy replacement and clean startup."""
        if self.account is not None:
            return self
        lock = self._driver_locks.setdefault(account.id, asyncio.Lock())
        async with lock:
            if self._closed:
                raise UpstreamError("browser driver is closed")
            if self._account_lookup is not None and self._account_lookup(account.id) is None:
                stale_drv = self._account_drivers.pop(account.id, None)
                if stale_drv is not None:
                    await stale_drv.shutdown()
                raise NotFound(f"account '{account.id}' not found")
            expected_proxy = resolve_proxy(account, self.settings.browser_proxy)
            drv = self._account_drivers.get(account.id)
            if drv is not None and drv.proxy != expected_proxy:
                log.info(
                    "account %s proxy changed from '%s' to '%s'; shutting down old driver",
                    account.id,
                    mask_proxy(drv.proxy),
                    mask_proxy(expected_proxy),
                )
                self._account_drivers.pop(account.id, None)
                await drv.shutdown()
                drv = None
            if drv is None:
                port = allocate_cdp_port(self.settings.cdp_port + len(self._account_drivers) + 1)
                pdir = resolve_account_profile_dir(self.settings, account.id)
                drv = BrowserDriver(
                    self.settings,
                    account=account,
                    port=port,
                    profile_dir=pdir,
                    proxy=expected_proxy,
                )
                _PORT_OWNERS[port] = drv
                drv._port_reserved = True
                drv.set_account_guards(
                    lookup=self._account_lookup,
                    is_blocked=self._account_blocked_cb,
                )
                self._account_drivers[account.id] = drv
            return drv

    async def release_account_driver(
        self,
        account_id: str,
        *,
        only_if_proxy_mismatch: str | None | object = _UNSET,
    ) -> None:
        """Shut down and remove an account's sub-driver (e.g. on account deletion or proxy mismatch)."""
        if self.account is not None:
            if self.account_id == account_id:
                if (
                    only_if_proxy_mismatch is not _UNSET
                    and self.proxy == (only_if_proxy_mismatch or "")
                ):
                    return
                await self.shutdown()
            return
        if (
            only_if_proxy_mismatch is not _UNSET
            and (existing := self._account_drivers.get(account_id)) is not None
            and existing.proxy == (only_if_proxy_mismatch or "")
        ):
            return
        task = self._prewarm_tasks.pop(account_id, None)
        if task and not task.done():
            task.cancel()
            with contextlib.suppress(asyncio.CancelledError, Exception):
                await task
        lock = self._driver_locks.setdefault(account_id, asyncio.Lock())
        async with lock:
            task2 = self._prewarm_tasks.pop(account_id, None)
            if task2 and not task2.done():
                task2.cancel()
                with contextlib.suppress(asyncio.CancelledError, Exception):
                    await task2
            drv = self._account_drivers.get(account_id)
            if drv is None:
                self._quota_cache.pop(account_id, None)
                return
            if (
                only_if_proxy_mismatch is not _UNSET
                and drv.proxy == (only_if_proxy_mismatch or "")
            ):
                return
            warm_task = drv._warming.pop(account_id, None)
            if warm_task and not warm_task.done():
                warm_task.cancel()
                with contextlib.suppress(asyncio.CancelledError, Exception):
                    await warm_task
            if drv.is_account_busy(account_id):
                log.warning(
                    "skipping release_account_driver for busy account %s",
                    account_id,
                )
                return
            self._account_drivers.pop(account_id, None)
            await drv.shutdown()
        self._quota_cache.pop(account_id, None)

    async def mark_account_stale(self, account_id: str) -> None:
        """Mark all open tabs of an account stale and close idle ones (e.g. after cookie PATCH)."""
        if self.account is None:
            drv = self._account_drivers.get(account_id)
            if drv is not None:
                await drv.mark_account_stale(account_id)
            self._quota_cache.pop(account_id, None)
            return
        self._quota_cache.pop(account_id, None)
        async with self._tab_cond:
            tabs = list(self._tabs.get(account_id, []))
            for tab in tabs:
                tab.stale = True
            idle = [t for t in tabs if not t.busy]
        for tab in idle:
            await self._close_tab(tab)

    def get_account_driver(self, account: Account) -> BrowserDriver:
        if self.account is not None:
            return self
        expected_proxy = resolve_proxy(account, self.settings.browser_proxy)
        drv = self._account_drivers.get(account.id)
        if drv is not None and drv.proxy != expected_proxy:
            log.info("account %s proxy changed from '%s' to '%s'; replacing driver",
                     account.id, mask_proxy(drv.proxy), mask_proxy(expected_proxy))
            asyncio.create_task(drv.shutdown())
            drv = None
        if drv is None:
            port = allocate_cdp_port(self.settings.cdp_port + len(self._account_drivers) + 1)
            pdir = resolve_account_profile_dir(self.settings, account.id)
            drv = BrowserDriver(
                self.settings,
                account=account,
                port=port,
                profile_dir=pdir,
                proxy=expected_proxy,
            )
            _PORT_OWNERS[port] = drv
            drv._port_reserved = True
            drv.set_account_guards(
                lookup=self._account_lookup,
                is_blocked=self._account_blocked_cb,
            )
            self._account_drivers[account.id] = drv
        return drv

    # ------------------------------------------------------------ lifecycle
    @property
    def browser_alive(self) -> bool:
        if self._browser is not None and not self._browser.closed:
            return True
        if self.account is None and self._account_drivers:
            return any(d.browser_alive for d in self._account_drivers.values())
        return False

    async def _recover_browser(self) -> None:
        async with self._recovery_lock:
            if self._closed or self.browser_alive:
                return
            log.warning("CDP disconnect detected on driver (account=%s); recovering browser and restarting Chromium...",
                        self.account_id or "default")
            current_task = asyncio.current_task()
            warming_tasks = [t for t in self._warming.values() if t is not current_task]
            for task in warming_tasks:
                task.cancel()
            if warming_tasks:
                await asyncio.gather(*warming_tasks, return_exceptions=True)
            if current_task is not None:
                self._warming = {k: t for k, t in self._warming.items() if t is current_task}
            else:
                self._warming.clear()

            async with self._tab_cond:
                tabs_to_close = [t for tabs in self._tabs.values() for t in tabs]
                for t in tabs_to_close:
                    t.closed = True
                self._tabs.clear()
                self._contexts.clear()
                self._tab_cond.notify_all()
            for t in tabs_to_close:
                with contextlib.suppress(Exception):
                    await t.session.close()

            if self._browser:
                with contextlib.suppress(Exception):
                    await self._browser.close()
                self._browser = None

            if self._chromium:
                with contextlib.suppress(Exception):
                    await self._chromium.stop()
                self._chromium = None

            killed = await asyncio.to_thread(_kill_stale_browsers, self.profile_dir, self.port)
            if killed:
                await asyncio.sleep(1.5)

            for lock in ("SingletonLock", "SingletonCookie", "DevToolsActivePort",
                         "SingletonSocket", "lockfile"):
                with contextlib.suppress(FileNotFoundError, OSError):
                    (self.profile_dir / lock).unlink()

            deadline = time.monotonic() + 10.0
            while time.monotonic() < deadline:
                if not _is_port_in_use(self.port):
                    break
                await asyncio.sleep(0.2)

            if self._closed:
                return
            await self.startup()
            if self._closed:
                await self.shutdown()
                return
            log.info("browser recovery complete on driver (account=%s)", self.account_id or "default")

    async def startup(self) -> None:
        if self._closed:
            raise UpstreamError("browser driver is closed")
        exe = find_chromium(self.settings.chromium_path)
        if self.account is None:
            # Root coordinator delegates every request to per-account sub-drivers;
            # avoid spawning an unused default Chromium on data/browser-profile.
            self._coordinator_ready = True
            log.info(
                "browser driver coordinator ready (lazy per-account browsers, chromium=%s)",
                exe,
            )
            return
        owner = _PORT_OWNERS.get(self.port)
        if (owner is not None and owner is not self and not owner._closed) or (
            (self.port not in _USED_PORTS or not self._port_reserved) and _is_port_in_use(self.port)
        ):
            self.port = allocate_cdp_port(self.port + 1)
        _USED_PORTS.add(self.port)
        _PORT_OWNERS[self.port] = self
        self._port_reserved = True
        chromium = ChromiumProcess(
            exe, self.port, self.profile_dir, self.settings.headless,
            proxy=self.proxy,
        )
        self._chromium = chromium
        try:
            ws_url = await chromium.start()
            if chromium.port != self.port:
                if _PORT_OWNERS.get(self.port) is self:
                    release_cdp_port(self.port)
                self.port = chromium.port
                _USED_PORTS.add(self.port)
                _PORT_OWNERS[self.port] = self
                self._port_reserved = True
            if self._closed:
                raise UpstreamError("browser driver is closed")
            self._browser = await CDPSession.connect(ws_url)
        except BaseException:
            try:
                if self._browser is not None:
                    with contextlib.suppress(Exception):
                        await self._browser.close()
                    self._browser = None
                with contextlib.suppress(Exception):
                    await chromium.stop()
            finally:
                if self._chromium is chromium:
                    self._chromium = None
            raise
        log.info("browser driver ready (account=%s, profile=%s, proxy=%s, port=%d)",
                 self.account_id or "default", self.profile_dir, mask_proxy(self.proxy), self.port)

    async def shutdown(self) -> None:
        self._closed = True
        try:
            if self.account is None:
                prewarms = list(self._prewarm_tasks.values())
                for task in prewarms:
                    task.cancel()
                if prewarms:
                    await asyncio.gather(*prewarms, return_exceptions=True)
                self._prewarm_tasks.clear()
                for d in list(self._account_drivers.values()):
                    with contextlib.suppress(Exception):
                        await d.shutdown()
                self._account_drivers.clear()
            warming_tasks = list(self._warming.values())
            for task in warming_tasks:
                task.cancel()
            if warming_tasks:
                await asyncio.gather(*warming_tasks, return_exceptions=True)
            self._warming.clear()
            for tabs in list(self._tabs.values()):
                for tab in list(tabs):
                    with contextlib.suppress(Exception):
                        await self._close_tab(tab)
            if self._browser:
                with contextlib.suppress(Exception):
                    await self._browser.close()
                self._browser = None
        finally:
            if self._chromium:
                with contextlib.suppress(Exception):
                    await self._chromium.stop()
                self._chromium = None
            if self._port_reserved or self.account is not None or self.port != self.settings.cdp_port:
                owner = _PORT_OWNERS.get(self.port)
                if owner is None or owner is self:
                    release_cdp_port(self.port)
            self._port_reserved = False

    async def health(self) -> dict[str, Any]:
        if self.account is None and self._account_drivers:
            sub_drivers = list(self._account_drivers.items())
            sub_healths = [await d.health() for _, d in sub_drivers]
            alive = any(h.get("browser_alive", False) for h in sub_healths)
            tabs = sum(h.get("tabs", 0) for h in sub_healths)
            busy = sum(h.get("busy_tabs", 0) for h in sub_healths)
            active_acc = None
            active_prx = None
            for acc_id, drv in sub_drivers:
                if drv.is_account_busy(acc_id):
                    active_acc = acc_id
                    active_prx = drv.proxy
                    break
            return {
                "driver": self.name,
                "ok": alive or self.browser_alive or getattr(self, "_coordinator_ready", False),
                "browser_alive": alive or self.browser_alive,
                "tabs": tabs + len([t for ts in self._tabs.values() for t in ts]),
                "busy_tabs": busy + sum(t.busy for ts in self._tabs.values() for t in ts),
                "account_drivers": len(self._account_drivers),
                "active_account_id": active_acc,
                "active_proxy": mask_proxy_url(active_prx),
            }
        ok = self.browser_alive or getattr(self, "_coordinator_ready", False)
        tabs = [t for ts in self._tabs.values() for t in ts]
        has_active_req = any(t.busy and not getattr(t, "warming", False) for t in tabs)
        return {
            "driver": self.name,
            "ok": ok,
            "browser_alive": self.browser_alive,
            "tabs": len(tabs),
            "busy_tabs": sum(t.busy for t in tabs),
            "active_account_id": self.account_id if has_active_req else None,
            "active_proxy": mask_proxy_url(self.proxy) if has_active_req else None,
        }

    def is_account_busy(self, account_id: str) -> bool:
        if self.account is None and account_id in self._account_drivers:
            return self._account_drivers[account_id].is_account_busy(account_id)
        non_warming_opening = max(
            0,
            self._opening.get(account_id, 0) - self._opening_warming.get(account_id, 0),
        )
        if non_warming_opening > 0:
            return True
        if (q_lock := self._quota_locks.get(account_id)) and q_lock.locked():
            return True
        if (v_lock := self._video_locks.get(account_id)) and v_lock.locked():
            return True
        tabs = self._tabs.get(account_id, [])
        return any(
            getattr(t, "busy", False) and not getattr(t, "warming", False)
            for t in tabs
        )

    # ------------------------------------------------------------ tabs
    async def _checkout(
        self,
        account: Account,
        prefer: Callable[[_Tab], bool] | None = None,
        *,
        warming: bool = False,
    ) -> _Tab:
        """Reserve an idle tab of ``account``, opening a new one while under the
        per-account limit. An idle tab matching ``prefer`` wins; without one, a new
        tab beats reusing (and so wiping) a tab that holds another conversation.
        Pair every call with ``_checkin``."""
        limit = max(1, self.settings.account_max_concurrency)
        dead: list[_Tab] = []
        async with self._tab_cond:
            while True:
                if self._closed:
                    raise UpstreamError("browser driver is closed")
                tabs = self._tabs.setdefault(account.id, [])
                for t in [t for t in tabs
                          if t.closed or (not t.busy and (t.stale or t.session.closed))]:
                    tabs.remove(t)
                    dead.append(t)
                # A warm tab first: whoever needs a new thread gets a started one.
                idle = sorted((t for t in tabs if not t.busy), key=lambda t: not t.warm)
                can_open = len(tabs) + self._opening.get(account.id, 0) < limit
                match = next((t for t in idle if prefer is None or prefer(t)), None)
                if match or (idle and not can_open):
                    tab = match or next((t for t in idle if not t.has_state), idle[0])
                    tab.busy = True
                    tab.warming = warming
                    break
                if can_open:
                    self._opening[account.id] = self._opening.get(account.id, 0) + 1
                    if warming:
                        self._opening_warming[account.id] = (
                            self._opening_warming.get(account.id, 0) + 1
                        )
                    tab = None
                    break
                # The pool caps requests per account, so this only waits out a tab
                # that is still being closed.
                await self._tab_cond.wait()
        for t in dead:
            await self._close_tab(t)
        if tab:
            return tab
        try:
            tab = await self._open_tab(account)
        finally:
            await asyncio.shield(self._finish_open(account.id, tab, warming=warming))
        log.info("opened tab %d for account %s", len(self._tabs[account.id]), account.id)
        return tab

    async def _finish_open(
        self, account_id: str, tab: _Tab | None, *, warming: bool = False
    ) -> None:
        async with self._tab_cond:
            self._opening[account_id] = max(0, self._opening.get(account_id, 1) - 1)
            if warming:
                self._opening_warming[account_id] = max(
                    0, self._opening_warming.get(account_id, 1) - 1
                )
            if tab:
                tab.busy = True
                tab.warming = warming
                self._tabs.setdefault(account_id, []).append(tab)
            self._tab_cond.notify_all()

    async def _checkin(self, tab: _Tab) -> None:
        # Free the tab before taking the lock: a request cancelled while waiting for
        # the lock (client gone) must not leave its tab marked busy for good.
        tab.busy = False
        tab.warming = False
        await asyncio.shield(self._notify_tabs())
        if tab.stale:
            await self._close_tab(tab)

    async def _notify_tabs(self) -> None:
        async with self._tab_cond:
            self._tab_cond.notify_all()

    async def _context(self, account: Account) -> str:
        async with self._ctx_lock:
            if self._closed:
                raise UpstreamError("browser driver is closed")
            if ctx := self._contexts.get(account.id):
                return ctx
            for attempt in (1, 2):
                if self._closed:
                    raise UpstreamError("browser driver is closed")
                if not self._browser or self._browser.closed:
                    await self._recover_browser()
                if self._closed:
                    raise UpstreamError("browser driver is closed")
                if not self._browser or self._browser.closed:
                    raise CDPError("browser is unavailable after recovery")
                try:
                    res = await self._browser.send("Target.createBrowserContext", {"disposeOnDetach": False})
                    self._contexts[account.id] = res["browserContextId"]
                    return res["browserContextId"]
                except CDPError as exc:
                    log.warning("CDP error in _context (attempt %d): %s", attempt, exc)
                    if attempt == 1 and not self._closed:
                        await self._recover_browser()
                    else:
                        raise
            raise RuntimeError("unreachable in _context")

    async def _open_tab(self, account: Account) -> _Tab:
        if self._closed:
            raise UpstreamError("browser driver is closed")
        if missing := muse.missing_session_cookies(account.cookies):
            raise UpstreamAuthError(f"account is missing session cookies: {', '.join(missing)}")
        for attempt in (1, 2):
            if self._closed:
                raise UpstreamError("browser driver is closed")
            if not self._browser or self._browser.closed:
                if self._chromium is None and self._browser is None:
                    async with self._recovery_lock:
                        if self._closed:
                            raise UpstreamError("browser driver is closed")
                        if self._chromium is None and self._browser is None:
                            await self.startup()
                if self._closed:
                    raise UpstreamError("browser driver is closed")
                if not self._browser or self._browser.closed:
                    await self._recover_browser()
            if self._closed:
                raise UpstreamError("browser driver is closed")
            if not self._browser or not self._chromium:
                raise CDPError("browser is unavailable after recovery")
            target_id: str | None = None
            session: CDPSession | None = None
            try:
                context_id = await self._context(account)
                # A window per tab: Chromium throttles background tabs of a shared window,
                # which made parallel requests on one account run one after another.
                target = await self._browser.send(
                    "Target.createTarget",
                    {"url": "about:blank", "browserContextId": context_id, "newWindow": True},
                )
                target_id = target["targetId"]
                session = await CDPSession.connect(
                    f"ws://127.0.0.1:{self._chromium.port}/devtools/page/{target_id}"
                )
                for domain in ("Page", "Runtime", "Network"):
                    await session.send(f"{domain}.enable")
                await session.send("Emulation.setFocusEmulationEnabled", {"enabled": True})
                await session.send("Page.setWebLifecycleState", {"state": "active"})
                await self._set_cookies(session, account)
                if self._closed:
                    raise UpstreamError("browser driver is closed")
                return _Tab(account.id, context_id, target_id, session)
            except CDPError as exc:
                if session is not None:
                    with contextlib.suppress(Exception):
                        await session.close()
                if target_id is not None and self._browser and not self._browser.closed:
                    with contextlib.suppress(Exception):
                        await self._browser.send("Target.closeTarget", {"targetId": target_id})
                log.warning("CDP error in _open_tab (attempt %d): %s", attempt, exc)
                if attempt == 1 and not self._closed:
                    await self._recover_browser()
                else:
                    raise
            except BaseException:
                if session is not None:
                    with contextlib.suppress(Exception):
                        await session.close()
                if target_id is not None and self._browser and not self._browser.closed:
                    with contextlib.suppress(Exception):
                        await self._browser.send("Target.closeTarget", {"targetId": target_id})
                raise
        raise RuntimeError("unreachable in _open_tab")

    async def _close_tab(self, tab: _Tab) -> None:
        """Idempotent; marks the tab closed first so a cancelled close never leaves it
        counted as open or busy."""
        if tab.closed:
            return
        tab.closed, tab.busy, tab.warming = True, False, False
        await asyncio.shield(self._dispose_tab(tab))

    async def _dispose_tab(self, tab: _Tab) -> None:
        async with self._tab_cond:
            tabs = self._tabs.get(tab.account_id, [])
            if tab in tabs:
                tabs.remove(tab)
            last = not tabs and not self._opening.get(tab.account_id)
            context_id = self._contexts.pop(tab.account_id, None) if last else None
            self._tab_cond.notify_all()
        await tab.session.close()
        if self._browser and not self._browser.closed:
            with contextlib.suppress(Exception):
                await self._browser.send("Target.closeTarget", {"targetId": tab.target_id})
            if context_id:
                with contextlib.suppress(Exception):
                    await self._browser.send(
                        "Target.disposeBrowserContext", {"browserContextId": context_id}
                    )

    @staticmethod
    async def _set_cookies(session: CDPSession, account: Account) -> None:
        floor = time.time() + 3600
        for name, value in account.cookies.items():
            if not value:
                continue
            exp = account.cookie_expires.get(name) or 0
            params = {
                "name": name,
                "value": value,
                "domain": ".muse.ai",
                "path": "/",
                "secure": True,
                # Never hand Chromium a past expiry; it would drop the cookie silently.
                "expires": exp if exp > floor else time.time() + 7 * 86400,
            }
            if name.startswith("__Host-"):
                # Chromium rejects __Host- cookies that carry a Domain attribute.
                params.pop("domain")
                params["url"] = "https://muse.ai/"
            await session.send("Network.setCookie", params)

    @staticmethod
    def _forget(tab: _Tab) -> None:
        tab.warm = False
        tab.hint = None
        tab.turns = []
        tab.image_count = 0
        tab.thread_url = ""

    def _load_hot(self, tab: _Tab, account: Account) -> None:
        """Restore the last conversation after a restart or a new browser tab."""
        if tab.turns or tab.thread_url:
            return
        hot = account.meta.get("hot_page")
        if not isinstance(hot, dict):
            return
        turns = []
        for item in hot.get("turns") or []:
            if isinstance(item, (list, tuple)) and len(item) == 2:
                turns.append((str(item[0]), str(item[1])))
        tab.turns = turns
        tab.hint = hot.get("hint") or None
        tab.thread_url = str(hot.get("url") or "")
        try:
            tab.image_count = int(hot.get("image_count") or 0)
        except (TypeError, ValueError):
            tab.image_count = 0

    def _save_hot(self, tab: _Tab, account: Account, req: ChatRequest, url: str) -> None:
        if url:
            tab.thread_url = url
        tab.hint = req.conversation_hint
        tab.turns = list(req.turns)
        tab.image_count = len(req.images)
        account.meta["hot_page"] = {
            "hint": req.conversation_hint or "",
            "turns": [list(turn) for turn in req.turns],
            "url": tab.thread_url,
            "image_count": tab.image_count,
        }

    async def _href(self, tab: _Tab) -> str:
        try:
            return (await tab.session.evaluate("location.href")) or ""
        except CDPError:
            return ""

    async def _on_thread(self, tab: _Tab) -> bool:
        """The open page is the saved conversation and can take another message."""
        href = await self._href(tab)
        if not is_thread_url(href):
            return False
        if tab.thread_url and urlsplit(href).path.rstrip("/") != urlsplit(tab.thread_url).path.rstrip("/"):
            return False
        try:
            page = await tab.session.evaluate(dom.PAGE_STATE) or {}
            if not page.get("hasInput"):
                return False
            if "Connecting..." in (page.get("head") or ""):
                return False
            state = await self._state(tab)
        except CDPError:
            return False
        if state.get("generating"):
            return False
        if any(hint in state.get("tail", "") for hint in dom.STALL_HINTS):
            return False
        return (state.get("agentCount") or 0) < HOT_BUBBLE_LIMIT

    _SOFT_NAV_TIMEOUT = 8.0

    async def _soft_new_thread(self, tab: _Tab) -> bool:
        """Switch an already loaded muse.ai page to a blank thread without reloading it.
        False means the caller should fall back to a full navigation."""
        with contextlib.suppress(CDPError, TimeoutError):
            if not await tab.session.evaluate(dom.SOFT_NEW_THREAD, timeout=5.0):
                return False
            deadline = time.monotonic() + self._SOFT_NAV_TIMEOUT
            while time.monotonic() < deadline:
                await asyncio.sleep(0.25)
                if await tab.session.evaluate(dom.FRESH_THREAD, timeout=5.0):
                    return True
        return False

    async def _maybe_retry_error_nav(
        self,
        tab: _Tab,
        *,
        href: Any,
        target_url: str,
        nav_retries: int,
        max_nav_retries: int,
        deadline: float,
        retry_margin: float,
        label: str,
    ) -> tuple[bool, int]:
        """Re-navigate via ``about:blank`` when a transient proxy stall leaves ``chrome-error://``."""
        if (
            isinstance(href, str)
            and href.startswith("chrome-error://")
            and nav_retries < max_nav_retries
            and (deadline - time.monotonic()) > retry_margin
        ):
            nav_retries += 1
            log.warning(
                "%s landed on browser error page (%s); retrying (%d/%d)",
                label,
                href,
                nav_retries,
                max_nav_retries,
            )
            await asyncio.sleep(1.0)
            await tab.session.send("Page.navigate", {"url": "about:blank"})
            await tab.session.send("Page.navigate", {"url": target_url})
            return True, nav_retries
        return False, nav_retries

    async def _new_thread(self, tab: _Tab) -> None:
        self._forget(tab)
        if await self._soft_new_thread(tab):
            return
        await tab.session.send("Page.navigate", {"url": muse.NEW_THREAD_URL})
        deadline = time.monotonic() + self.settings.page_ready_timeout
        nav_retries = 0
        max_nav_retries = 2
        retry_margin = min(5.0, self.settings.page_ready_timeout * 0.25)
        state: dict = {}
        while time.monotonic() < deadline:
            if getattr(tab.session, "closed", False) is True:
                raise CDPError("cdp connection closed")
            await asyncio.sleep(0.25)
            if getattr(tab.session, "closed", False) is True:
                raise CDPError("cdp connection closed")
            # A busy page can leave Runtime.evaluate unanswered while it loads; poll again.
            try:
                state = await tab.session.evaluate(dom.PAGE_STATE, timeout=5.0) or {}
            except CDPError:
                if getattr(tab.session, "closed", False) is True:
                    raise
                continue
            except TimeoutError as exc:
                if getattr(tab.session, "closed", False) is True:
                    raise CDPError("cdp connection closed") from exc
                continue
            if state.get("vmDisabled"):
                raise UpstreamVmDisabled(
                    "muse.ai disconnected this account's workspace (vm_state=DISABLED); "
                    "it needs a support ticket on muse.ai to reconnect")
            if state.get("ready"):
                return
            retried, nav_retries = await self._maybe_retry_error_nav(
                tab,
                href=state.get("href") or "",
                target_url=muse.NEW_THREAD_URL,
                nav_retries=nav_retries,
                max_nav_retries=max_nav_retries,
                deadline=deadline,
                retry_margin=retry_margin,
                label="muse.ai navigation",
            )
            if retried:
                continue
        head = (state.get("head") or "").lower()
        if any(h in head for h in dom.LOGIN_HINTS):
            raise UpstreamAuthError("muse.ai redirected to login; cookies are no longer valid")
        raise UpstreamTimeout("muse.ai page did not become ready")

    # ------------------------------------------------------------ input
    # The textarea mounts ~0.5s before the hidden file input on a fresh thread;
    # attaching immediately fails roughly every third time on a healthy account.
    _ATTACH_READY_TIMEOUT = 3.0
    _UPLOAD_TIMEOUT = 45.0
    _UPLOAD_POLL_INTERVAL = 0.2

    async def _wait_upload_complete(self, tab: _Tab, timeout: float = 45.0) -> None:
        """Wait for reference image upload to complete before sending (P1-6).

        Polls every 200ms checking for [role=progressbar], [aria-busy=true] or upload indicators.
        Times out after 45s and raises an error if upload doesn't finish, preventing sending prompt without image.
        """
        deadline = time.monotonic() + timeout
        await asyncio.sleep(self._UPLOAD_POLL_INTERVAL)
        while time.monotonic() < deadline:
            is_busy = await tab.session.evaluate(dom.CHECK_UPLOAD_BUSY)
            if not is_busy:
                await asyncio.sleep(0.1)
                is_busy_confirm = await tab.session.evaluate(dom.CHECK_UPLOAD_BUSY)
                if not is_busy_confirm:
                    return
            await asyncio.sleep(self._UPLOAD_POLL_INTERVAL)

        is_still_busy = await tab.session.evaluate(dom.CHECK_UPLOAD_BUSY)
        if is_still_busy:
            raise UpstreamTimeout(
                f"reference image upload timed out after {timeout:.0f}s: upload progressbar still active"
            )

    async def _attach(self, tab: _Tab, images: list[InputImage]) -> None:
        if images:
            # Wait for the file input to mount before the first attach attempt.
            deadline = time.monotonic() + self._ATTACH_READY_TIMEOUT
            while time.monotonic() < deadline:
                page = await tab.session.evaluate(dom.PAGE_STATE) or {}
                if page.get("hasFileInput"):
                    break
                await asyncio.sleep(0.15)
        for idx, img in enumerate(images):
            ext = img.mime.split("/")[-1].replace("jpeg", "jpg")
            res = await tab.session.evaluate(
                dom.attach_file(base64.b64encode(img.data).decode(), img.mime, f"input_{idx}.{ext}")
            )
            if not (res or {}).get("ok"):
                raise UpstreamError(f"failed to attach image: {(res or {}).get('err')}")
        if images:
            await self._wait_upload_complete(tab, timeout=self._UPLOAD_TIMEOUT)

    async def _send(self, tab: _Tab, prompt: str, cancel: asyncio.Event | None = None) -> None:
        if cancel and cancel.is_set():
            raise asyncio.CancelledError
        res = await tab.session.evaluate(dom.fill_input(prompt))
        if not (res or {}).get("ok"):
            raise UpstreamError(f"cannot fill chat input: {(res or {}).get('err')}")
        if cancel and cancel.is_set():
            raise asyncio.CancelledError
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if cancel and cancel.is_set():
                raise asyncio.CancelledError
            if await tab.session.evaluate(dom.CLICK_SEND) == "clicked":
                return
            await asyncio.sleep(0.1)
        if cancel and cancel.is_set():
            raise asyncio.CancelledError
        for kind in ("keyDown", "keyUp"):
            if cancel and cancel.is_set():
                raise asyncio.CancelledError
            await tab.session.send(
                "Input.dispatchKeyEvent",
                {"type": kind, "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13,
                 **({"text": "\r"} if kind == "keyDown" else {})},
            )
        await asyncio.sleep(0.5)
        if not await tab.session.evaluate(dom.INPUT_EMPTY):
            raise UpstreamError("send button not found and Enter did not submit")

