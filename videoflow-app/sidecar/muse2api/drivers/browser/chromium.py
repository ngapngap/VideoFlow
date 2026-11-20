"""Chromium process management."""

from __future__ import annotations

import asyncio
import contextlib
import logging
import os
import shutil
import socket
import subprocess
from pathlib import Path

import httpx

from .proxy_relay import LocalProxyRelay, needs_proxy_relay

log = logging.getLogger(__name__)

_CANDIDATES = (
    "chromium",
    "chromium-browser",
    "google-chrome",
    "google-chrome-stable",
    "/usr/bin/chromium",
    "/snap/bin/chromium",
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
    r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
    r"C:\Program Files\Google\Chrome\Application\chrome.exe",
    r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
)


def find_chromium(configured: str = "") -> str:
    for c in ((configured,) if configured else ()) + _CANDIDATES:
        if c and (os.path.isfile(c) or shutil.which(c)):
            return c
    raise FileNotFoundError(
        "Chromium/Chrome not found; install it or set MUSE2API_CHROMIUM_PATH"
    )


def _is_port_in_use(port: int) -> bool:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.settimeout(0.2)
        return sock.connect_ex(("127.0.0.1", port)) == 0


def _find_free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def _build_wmi_filter(profile_dir: Path, port: int | None = None) -> str:
    """Build a boundary-aware case-insensitive PowerShell filter matching only processes of this profile/port."""
    full = str(profile_dir).replace("'", "''")
    parent_esc = str(profile_dir.parent).replace("'", "''")
    # Match exact --user-data-dir=<full> followed by quote, whitespace, or end-of-line
    # so acc_1 never matches acc_10.
    cond_parts: list[str] = [
        f"($_.CommandLine -imatch ('--user-data-dir=\"?' + [regex]::Escape('{full}') + '(\"|\\s|$)'))"
    ]
    if port:
        # Require this app's profile parent dir alongside --remote-debugging-port so
        # unrelated user browsers using the same port are never matched.
        cond_parts.append(
            f"(($_.CommandLine -imatch [regex]::Escape('{parent_esc}')) -and ($_.CommandLine -imatch '--remote-debugging-port={port}(\\s|$)'))"
        )
    return " -or ".join(cond_parts)


def _kill_stale_browsers(profile_dir: Path, port: int | None = None) -> int:
    """Reap Chromium/Edge processes still holding ``profile_dir``.

    Cleanup is strictly isolated to the specified profile directory and port,
    preventing any cross-account process termination in multi-account environments.
    """
    if os.name != "nt":
        return 0
    conditions = _build_wmi_filter(profile_dir, port)
    creationflags = getattr(subprocess, "CREATE_NO_WINDOW", 0x08000000)
    try:
        script = (
            "Get-CimInstance Win32_Process -Filter \"Name='msedge.exe' OR Name='chrome.exe'\" | "
            f"Where-Object {{ $_.CommandLine -and ({conditions}) }} | "
            "ForEach-Object { $_.ProcessId }"
        )
        out = subprocess.run(
            ["powershell", "-NoProfile", "-NonInteractive", "-Command", script],
            capture_output=True, text=True, timeout=25,
            creationflags=creationflags,
        )
        pids = [int(x) for x in (out.stdout or "").split() if x.strip().isdigit()]
    except Exception as exc:  # noqa: BLE001 - best effort cleanup
        log.warning("stale browser scan failed: %s", exc)
        return 0
    for pid in pids:
        with contextlib.suppress(Exception):
            subprocess.run(
                ["taskkill", "/PID", str(pid), "/T", "/F"],
                capture_output=True, timeout=15,
                creationflags=creationflags,
            )
    if pids:
        log.warning("killed %d stale browser process(es) holding %s", len(pids), profile_dir)
    return len(pids)


