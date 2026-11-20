"""Interactive headed browser sessions for logging in and viewing account threads."""

from __future__ import annotations

import asyncio
import contextlib
import inspect
import json
import logging
import socket
import time
import urllib.parse
from pathlib import Path
from typing import Any

import httpx

from ...accounts.model import Account, AccountStatus
from ...accounts.pool import AccountPool
from ...config import Settings
from .cdp import CDPSession
from .chromium import find_chromium
from .proxy_relay import LocalProxyRelay, needs_proxy_relay

log = logging.getLogger(__name__)


def find_free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return int(s.getsockname()[1])


def _is_muse_hostname(url_or_host: str) -> bool:
    """Return True only if ``url_or_host`` is a valid hostname or http(s) URL for ``muse.ai`` or ``*.muse.ai``."""
    if not url_or_host or not isinstance(url_or_host, str) or "\\" in url_or_host:
        return False
    raw = url_or_host.strip()
    if not raw:
        return False
    if "://" in raw:
        try:
            parsed = urllib.parse.urlsplit(raw)
        except Exception:  # noqa: BLE001
            return False
        if parsed.scheme not in ("http", "https"):
            return False
        if parsed.username is not None or parsed.password is not None or "@" in (parsed.netloc or ""):
            return False
        host_raw = parsed.hostname or ""
    else:
        if any(ch in raw for ch in ("/", "@", ":", "?", "#")):
            return False
        host_raw = raw
    if host_raw.startswith("."):
        return False
    host = host_raw.lower().rstrip(".")
    if not host or host.startswith(".") or ".." in host:
        return False
    return host == "muse.ai" or host.endswith(".muse.ai")


def _sanitize_view_url(url: str | None, fallback: str = "https://muse.ai/") -> str:
    """Validate and normalize a view target URL; fall back to ``fallback`` if unsafe."""
    if not url or not isinstance(url, str):
        return fallback
    cleaned = url.strip()
    if "\\" in cleaned:
        return fallback
    try:
        parsed = urllib.parse.urlsplit(cleaned)
    except Exception:  # noqa: BLE001
        return fallback
    if parsed.scheme != "https":
        return fallback
    if parsed.username is not None or parsed.password is not None or "@" in (parsed.netloc or ""):
        return fallback
    host_raw = parsed.hostname or ""
    if host_raw.startswith("."):
        return fallback
    host = host_raw.lower().rstrip(".")
    if not host or host.startswith(".") or ".." in host or not (host == "muse.ai" or host.endswith(".muse.ai")):
        return fallback
    return cleaned


async def _resolve_page_ws_url(
    port: int,
    proc: asyncio.subprocess.Process | None,
    *,
    attempts: int = 60,
    interval: float = 0.5,
) -> str | None:
    """Poll ``/json/list`` and return the ``webSocketDebuggerUrl`` of a ``page`` target."""
    list_url = f"http://127.0.0.1:{port}/json/list"
    async with httpx.AsyncClient(timeout=1.5, trust_env=False) as client:
        for _ in range(attempts):
            if proc is not None and proc.returncode is not None:
                return None
            try:
                r = await client.get(list_url)
                targets = r.json() if r.status_code == 200 else []
                if isinstance(targets, list):
                    pages = [
                        t
                        for t in targets
                        if isinstance(t, dict)
                        and t.get("type") == "page"
                        and isinstance(t.get("webSocketDebuggerUrl"), str)
                        and t.get("webSocketDebuggerUrl")
                    ]
                    if pages:
                        for p in pages:
                            if _is_muse_hostname(str(p.get("url", ""))):
                                return str(p["webSocketDebuggerUrl"])
                        return str(pages[0]["webSocketDebuggerUrl"])
            except Exception:  # noqa: BLE001
                pass
            await asyncio.sleep(interval)
    return None


async def _inject_cookies(cdp: CDPSession, account: Account) -> None:
    """Inject account cookies into the page target using ``url='https://muse.ai'`` and ``secure=True``."""
    now = time.time()
    expires_map = account.cookie_expires or {}
    for name, value in (account.cookies or {}).items():
        if not name or not value:
            continue
        param: dict[str, Any] = {
            "name": name,
            "value": value,
            "url": "https://muse.ai",
            "path": "/",
            "secure": True,
        }
        if not name.startswith("__Host-"):
            param["domain"] = ".muse.ai"
        exp = expires_map.get(name)
        if exp and float(exp) > now:
            param["expires"] = float(exp)
        with contextlib.suppress(Exception):
            await cdp.send("Network.setCookie", param)


