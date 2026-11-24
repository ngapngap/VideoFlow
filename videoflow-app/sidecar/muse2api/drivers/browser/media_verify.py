"""Standalone browser driver helpers: proxy/profile resolution and media integrity verification."""

from __future__ import annotations

import asyncio
import contextlib
import logging
import os
import tempfile
import uuid
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit

from ...accounts.model import Account
from ...config import Settings
from ...errors import Muse2APIError, UpstreamError

log = logging.getLogger(__name__)

QUOTA_CACHE_SECONDS = 1800.0


def resolve_proxy(account: Account | None, global_proxy: str = "") -> str:
    """Return proxy to use:
    1. account.proxy if set and non-empty.
    2. fallback to global browser_proxy if set and non-empty.
    3. fallback to direct connection (empty string).
    """
    if account and account.proxy and account.proxy.strip():
        return account.proxy.strip()
    if global_proxy and global_proxy.strip():
        return global_proxy.strip()
    return ""


def resolve_account_profile_dir(settings: Settings, account_id: str | None) -> Path:
    """Return isolated profile directory profiles/<account_id> under data_dir for the account."""
    if not account_id:
        return settings.profile_dir
    if hasattr(settings, "account_profile_dir"):
        return settings.account_profile_dir(account_id)
    if settings.browser_profile_dir:
        return settings.browser_profile_dir.parent / "profiles" / account_id
    return settings.data_dir / "profiles" / account_id


def quota_cache_seconds() -> float:
    """``MUSE2API_QUOTA_CACHE_S`` (read here: config.py has no knob for it)."""
    try:
        return max(0.0, float(os.environ.get("MUSE2API_QUOTA_CACHE_S", QUOTA_CACHE_SECONDS)))
    except ValueError:
        return QUOTA_CACHE_SECONDS


class AccountBusy(Muse2APIError):
    """Every tab of the account is serving a request; a quota read never takes one over."""

    status_code = 409
    error_type = "server_error"
    code = "account_busy"


def is_thread_url(url: str) -> bool:
    """A real muse conversation, not the empty "new thread" page."""
    path = urlsplit(url).path.rstrip("/")
    return path.startswith("/thread/") and path != "/thread/new"


def verify_magic_bytes(data: bytes, kind: str) -> bool:
    """Verify file magic bytes according to P1-1 specification."""
    if not data or len(data) < 3:
        return False
    if kind == "image":
        # PNG: 89 50 4E 47
        if data.startswith(b"\x89PNG") or data.startswith(b"\x89\x50\x4e\x47"):
            return True
        # JPEG: FF D8 FF
        if data.startswith(b"\xff\xd8\xff"):
            return True
        # WebP: 52 49 46 46 (RIFF) + WEBP at offset 8..12
        if data.startswith(b"RIFF") and len(data) >= 12 and data[8:12] == b"WEBP":
            return True
        # GIF: GIF87a / GIF89a
        if data.startswith((b"GIF87a", b"GIF89a")):
            return True
        # AVIF: ISO BMFF ftyp box with avif/avis/mif1 major brand or avif/avis compatible brand
        if len(data) >= 12 and data[4:8] == b"ftyp":
            if data[8:12] in (b"avif", b"avis", b"mif1"):
                return True
            box_size = int.from_bytes(data[:4], "big")
            compat_end = min(len(data), box_size) if box_size >= 16 else min(len(data), 64)
            compat_end = min(compat_end, 64)
            if any(data[i : i + 4] in (b"avif", b"avis") for i in range(16, compat_end - 3, 4)):
                return True
        return False
    elif kind == "video":
        # MP4/MOV/M4V: 00 00 00 ... ftyp
        if len(data) >= 8 and data[4:8] == b"ftyp":
            return True
        if len(data) >= 12 and data.startswith(b"\x00\x00\x00") and b"ftyp" in data[:16]:
            return True
        # WebM / Matroska (EBML header: 1A 45 DF A3)
        if data.startswith(b"\x1a\x45\xdf\xa3"):
            return True
        return False
    return True


