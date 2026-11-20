"""Interactive headed browser sessions for logging in and viewing account threads."""

from __future__ import annotations

import asyncio
import contextlib
import json
import logging
import socket
import time
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
        s.bind(("", 0))
        return s.getsockname()[1]


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

    def is_account_busy(self, account_id: str) -> bool:
        if hasattr(self.driver, "is_account_busy") and self.driver.is_account_busy(account_id):
            return True
        return False

    def is_session_active(self, account_id: str) -> bool:
        sess = self._sessions.get(account_id)
        return sess is not None and sess.task is not None and not sess.task.done()

    async def open_login(self, account: Account) -> dict[str, Any]:
        async with self._lock:
            if self.is_session_active(account.id):
                return {"ok": True, "message": "Trình duyệt đăng nhập đã đang mở", "status": "already_open"}
            sess = InteractiveSession(account.id, "login")
            sess.task = asyncio.create_task(self._run_login_session(account, sess))
            self._sessions[account.id] = sess
            return {"ok": True, "message": "Đang mở trình duyệt để đăng nhập", "status": "opening"}

    async def open_view(self, account: Account) -> dict[str, Any]:
        async with self._lock:
            if self.is_session_active(account.id):
                return {"ok": True, "message": "Trình duyệt xem hội thoại đã đang mở", "status": "already_open"}
            sess = InteractiveSession(account.id, "view")
            sess.task = asyncio.create_task(self._run_view_session(account, sess))
            self._sessions[account.id] = sess
            return {"ok": True, "message": "Đang mở trình duyệt xem hội thoại", "status": "opening"}

    def _get_profile_dir(self, account_id: str) -> Path:
        return self.settings.account_profile_dir(account_id)

    async def _run_login_session(self, account: Account, sess: InteractiveSession) -> None:
        account_id = account.id
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
            "--remote-allow-origins=*",
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

        # Poll DevTools port
        ws_url = None
        for _ in range(60):
            if sess.proc.returncode is not None:
                break
            try:
                async with httpx.AsyncClient(timeout=1.5, trust_env=False) as client:
                    r = await client.get(f"http://127.0.0.1:{port}/json/version")
                    ws_url = r.json().get("webSocketDebuggerUrl")
                    if ws_url:
                        break
            except Exception:
                pass
            await asyncio.sleep(0.5)

        cdp: CDPSession | None = None
        if ws_url:
            try:
                cdp = await CDPSession.connect(ws_url)
                if account.cookies:
                    for k, v in account.cookies.items():
                        with contextlib.suppress(Exception):
                            await cdp.send("Network.setCookie", {
                                "name": k, "value": v, "domain": "muse.ai", "path": "/"
                            })
                await cdp.send("Network.enable")
                await cdp.send("Page.enable")
            except Exception as exc:
                log.warning("failed to connect CDP to interactive browser: %s", exc)

        captured_auth_headers: dict[str, str] = {}
        if cdp:
            def on_request(params: dict) -> None:
                req = params.get("request", {})
                url = req.get("url", "")
                if "muse.ai" in url:
                    headers = req.get("headers", {})
                    for hk, hv in headers.items():
                        if hk.lower() in ("authorization", "x-auth-token", "x-session-token", "x-csrf-token"):
                            captured_auth_headers[hk] = hv
            cdp.on("Network.requestWillBeSent", on_request)

        try:
            while sess.proc.returncode is None:
                if cdp and not cdp.closed:
                    try:
                        # 1. Harvest cookies
                        res = await cdp.send("Network.getCookies", {"urls": ["https://muse.ai", "https://muse.ai/"]})
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
                            local_storage = {}
                            with contextlib.suppress(Exception):
                                storage_str = await cdp.evaluate("""
                                (() => {
                                    try {
                                        return JSON.stringify(window.localStorage);
                                    } catch(e) {
                                        return null;
                                    }
                                })()
                                """)
                                if storage_str:
                                    local_storage = json.loads(storage_str)

                            acc = self.pool.get(account_id)
                            if acc:
                                acc.cookies.update(new_cookies)
                                acc.cookie_expires.update(new_expires)
                                acc.status = AccountStatus.ACTIVE
                                acc.last_error = None
                                if captured_auth_headers:
                                    acc.meta["auth_headers"] = {**acc.meta.get("auth_headers", {}), **captured_auth_headers}
                                jwt_tokens = {}
                                for k, v in local_storage.items():
                                    if any(token_key in k.lower() for token_key in ("token", "jwt", "auth", "session")):
                                        jwt_tokens[k] = v
                                if jwt_tokens:
                                    acc.meta["auth_tokens"] = jwt_tokens
                                acc.meta["last_harvest_at"] = time.time()
                                await self.pool.upsert(acc)
                                sess.harvested = True
                    except Exception as poll_exc:
                        log.debug("error polling interactive cdp session: %s", poll_exc)

                await asyncio.sleep(1.0)
        finally:
            if cdp:
                with contextlib.suppress(Exception):
                    await cdp.close()
            with contextlib.suppress(Exception):
                if sess.proc and sess.proc.returncode is None:
                    sess.proc.terminate()
                    await sess.proc.wait()
            if proxy_relay is not None:
                with contextlib.suppress(Exception):
                    await proxy_relay.stop()
            if hasattr(self.driver, "_tabs"):
                tabs = self.driver._tabs.get(account_id, [])
                for t in tabs:
                    t.stale = True
            log.info("headed login session ended for account %s (harvested: %s)", account_id, sess.harvested)

    async def _run_view_session(self, account: Account, sess: InteractiveSession) -> None:
        account_id = account.id
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

        target_url = account.meta.get("hot_page", {}).get("url") or account.meta.get("last_thread_url") or "https://muse.ai/"
        args = [
            exe,
            f"--remote-debugging-port={port}",
            "--remote-allow-origins=*",
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

        ws_url = None
        for _ in range(30):
            if sess.proc.returncode is not None:
                break
            try:
                async with httpx.AsyncClient(timeout=1.5, trust_env=False) as client:
                    r = await client.get(f"http://127.0.0.1:{port}/json/version")
                    ws_url = r.json().get("webSocketDebuggerUrl")
                    if ws_url:
                        break
            except Exception:
                pass
            await asyncio.sleep(0.5)

        if ws_url and account.cookies:
            with contextlib.suppress(Exception):
                cdp = await CDPSession.connect(ws_url)
                for k, v in account.cookies.items():
                    with contextlib.suppress(Exception):
                        await cdp.send("Network.setCookie", {
                            "name": k, "value": v, "domain": "muse.ai", "path": "/"
                        })
                await cdp.close()

        try:
            await sess.proc.wait()
        finally:
            if proxy_relay is not None:
                with contextlib.suppress(Exception):
                    await proxy_relay.stop()
            log.info("headed view session ended for account %s", account_id)
