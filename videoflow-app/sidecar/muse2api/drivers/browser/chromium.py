"""Chromium process management."""

from __future__ import annotations

import asyncio
import contextlib
import logging
import os
import shutil
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


def _build_wmi_filter(profile_dir: Path, port: int | None = None) -> str:
    """Build a boundary-aware case-insensitive PowerShell filter matching only processes of this profile/port."""
    full = str(profile_dir).replace("'", "''")
    # Match exact --user-data-dir=<full> followed by quote, whitespace, or end-of-line
    # so acc_1 never matches acc_10.
    cond_parts: list[str] = [
        f"($_.CommandLine -imatch ('--user-data-dir=\"?' + [regex]::Escape('{full}') + '(\"|\\s|$)'))"
    ]
    if port:
        cond_parts.append(
            f"($_.CommandLine -imatch '--remote-debugging-port={port}(\\s|$)')"
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
    try:
        script = (
            "Get-CimInstance Win32_Process -Filter \"Name='msedge.exe' OR Name='chrome.exe'\" | "
            f"Where-Object {{ $_.CommandLine -and ({conditions}) }} | "
            "ForEach-Object { $_.ProcessId }"
        )
        out = subprocess.run(
            ["powershell", "-NoProfile", "-NonInteractive", "-Command", script],
            capture_output=True, text=True, timeout=25,
        )
        pids = [int(x) for x in (out.stdout or "").split() if x.strip().isdigit()]
    except Exception as exc:  # noqa: BLE001 - best effort cleanup
        log.warning("stale browser scan failed: %s", exc)
        return 0
    for pid in pids:
        with contextlib.suppress(Exception):
            subprocess.run(["taskkill", "/PID", str(pid), "/T", "/F"],
                           capture_output=True, timeout=15)
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
        try:
            # Local DevTools port must not follow http_proxy/https_proxy.
            async with httpx.AsyncClient(timeout=2, trust_env=False) as client:
                r = await client.get(f"{self.http_base}/json/version")
                return r.json().get("webSocketDebuggerUrl")
        except (httpx.HTTPError, ValueError):
            return None

    async def start(self) -> str:
        """Start (or attach to an already running) Chromium; return the browser ws URL."""
        if url := await self.browser_ws_url():
            log.info("attached to existing Chromium on port %s", self.port)
            return url

        # A stale Edge/Chrome still holding this profile makes the new launch hand
        # off and exit before opening the DevTools port — reap it, then retry.
        # Attempt 3 wipes the profile directory outright: it only holds the browser
        # session cache, while the accounts themselves live in data/accounts.json.
        for attempt in (1, 2, 3):
            killed = await asyncio.to_thread(_kill_stale_browsers, self.profile_dir, self.port)
            if killed:
                await asyncio.sleep(1.5)
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
        if self.proxy:
            if needs_proxy_relay(self.proxy):
                if self._proxy_relay is not None:
                    await self._proxy_relay.stop()
                self._proxy_relay = LocalProxyRelay(self.proxy)
                effective_proxy = await self._proxy_relay.start()
            else:
                effective_proxy = self.proxy
            args.insert(1, f"--proxy-server={effective_proxy}")
        if self.headless:
            args[1:1] = ["--headless=new", "--disable-gpu"]
        if hasattr(os, "geteuid") and os.geteuid() == 0:
            args.insert(1, "--no-sandbox")

        log.info("launching Chromium: %s (profile=%s)", self.executable, self.profile_dir)
        # Keep stderr isolated per profile so concurrent account launches never collide on Windows.
        self._stderr_path = self.profile_dir / "chromium-stderr.log"
        try:
            stderr_sink = open(self._stderr_path, "wb")
            try:
                self.proc = await asyncio.create_subprocess_exec(
                    *args, stdout=asyncio.subprocess.DEVNULL, stderr=stderr_sink
                )
            finally:
                stderr_sink.close()
        except Exception:
            if self._proxy_relay is not None:
                await self._proxy_relay.stop()
                self._proxy_relay = None
            raise
        for _ in range(120):  # 60s: cold start on Windows + AV scanning can be slow
            if url := await self.browser_ws_url():
                return url
            if self.proc.returncode is not None:
                tail = ""
                with contextlib.suppress(OSError):
                    tail = self._stderr_path.read_text(encoding="utf-8", errors="replace")[-800:]
                log.warning("Chromium exited early with code %s; stderr: %s",
                            self.proc.returncode, tail or "(empty)")
                break
            await asyncio.sleep(0.5)
        return None

    async def stop(self) -> None:
        if self.proc and self.proc.returncode is None:
            if os.name == "nt":
                with contextlib.suppress(Exception):
                    await asyncio.to_thread(
                        subprocess.run,
                        ["taskkill", "/PID", str(self.proc.pid), "/T", "/F"],
                        capture_output=True,
                        timeout=15,
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
        self.proc = None
        if self._proxy_relay is not None:
            await self._proxy_relay.stop()
            self._proxy_relay = None