async def _terminate_proc(proc: asyncio.subprocess.Process | None) -> None:
    if proc is None or proc.returncode is not None:
        return
    with contextlib.suppress(Exception):
        proc.terminate()
    try:
        await asyncio.wait_for(proc.wait(), timeout=5.0)
    except (asyncio.TimeoutError, Exception):  # noqa: BLE001
        with contextlib.suppress(Exception):
            proc.kill()
        with contextlib.suppress(Exception):
            await asyncio.wait_for(proc.wait(), timeout=2.0)


class InteractiveSession:
    def __init__(self, account_id: str, kind: str) -> None:
        self.account_id = account_id
        self.kind = kind  # "login" or "view"
        self.task: asyncio.Task | None = None
        self.proc: asyncio.subprocess.Process | None = None
        self.started_at: float = time.time()
        self.harvested: bool = False


class InteractiveBrowserManager:
    """Manages on-demand headed browser windows for manual login and thread viewing."""

    def __init__(self, settings: Settings, pool: AccountPool, driver: Any) -> None:
        self.settings = settings
        self.pool = pool
        self.driver = driver
        self._sessions: dict[str, InteractiveSession] = {}
        self._lock = asyncio.Lock()
        self._closed = False

    def is_account_busy(self, account_id: str) -> bool:
        if hasattr(self.pool, "is_inflight") and self.pool.is_inflight(account_id):
            return True
        if hasattr(self.driver, "is_account_busy") and self.driver.is_account_busy(account_id):
            return True
        return False

    def is_session_active(self, account_id: str) -> bool:
        sess = self._sessions.get(account_id)
        return sess is not None and sess.task is not None and not sess.task.done()

    async def open_login(self, account: Account) -> dict[str, Any]:
        async with self._lock:
            if self._closed:
                return {"ok": False, "message": "Hệ thống đang đóng", "status": "closed"}
            if self.is_session_active(account.id):
                return {"ok": True, "message": "Trình duyệt đăng nhập đã đang mở", "status": "already_open"}
            sess = InteractiveSession(account.id, "login")
            sess.task = asyncio.create_task(self._run_login_session(account, sess))
            self._sessions[account.id] = sess
            return {"ok": True, "message": "Đang mở trình duyệt để đăng nhập", "status": "opening"}

    async def open_view(self, account: Account) -> dict[str, Any]:
        async with self._lock:
            if self._closed:
                return {"ok": False, "message": "Hệ thống đang đóng", "status": "closed"}
            if self.is_session_active(account.id):
                return {"ok": True, "message": "Trình duyệt xem hội thoại đã đang mở", "status": "already_open"}
            sess = InteractiveSession(account.id, "view")
            sess.task = asyncio.create_task(self._run_view_session(account, sess))
            self._sessions[account.id] = sess
            return {"ok": True, "message": "Đang mở trình duyệt xem hội thoại", "status": "opening"}

    async def shutdown(self) -> None:
        async with self._lock:
            self._closed = True
            sessions = list(self._sessions.values())
            self._sessions.clear()
        tasks = []
        for s in sessions:
            await _terminate_proc(s.proc)
            if s.task and not s.task.done():
                s.task.cancel()
                tasks.append(s.task)
        if tasks:
            await asyncio.gather(*tasks, return_exceptions=True)

    def _get_profile_dir(self, account_id: str) -> Path:
        return self.settings.account_profile_dir(account_id)

    async def _run_login_session(self, account: Account, sess: InteractiveSession) -> None:
        account_id = account.id
        if hasattr(self.driver, "release_account_driver"):
            with contextlib.suppress(Exception):
                await self.driver.release_account_driver(account_id)

        profile_dir = self._get_profile_dir(account_id)
        profile_dir.mkdir(parents=True, exist_ok=True)

        for lock in ("SingletonLock", "SingletonSocket", "SingletonCookie", "lockfile", "DevToolsActivePort"):
            with contextlib.suppress(FileNotFoundError, OSError):
                (profile_dir / lock).unlink()

        try:
            exe = find_chromium(self.settings.chromium_path)
        except Exception as exc:
            log.error("cannot find chromium for interactive login: %s", exc)
            return

        port = find_free_port()
        proxy = account.proxy or self.settings.browser_proxy or ""
        proxy_relay: LocalProxyRelay | None = None

        target_url = "https://muse.ai/"
        args = [
            exe,
            f"--remote-debugging-port={port}",
            "--remote-debugging-address=127.0.0.1",
            f"--user-data-dir={profile_dir}",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-dev-shm-usage",
            "--autoplay-policy=no-user-gesture-required",
            "--window-size=1280,820",
            target_url,
        ]
        if proxy:
            if needs_proxy_relay(proxy):
                proxy_relay = LocalProxyRelay(proxy)
                effective_proxy = await proxy_relay.start()
            else:
                effective_proxy = proxy
            args.insert(1, f"--proxy-server={effective_proxy}")

        log.info("launching headed browser for account %s login on port %d", account_id, port)
        try:
            sess.proc = await asyncio.create_subprocess_exec(
                *args, stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL
            )
        except Exception as exc:
            log.error("failed to launch headed browser: %s", exc)
            if proxy_relay is not None:
                await proxy_relay.stop()
            return

        ws_url = await _resolve_page_ws_url(port, sess.proc, attempts=60)
        cdp: CDPSession | None = None
        if ws_url:
            try:
                cdp = await CDPSession.connect(ws_url)
                if account.cookies:
                    await _inject_cookies(cdp, account)
                await cdp.send("Network.enable")
                await cdp.send("Page.enable")
            except Exception as exc:
                log.warning("failed to connect CDP to interactive browser: %s", exc)
                if cdp is not None:
                    with contextlib.suppress(Exception):
                        await cdp.close()
                    cdp = None

        captured_auth_headers: dict[str, str] = {}
        if cdp:
            def on_request(params: dict) -> None:
                req = params.get("request", {})
                url = str(req.get("url", ""))
                if _is_muse_hostname(url):
                    headers = req.get("headers", {})
                    for hk, hv in headers.items():
                        hk_low = hk.lower()
                        if hk_low in ("authorization", "x-auth-token", "x-session-token", "x-csrf-token"):
                            captured_auth_headers[hk_low] = hv
            cdp.on("Network.requestWillBeSent", on_request)

        last_snapshot: tuple[Any, ...] | None = None
        try:
            while sess.proc.returncode is None:
                if cdp and not cdp.closed:
                    try:
                        # 1. Harvest cookies
                        res = await cdp.send(
                            "Network.getCookies",
                            {"urls": ["https://muse.ai", "https://muse.ai/"]},
                        )
                        cookies_list = res.get("cookies", [])
                        new_cookies: dict[str, str] = {}
                        new_expires: dict[str, int] = {}
                        for c in cookies_list:
                            cname = c.get("name")
                            cval = c.get("value")
                            if cname and cval:
                                new_cookies[cname] = cval
                                exp = c.get("expires")
                                if exp and float(exp) > 0:
                                    new_expires[cname] = int(float(exp))

                        # 2. Check for login token / session
                        if "hatch_sess" in new_cookies and new_cookies["hatch_sess"]:
                            local_storage: dict[str, Any] = {}
                            with contextlib.suppress(Exception):
                                storage_val = await cdp.evaluate(
                                    """
                                    (() => {
                                        try {
                                            return JSON.stringify(window.localStorage);
                                        } catch (e) {
                                            return null;
                                        }
                                    })()
                                    """
                                )
                                if isinstance(storage_val, str) and storage_val:
                                    local_storage = json.loads(storage_val)
                                elif isinstance(storage_val, dict):
                                    local_storage = storage_val

                            jwt_tokens = {}
                            for k, v in local_storage.items():
                                if any(
                                    token_key in k.lower()
                                    for token_key in ("token", "jwt", "auth", "session")
                                ):
                                    jwt_tokens[k] = v

                            snapshot = (
                                tuple(sorted(new_cookies.items())),
                                tuple(sorted(new_expires.items())),
                                tuple(sorted(captured_auth_headers.items())),
                                tuple(sorted(jwt_tokens.items())),
                            )
                            if snapshot != last_snapshot:
                                acc = self.pool.get(account_id)
                                if acc:
                                    acc.cookies.update(new_cookies)
                                    acc.cookie_expires.update(new_expires)
                                    acc.status = AccountStatus.ACTIVE
                                    acc.last_error = None
                                    if captured_auth_headers:
                                        acc.meta["auth_headers"] = {
                                            **acc.meta.get("auth_headers", {}),
                                            **captured_auth_headers,
                                        }
                                    if jwt_tokens:
                                        acc.meta["auth_tokens"] = jwt_tokens
                                    acc.meta["last_harvest_at"] = time.time()
                                    await self.pool.upsert(acc)
                                    last_snapshot = snapshot
                                    sess.harvested = True
                    except Exception as poll_exc:
                        log.debug("error polling interactive cdp session: %s", poll_exc)

                await asyncio.sleep(1.0)
        finally:
            if cdp:
                with contextlib.suppress(Exception):
                    await cdp.close()
            await _terminate_proc(sess.proc)
            if proxy_relay is not None:
                with contextlib.suppress(Exception):
                    await proxy_relay.stop()
            if hasattr(self.driver, "mark_account_stale"):
                with contextlib.suppress(Exception):
                    res_stale = self.driver.mark_account_stale(account_id)
                    if inspect.isawaitable(res_stale):
                        await res_stale
            elif hasattr(self.driver, "_tabs"):
                for t in self.driver._tabs.get(account_id, []):
                    t.stale = True
            if self._sessions.get(account_id) is sess:
                self._sessions.pop(account_id, None)
            log.info("headed login session ended for account %s (harvested: %s)", account_id, sess.harvested)

    async def _run_view_session(self, account: Account, sess: InteractiveSession) -> None:
        account_id = account.id
        if hasattr(self.driver, "release_account_driver"):
            with contextlib.suppress(Exception):
                await self.driver.release_account_driver(account_id)

        profile_dir = self._get_profile_dir(account_id)
        profile_dir.mkdir(parents=True, exist_ok=True)

        for lock in ("SingletonLock", "SingletonSocket", "SingletonCookie", "lockfile", "DevToolsActivePort"):
            with contextlib.suppress(FileNotFoundError, OSError):
                (profile_dir / lock).unlink()

        try:
            exe = find_chromium(self.settings.chromium_path)
        except Exception as exc:
            log.error("cannot find chromium for interactive view: %s", exc)
            return

        port = find_free_port()
        proxy = account.proxy or self.settings.browser_proxy or ""
        proxy_relay: LocalProxyRelay | None = None

        raw_url = (
            (account.meta.get("hot_page") or {}).get("url")
            or account.meta.get("last_thread_url")
            or "https://muse.ai/"
        )
        target_url = _sanitize_view_url(raw_url)
        args = [
            exe,
            f"--remote-debugging-port={port}",
            "--remote-debugging-address=127.0.0.1",
            f"--user-data-dir={profile_dir}",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-dev-shm-usage",
            "--autoplay-policy=no-user-gesture-required",
            "--window-size=1280,820",
            target_url,
        ]
        if proxy:
            if needs_proxy_relay(proxy):
                proxy_relay = LocalProxyRelay(proxy)
                effective_proxy = await proxy_relay.start()
            else:
                effective_proxy = proxy
            args.insert(1, f"--proxy-server={effective_proxy}")

        log.info("launching headed browser for account %s view on %s", account_id, target_url)
        try:
            sess.proc = await asyncio.create_subprocess_exec(
                *args, stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL
            )
        except Exception as exc:
            log.error("failed to launch headed browser for view: %s", exc)
            if proxy_relay is not None:
                await proxy_relay.stop()
            return

        cdp: CDPSession | None = None
        try:
            ws_url = await _resolve_page_ws_url(port, sess.proc, attempts=30)
            if ws_url and account.cookies:
                with contextlib.suppress(Exception):
                    cdp = await CDPSession.connect(ws_url)
                    await _inject_cookies(cdp, account)
                if cdp is not None:
                    with contextlib.suppress(Exception):
                        await cdp.close()
                    cdp = None
            await sess.proc.wait()
        finally:
            if cdp is not None:
                with contextlib.suppress(Exception):
                    await cdp.close()
            await _terminate_proc(sess.proc)
            if proxy_relay is not None:
                with contextlib.suppress(Exception):
                    await proxy_relay.stop()
            if hasattr(self.driver, "mark_account_stale"):
                with contextlib.suppress(Exception):
                    res_stale = self.driver.mark_account_stale(account_id)
                    if inspect.isawaitable(res_stale):
                        await res_stale
            if self._sessions.get(account_id) is sess:
                self._sessions.pop(account_id, None)
            log.info("headed view session ended for account %s", account_id)