def _infer_mime_from_bytes(data: bytes, kind: str) -> str:
    """Infer MIME type from verified magic bytes when upstream MIME is absent/generic."""
    if kind == "image":
        if data.startswith(b"\x89PNG"):
            return "image/png"
        if data.startswith(b"\xff\xd8\xff"):
            return "image/jpeg"
        if data.startswith(b"RIFF") and len(data) >= 12 and data[8:12] == b"WEBP":
            return "image/webp"
        if data.startswith((b"GIF87a", b"GIF89a")):
            return "image/gif"
        if len(data) >= 12 and data[4:8] == b"ftyp":
            return "image/avif"
        return "image/png"
    if kind == "video":
        if data.startswith(b"\x1a\x45\xdf\xa3"):
            return "video/webm"
        return "video/mp4"
    return "application/octet-stream"


async def verify_and_save_download(data: bytes, kind: str) -> bytes:
    """Verify download integrity using .partial file, magic bytes, and FFmpeg decode verify (P1-1).

    1. Check 0-bytes / empty data.
    2. Write temporary .partial file.
    3. Verify magic bytes: PNG, JPEG, WebP, GIF, AVIF, MP4, WebM.
    4. For video: run ffmpeg -v error -i file.partial -f null -
       If FFmpeg is not available, fallback to minimum size (>= 16 bytes) and valid magic bytes.
    5. If decode succeeds: atomic rename to verified file.
    6. If error: delete temporary file, raise UpstreamError to trigger automatic retry.
    """
    if not data or len(data) == 0:
        raise UpstreamError(f"downloaded data is empty (0 bytes) for {kind}")

    temp_dir = Path(tempfile.gettempdir()) / "videoflow_downloads"
    temp_dir.mkdir(parents=True, exist_ok=True)
    file_id = uuid.uuid4().hex
    partial_path = temp_dir / f"download_{file_id}.partial"
    verified_path = temp_dir / f"download_{file_id}.{'mp4' if kind == 'video' else 'verified'}"

    try:
        await asyncio.to_thread(partial_path.write_bytes, data)

        if (await asyncio.to_thread(partial_path.stat)).st_size == 0:
            raise UpstreamError(f"downloaded {kind} file on disk is 0 bytes")

        if not verify_magic_bytes(data, kind):
            raise UpstreamError(f"file integrity check failed: invalid magic bytes for {kind}")

        if kind == "video":
            proc = None
            try:
                sub_kwargs: dict[str, Any] = {
                    "stdout": asyncio.subprocess.PIPE,
                    "stderr": asyncio.subprocess.PIPE,
                }
                if os.name == "nt":
                    sub_kwargs["creationflags"] = 0x08000000  # CREATE_NO_WINDOW
                try:
                    proc = await asyncio.create_subprocess_exec(
                        "ffmpeg", "-v", "error", "-i", str(partial_path), "-f", "null", "-",
                        **sub_kwargs,
                    )
                except (FileNotFoundError, OSError) as exc:
                    log.warning(
                        "FFmpeg not available or cannot run (%s); falling back to minimum size and magic bytes check",
                        exc,
                    )
                    if len(data) < 16 or not verify_magic_bytes(data, "video"):
                        raise UpstreamError(
                            "video failed fallback integrity check: file too small or invalid magic bytes"
                        ) from exc
                else:
                    try:
                        _stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=30.0)
                    except asyncio.TimeoutError as exc:
                        raise UpstreamError("FFmpeg decode verification timed out") from exc
                    if proc.returncode != 0:
                        err_msg = (stderr or b"").decode(errors="replace").strip()
                        raise UpstreamError(f"FFmpeg decode verification failed: {err_msg}")
            finally:
                if proc is not None and proc.returncode is None:
                    with contextlib.suppress(ProcessLookupError, OSError):
                        proc.kill()
                    with contextlib.suppress(Exception):
                        await asyncio.wait_for(proc.wait(), timeout=5.0)

        # Atomic rename to official verified file
        await asyncio.to_thread(partial_path.replace, verified_path)
        verified_data = await asyncio.to_thread(verified_path.read_bytes)
        return verified_data
    finally:
        with contextlib.suppress(OSError):
            partial_path.unlink(missing_ok=True)
        with contextlib.suppress(OSError):
            verified_path.unlink(missing_ok=True)


