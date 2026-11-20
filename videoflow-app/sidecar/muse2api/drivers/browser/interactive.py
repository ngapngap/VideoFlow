"""Interactive headed browser sessions for logging in and viewing account threads."""

from __future__ import annotations

import asyncio
import contextlib
import inspect
import json
import logging
import os
import socket
import subprocess
import time
import urllib.parse
from pathlib import Path
from typing import Any

import httpx

from ...accounts.model import Account, AccountStatus
from ...accounts.pool import AccountPool
from ...config import Settings
from ...errors import NotFound
from ...upstream.muse import missing_session_cookies
from .cdp import CDPSession
from .chromium import _kill_stale_browsers, find_chromium
from .driver import AccountBusy, resolve_proxy
from .proxy_relay import LocalProxyRelay, mask_proxy, needs_proxy_relay

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


async def _shield_quiet(coro: Any) -> None:
    async def _runner() -> None:
        with contextlib.suppress(Exception):
            await coro

    await asyncio.shield(_runner())


async def _terminate_proc(proc: asyncio.subprocess.Process | None) -> None:
    if proc is None or proc.returncode is not None:
        return
    killed_via_taskkill = False
    if os.name == "nt" and getattr(proc, "pid", None):
        try:
            res = await asyncio.to_thread(
                subprocess.run,
                ["taskkill", "/PID", str(proc.pid), "/T", "/F"],
                capture_output=True,
                timeout=15,
                creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0x08000000),
            )
            killed_via_taskkill = getattr(res, "returncode", 1) == 0
        except asyncio.CancelledError:
            with contextlib.suppress(Exception):
                proc.kill()
            raise
        except Exception:  # noqa: BLE001
            pass
    if not killed_via_taskkill:
        with contextlib.suppress(Exception):
            proc.terminate()
    try:
        await asyncio.wait_for(proc.wait(), timeout=5.0)
    except asyncio.CancelledError:
        with contextlib.suppress(Exception):
            proc.kill()
        raise
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

    async def _open_session(self, account: Account, kind: str) -> dict[str, Any]:
        already_msg = (
            "Trình duyệt đăng nhập đã đang mở"
            if kind == "login"
            else "Trình duyệt xem hội thoại đã đang mở"
        )
        opening_msg = (
            "Đang mở trình duyệt để đăng nhập"
            if kind == "login"
            else "Đang mở trình duyệt xem hội thoại"
        )
        cond = getattr(self.pool, "_cond", None)
        async with self._lock:
            if self._closed:
                return {"ok": False, "message": "Hệ thống đang đóng", "status": "closed"}
            async with (cond if cond is not None else contextlib.nullcontext()):
                if self.is_session_active(account.id):
                    return {"ok": True, "message": already_msg, "status": "already_open"}
                if self.is_account_busy(account.id):
                    raise AccountBusy("Tài khoản đang bận chạy batch, không thể mở trình duyệt")
                if hasattr(self.pool, "get"):
                    latest = self.pool.get(account.id)
                    if latest is None:
                        raise NotFound(f"account '{account.id}' not found")
                else:
                    latest = None
                snap = (latest or account).model_copy(deep=True)
                sess = InteractiveSession(snap.id, kind)
                coro = (
                    self._run_login_session(snap, sess)
                    if kind == "login"
                    else self._run_view_session(snap, sess)
                )
                sess.task = asyncio.create_task(coro)
                self._sessions[snap.id] = sess
                if cond is not None:
                    cond.notify_all()
                return {"ok": True, "message": opening_msg, "status": "opening"}

    async def open_login(self, account: Account) -> dict[str, Any]:
        return await self._open_session(account, "login")

    async def open_view(self, account: Account) -> dict[str, Any]:
        return await self._open_session(account, "view")

    async def stop_session(self, account_id: str) -> bool:
        async with self._lock:
            sess = self._sessions.get(account_id)
            if sess is None:
                return False
        if sess.task is not None and inspect.isawaitable(sess.task) and not sess.task.done():
            sess.task.cancel()
        await _terminate_proc(sess.proc)
        if sess.task is not None and inspect.isawaitable(sess.task):
            with contextlib.suppress(BaseException):
                await asyncio.wait_for(asyncio.shield(sess.task), timeout=22.0)
        async with self._lock:
            if self._sessions.get(account_id) is sess:
                self._sessions.pop(account_id, None)
        cond = getattr(self.pool, "_cond", None)
        if cond is not None:
            async with cond:
                cond.notify_all()
        return True

    async def shutdown(self) -> None:
        async with self._lock:
            self._closed = True
            sessions = list(self._sessions.values())
            self._sessions.clear()
        tasks = []
        for s in sessions:
            if s.task and inspect.isawaitable(s.task) and not s.task.done():
                s.task.cancel()
                tasks.append(s.task)
            await _terminate_proc(s.proc)
        if tasks:
            await asyncio.gather(*tasks, return_exceptions=True)

    async def _cleanup_session(
        self,
        account_id: str,
        sess: InteractiveSession,
        cdp: CDPSession | None,
        proxy_relay: LocalProxyRelay | None,
        *,
        fallback_stale_tabs: bool = False,
    ) -> None:
        cancelled_exc: asyncio.CancelledError | None = None

        if cdp is not None:
            try:
                await _shield_quiet(cdp.close())
            except asyncio.CancelledError as exc:
                cancelled_exc = exc
            except Exception:  # noqa: BLE001
                pass

        try:
            await _terminate_proc(sess.proc)
        except asyncio.CancelledError as exc:
            cancelled_exc = exc
        except Exception:  # noqa: BLE001
            pass

        if proxy_relay is not None:
            try:
                await _shield_quiet(proxy_relay.stop())
            except asyncio.CancelledError as exc:
                cancelled_exc = exc
            except Exception:  # noqa: BLE001
                pass

        if hasattr(self.driver, "mark_account_stale"):
            try:
                res_stale = self.driver.mark_account_stale(account_id)
                if inspect.isawaitable(res_stale):
                    await _shield_quiet(res_stale)
            except asyncio.CancelledError as exc:
                cancelled_exc = exc
            except Exception:  # noqa: BLE001
                pass
        elif fallback_stale_tabs and hasattr(self.driver, "_tabs"):
            for t in self.driver._tabs.get(account_id, []):
                t.stale = True

        if self._sessions.get(account_id) is sess:
            self._sessions.pop(account_id, None)

        cond = getattr(self.pool, "_cond", None)
        if cond is not None:
            try:
                async with cond:
                    cond.notify_all()
            except asyncio.CancelledError as exc:
                cancelled_exc = exc
            except Exception:  # noqa: BLE001
                pass

        if cancelled_exc is not None:
            raise cancelled_exc

    def _get_profile_dir(self, account_id: str) -> Path:
        return self.settings.account_profile_dir(account_id)

    async def _prepare_profile_and_proxy(
        self,
        profile_dir: Path,
        args: list[str],
        account: Account,
    ) -> LocalProxyRelay | None:
        def _prep_dir_and_clean() -> None:
            profile_dir.mkdir(parents=True, exist_ok=True)
            _kill_stale_browsers(profile_dir)
            for lock in ("SingletonLock", "SingletonSocket", "SingletonCookie", "lockfile", "DevToolsActivePort"):
                with contextlib.suppress(FileNotFoundError, OSError):
                    (profile_dir / lock).unlink()

        await asyncio.to_thread(_prep_dir_and_clean)

        proxy = resolve_proxy(account, self.settings.browser_proxy)
        if not proxy:
            return None
        if needs_proxy_relay(proxy):
            relay = LocalProxyRelay(proxy)
            try:
                effective_proxy = await relay.start()
            except Exception:
                await relay.stop()
                raise
            args.insert(1, f"--proxy-server={effective_proxy}")
            return relay
        args.insert(1, f"--proxy-server={proxy.strip().rstrip('/')}")
        return None

    async def _run_login_session(self, account: Account, sess: InteractiveSession) -> None:
        account_id = account.id
        proxy_relay: LocalProxyRelay | None = None
        cdp: CDPSession | None = None
        try:
            if hasattr(self.driver, "release_account_driver"):
                with contextlib.suppress(Exception):
                    await self.driver.release_account_driver(account_id)

            profile_dir = self._get_profile_dir(account_id)
            try:
                exe = find_chromium(self.settings.chromium_path)
            except Exception as exc:
                log.error("cannot find chromium for interactive login: %s", exc)
                return

            port = find_free_port()
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
                "about:blank",
            ]
            try:
                proxy_relay = await self._prepare_profile_and_proxy(profile_dir, args, account)
            except Exception as exc:
                log.error(
                    "failed to start proxy relay (%s) for interactive login: %s",
                    mask_proxy(resolve_proxy(account, self.settings.browser_proxy)),
                    exc,
                )
                return

            log.info("launching headed browser for account %s login on port %d", account_id, port)
            try:
                sess.proc = await asyncio.create_subprocess_exec(
                    *args, stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL
                )
            except Exception as exc:
                log.error("failed to launch headed browser: %s", exc)
                return

            captured_auth_headers: dict[str, str] = {}
            ws_url = await _resolve_page_ws_url(port, sess.proc, attempts=60)
            if ws_url:
                try:
                    cdp = await CDPSession.connect(ws_url)
                    if account.cookies:
                        await _inject_cookies(cdp, account)
                    await cdp.send("Network.enable")
                    await cdp.send("Page.enable")

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
                    await cdp.send("Page.navigate", {"url": target_url})
                except Exception as exc:
                    log.warning("failed to connect CDP to interactive browser: %s", exc)
                    if cdp is not None:
                        with contextlib.suppress(Exception):
                            await cdp.close()
                        cdp = None

            last_snapshot: tuple[Any, ...] | None = None
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
                                def _apply_harvest(
                                    latest: Account,
                                    cookies: dict[str, str] = new_cookies,
                                    expires: dict[str, int] = new_expires,
                                    tokens: dict[str, Any] = jwt_tokens,
                                ) -> tuple[bool, None]:
                                    latest.cookies.update(cookies)
                                    if latest.cookie_expires is None:
                                        latest.cookie_expires = {}
                                    for name in cookies:
                                        if name not in expires:
                                            latest.cookie_expires.pop(name, None)
                                    latest.cookie_expires.update(expires)
                                    if (
                                        latest.status != AccountStatus.VIDEO_LOCKED
                                        and not missing_session_cookies(latest.cookies)
                                    ):
                                        latest.status = AccountStatus.ACTIVE
                                        latest.last_error = None
                                    if captured_auth_headers:
                                        latest.meta["auth_headers"] = {
                                            **latest.meta.get("auth_headers", {}),
                                            **captured_auth_headers,
                                        }
                                    if tokens:
                                        latest.meta["auth_tokens"] = tokens
                                    latest.meta["last_harvest_at"] = time.time()
                                    return True, None

                                if hasattr(self.pool, "try_update"):
                                    found, _ = await self.pool.try_update(account_id, _apply_harvest)
                                else:
                                    acc = self.pool.get(account_id)
                                    found = acc is not None
                                    if acc is not None:
                                        _apply_harvest(acc)
                                        await self.pool.upsert(acc)
                                if found:
                                    last_snapshot = snapshot
                                    sess.harvested = True
                    except Exception as poll_exc:
                        log.debug("error polling interactive cdp session: %s", poll_exc)

                await asyncio.sleep(1.0)
        finally:
            await self._cleanup_session(
                account_id,
                sess,
                cdp,
                proxy_relay,
                fallback_stale_tabs=True,
            )
            log.info("headed login session ended for account %s (harvested: %s)", account_id, sess.harvested)

    async def _run_view_session(self, account: Account, sess: InteractiveSession) -> None:
        account_id = account.id
        proxy_relay: LocalProxyRelay | None = None
        cdp: CDPSession | None = None
        try:
            if hasattr(self.driver, "release_account_driver"):
                with contextlib.suppress(Exception):
                    await self.driver.release_account_driver(account_id)

            profile_dir = self._get_profile_dir(account_id)
            try:
                exe = find_chromium(self.settings.chromium_path)
            except Exception as exc:
                log.error("cannot find chromium for interactive view: %s", exc)
                return

            port = find_free_port()
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
                "about:blank",
            ]
            try:
                proxy_relay = await self._prepare_profile_and_proxy(profile_dir, args, account)
            except Exception as exc:
                log.error(
                    "failed to start proxy relay (%s) for interactive view: %s",
                    mask_proxy(resolve_proxy(account, self.settings.browser_proxy)),
                    exc,
                )
                return

            log.info("launching headed browser for account %s view on %s", account_id, target_url)
            try:
                sess.proc = await asyncio.create_subprocess_exec(
                    *args, stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL
                )
            except Exception as exc:
                log.error("failed to launch headed browser for view: %s", exc)
                return

            ws_url = await _resolve_page_ws_url(port, sess.proc, attempts=30)
            if ws_url:
                try:
                    cdp = await CDPSession.connect(ws_url)
                    if account.cookies:
                        await _inject_cookies(cdp, account)
                    await cdp.send("Page.enable")
                    await cdp.send("Page.navigate", {"url": target_url})
                except Exception as exc:
                    log.warning("failed to connect/navigate CDP for view: %s", exc)
                finally:
                    if cdp is not None:
                        with contextlib.suppress(Exception):
                            await cdp.close()
                        cdp = None
            await sess.proc.wait()
        finally:
            await self._cleanup_session(
                account_id,
                sess,
                cdp,
                proxy_relay,
                fallback_stale_tabs=False,
            )
            log.info("headed view session ended for account %s", account_id)