class ChromiumProcess:
    def __init__(self, executable: str, port: int, profile_dir: Path, headless: bool = True,
                 proxy: str = ""):
        self.executable = executable
        self.port = port
        self.profile_dir = profile_dir
        self.headless = headless
        self.proxy = proxy
        self.proc: asyncio.subprocess.Process | None = None
        self._proxy_relay: LocalProxyRelay | None = None

    @property
    def http_base(self) -> str:
        return f"http://127.0.0.1:{self.port}"

    async def browser_ws_url(self) -> str | None:
        dap = self.profile_dir / "DevToolsActivePort"
        if dap.is_file():
            with contextlib.suppress(OSError, ValueError):
                lines = [ln.strip() for ln in dap.read_text(encoding="utf-8", errors="ignore").splitlines() if ln.strip()]
                if lines and lines[0].isdigit() and int(lines[0]) != self.port:
                    return None
        try:
            # Local DevTools port must not follow http_proxy/https_proxy.
            async with httpx.AsyncClient(timeout=2, trust_env=False) as client:
                r = await client.get(f"{self.http_base}/json/version")
                return r.json().get("webSocketDebuggerUrl")
        except (httpx.HTTPError, ValueError):
            return None

    async def start(self) -> str:
        """Start a new Chromium process for this profile; return the browser ws URL."""
        # A stale Edge/Chrome still holding this profile makes the new launch hand
        # off and exit before opening the DevTools port — reap it, then retry.
        # Attempt 3 wipes the profile directory outright: it only holds the browser
        # session cache, while the accounts themselves live in data/accounts.json.
        for attempt in (1, 2, 3):
            killed = await asyncio.to_thread(_kill_stale_browsers, self.profile_dir, self.port)
            if killed:
                await asyncio.sleep(1.5)
            if await asyncio.to_thread(_is_port_in_use, self.port):
                new_port = _find_free_port()
                log.warning(
                    "CDP port %d is still occupied by another process after stale cleanup; "
                    "re-allocating port %d",
                    self.port,
                    new_port,
                )
                self.port = new_port
            if attempt == 3:
                log.warning("resetting browser profile %s after repeated CDP failures",
                            self.profile_dir)
                shutil.rmtree(self.profile_dir, ignore_errors=True)
            self.profile_dir.mkdir(parents=True, exist_ok=True)
            for lock in ("SingletonLock", "SingletonSocket", "SingletonCookie",
                         "lockfile", "DevToolsActivePort"):
                with contextlib.suppress(FileNotFoundError):
                    (self.profile_dir / lock).unlink()
            if url := await self._launch():
                return url
            log.warning("Chromium did not expose DevTools on attempt %d", attempt)
            await self.stop()
            await asyncio.sleep(1.5)
        raise RuntimeError(
            "Chromium did not expose a DevTools endpoint after cleaning stale profile "
            f"processes and resetting the profile; check that Edge/Chrome can start "
            f"with profile {self.profile_dir} (see chromium-stderr.log next to it)"
        )

    async def _launch(self) -> str | None:
        args = [
            self.executable,
            f"--remote-debugging-port={self.port}",
            f"--user-data-dir={self.profile_dir}",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-dev-shm-usage",
            "--disable-background-networking",
            "--autoplay-policy=no-user-gesture-required",
            "--window-size=1440,2200",
            "about:blank",
        ]
        try:
            if self.proxy:
                if needs_proxy_relay(self.proxy):
                    if self._proxy_relay is not None:
                        await self._proxy_relay.stop()
                    self._proxy_relay = LocalProxyRelay(self.proxy)
                    effective_proxy = await self._proxy_relay.start()
                else:
                    effective_proxy = self.proxy.strip().rstrip("/")
                args.insert(1, f"--proxy-server={effective_proxy}")
            if self.headless:
                args[1:1] = ["--headless=new", "--disable-gpu"]
            if hasattr(os, "geteuid") and os.geteuid() == 0:
                args.insert(1, "--no-sandbox")

            log.info("launching Chromium: %s (profile=%s)", self.executable, self.profile_dir)
            # Keep stderr isolated per profile so concurrent account launches never collide on Windows.
            self._stderr_path = self.profile_dir / "chromium-stderr.log"
            stderr_sink = await asyncio.to_thread(self._stderr_path.open, "wb")
            try:
                self.proc = await asyncio.create_subprocess_exec(
                    *args, stdout=asyncio.subprocess.DEVNULL, stderr=stderr_sink
                )
            finally:
                await asyncio.to_thread(stderr_sink.close)

            for _ in range(120):  # 60s: cold start on Windows + AV scanning can be slow
                if self.proc.returncode is not None:
                    tail = ""
                    with contextlib.suppress(OSError):
                        tail = self._stderr_path.read_text(encoding="utf-8", errors="replace")[-800:]
                    log.warning("Chromium exited early with code %s; stderr: %s",
                                self.proc.returncode, tail or "(empty)")
                    break
                if url := await self.browser_ws_url():
                    return url
                await asyncio.sleep(0.5)
            return None
        except BaseException:
            await self.stop()
            raise

    async def stop(self) -> None:
        try:
            if self.proc and self.proc.returncode is None:
                try:
                    if os.name == "nt":
                        creationflags = getattr(subprocess, "CREATE_NO_WINDOW", 0x08000000)
                        with contextlib.suppress(Exception):
                            await asyncio.to_thread(
                                subprocess.run,
                                ["taskkill", "/PID", str(self.proc.pid), "/T", "/F"],
                                capture_output=True,
                                timeout=15,
                                creationflags=creationflags,
                            )
                    else:
                        with contextlib.suppress(ProcessLookupError, OSError):
                            self.proc.terminate()
                    try:
                        await asyncio.wait_for(self.proc.wait(), 10)
                    except asyncio.TimeoutError:
                        with contextlib.suppress(ProcessLookupError, OSError):
                            self.proc.kill()
                        with contextlib.suppress(Exception):
                            await asyncio.wait_for(self.proc.wait(), 5)
                except asyncio.CancelledError:
                    with contextlib.suppress(ProcessLookupError, OSError):
                        self.proc.kill()
                    raise
        finally:
            self.proc = None
            if self._proxy_relay is not None:
                try:
                    await self._proxy_relay.stop()
                finally:
                    self._proxy_relay = None
