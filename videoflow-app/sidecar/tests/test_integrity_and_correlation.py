"""Unit tests for Milestone 4: Issue #7 [P1.1]
- P1-1: Download integrity check (.partial, magic bytes, FFmpeg decode verify)
- P1-2: Video mount polling (up to 6s)
- P1-3: URL attachment correlation snapshot (existing_urls set)
"""

from __future__ import annotations

import asyncio
import base64
import tempfile
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from muse2api.config import Settings
from muse2api.core.media import MediaStore
from muse2api.drivers.browser import dom
from muse2api.drivers.browser.driver import (
    BrowserDriver,
    _Tab,
    verify_and_save_download,
    verify_magic_bytes,
)
from muse2api.errors import UpstreamError


@pytest.fixture
def settings(tmp_path: Path) -> Settings:
    s = Settings(data_dir=tmp_path, media_dir=tmp_path / "media")
    s.media_dir.mkdir(parents=True, exist_ok=True)
    return s


# =========================================================================
# P1-1: Magic Bytes Verification
# =========================================================================

def test_verify_magic_bytes_png():
    # PNG signature: 89 50 4E 47
    png_header = b"\x89PNG\r\n\x1a\n" + b"\x00" * 20
    assert verify_magic_bytes(png_header, "image") is True

    # Hex representation
    png_hex = bytes.fromhex("89504E470D0A1A0A0000000D49484452")
    assert verify_magic_bytes(png_hex, "image") is True


def test_verify_magic_bytes_jpeg():
    # JPEG signature: FF D8 FF
    jpeg_header = b"\xff\xd8\xff\xe0\x00\x10JFIF" + b"\x00" * 20
    assert verify_magic_bytes(jpeg_header, "image") is True

    jpeg_hex = bytes.fromhex("FFD8FFE1001845786966")
    assert verify_magic_bytes(jpeg_hex, "image") is True


def test_verify_magic_bytes_webp():
    # WebP signature: 52 49 46 46 (RIFF) + WEBP
    webp_header = b"RIFF\x24\x00\x00\x00WEBPVP8 " + b"\x00" * 20
    assert verify_magic_bytes(webp_header, "image") is True

    webp_hex = bytes.fromhex("52494646240000005745425056503820")
    assert verify_magic_bytes(webp_hex, "image") is True


def test_verify_magic_bytes_mp4():
    # MP4 signature: 00 00 00 ... ftyp
    mp4_header = b"\x00\x00\x00\x18ftypmp42" + b"\x00" * 20
    assert verify_magic_bytes(mp4_header, "video") is True

    # Alternate MP4 ftyp brand
    mp4_isom = b"\x00\x00\x00\x20ftypisom\x02\x00\x00\x00"
    assert verify_magic_bytes(mp4_isom, "video") is True


def test_verify_magic_bytes_invalid_and_empty():
    # Corrupt data
    assert verify_magic_bytes(b"<html>502 Bad Gateway</html>", "image") is False
    assert verify_magic_bytes(b"<html>502 Bad Gateway</html>", "video") is False

    # Short / empty
    assert verify_magic_bytes(b"", "image") is False
    assert verify_magic_bytes(b"", "video") is False
    assert verify_magic_bytes(b"\x00\x00", "video") is False

    # Wrong kind (e.g. video bytes for image or vice versa)
    mp4_header = b"\x00\x00\x00\x18ftypmp42" + b"\x00" * 20
    assert verify_magic_bytes(mp4_header, "image") is False

    png_header = b"\x89PNG\r\n\x1a\n" + b"\x00" * 20
    assert verify_magic_bytes(png_header, "video") is False


# =========================================================================
# P1-1: Download Integrity Verification Flow (.partial + FFmpeg verify)
# =========================================================================

@pytest.mark.asyncio
async def test_verify_and_save_download_zero_bytes():
    with pytest.raises(UpstreamError, match="empty .*0 bytes"):
        await verify_and_save_download(b"", "video")

    with pytest.raises(UpstreamError, match="empty .*0 bytes"):
        await verify_and_save_download(b"", "image")


@pytest.mark.asyncio
async def test_verify_and_save_download_corrupt_magic_bytes():
    corrupt_data = b"Some random corrupted text from network error"
    with pytest.raises(UpstreamError, match="invalid magic bytes"):
        await verify_and_save_download(corrupt_data, "video")

    with pytest.raises(UpstreamError, match="invalid magic bytes"):
        await verify_and_save_download(corrupt_data, "image")


@pytest.mark.asyncio
async def test_verify_and_save_download_image_success():
    valid_png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR" + b"\x00" * 64
    verified = await verify_and_save_download(valid_png, "image")
    assert verified == valid_png

    valid_jpeg = b"\xff\xd8\xff\xe0\x00\x10JFIF" + b"\x00" * 64
    verified_jpg = await verify_and_save_download(valid_jpeg, "image")
    assert verified_jpg == valid_jpeg


@pytest.mark.asyncio
async def test_verify_and_save_download_video_ffmpeg_success():
    valid_mp4 = b"\x00\x00\x00\x1cftypmp42\x00\x00\x00\x00mp42mp41isom" + b"\x00" * 128

    # Mock FFmpeg process success (returncode 0)
    mock_proc = MagicMock()
    mock_proc.returncode = 0
    mock_proc.communicate = AsyncMock(return_value=(b"", b""))

    with patch("asyncio.create_subprocess_exec", return_value=mock_proc) as mock_exec:
        verified = await verify_and_save_download(valid_mp4, "video")
        assert verified == valid_mp4
        assert mock_exec.called
        # Check command args: ffmpeg -v error -i file.partial -f null -
        call_args = mock_exec.call_args[0]
        assert call_args[0] == "ffmpeg"
        assert "-v" in call_args and "error" in call_args
        assert "-f" in call_args and "null" in call_args
        assert str(call_args[4]).endswith(".partial")


@pytest.mark.asyncio
async def test_verify_and_save_download_video_ffmpeg_decode_failure():
    valid_mp4 = b"\x00\x00\x00\x1cftypmp42\x00\x00\x00\x00mp42mp41isom" + b"\x00" * 128

    # Mock FFmpeg process failure (returncode 1, error message)
    mock_proc = MagicMock()
    mock_proc.returncode = 1
    mock_proc.communicate = AsyncMock(return_value=(b"", b"Invalid NAL unit 0, corrupt bitstream"))

    with patch("asyncio.create_subprocess_exec", return_value=mock_proc):
        with pytest.raises(UpstreamError, match="FFmpeg decode verification failed"):
            await verify_and_save_download(valid_mp4, "video")


@pytest.mark.asyncio
async def test_verify_and_save_download_video_ffmpeg_unavailable_fallback():
    valid_mp4 = b"\x00\x00\x00\x1cftypmp42\x00\x00\x00\x00mp42mp41isom" + b"\x00" * 64

    # When FFmpeg executable is not installed or found
    with patch("asyncio.create_subprocess_exec", side_effect=FileNotFoundError("ffmpeg not found")):
        verified = await verify_and_save_download(valid_mp4, "video")
        assert verified == valid_mp4


@pytest.mark.asyncio
async def test_verify_and_save_download_video_ffmpeg_unavailable_too_short():
    short_mp4 = b"\x00\x00\x00\x0cftyp"  # Less than 16 bytes

    with patch("asyncio.create_subprocess_exec", side_effect=FileNotFoundError("ffmpeg not found")):
        with pytest.raises(UpstreamError, match="file too small"):
            await verify_and_save_download(short_mp4, "video")


# =========================================================================
# P1-1: MediaStore Atomic Save with .partial
# =========================================================================

def test_media_store_atomic_save_partial(settings: Settings):
    store = MediaStore(settings.media_dir)
    data = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDRtest"
    name = store.save(data, "image/png", prefix="img")

    # Final file should exist
    saved_path = store.path_of(name)
    assert saved_path.is_file()
    assert saved_path.read_bytes() == data

    # No leftover .partial files in media directory
    partial_files = list(settings.media_dir.glob("*.partial"))
    assert len(partial_files) == 0


# =========================================================================
# P1-1 & P1-2: DOM scripts (fetch_as_base64, wait_video_mount, MEDIA_URLS_SNAPSHOT)
# =========================================================================

def test_dom_fetch_as_base64_error_guards():
    script = dom.fetch_as_base64("blob:https://muse.ai/test-id")
    # Must guard against HTTP errors and empty blob/buffer
    assert "!r.ok" in script
    assert "b.size === 0" in script
    assert "buf.length === 0" in script
    assert "empty blob (0 bytes)" in script


def test_dom_wait_video_mount_script():
    script = dom.wait_video_mount(timeout_ms=6000, interval_ms=100)
    assert "scrollTop = e.scrollHeight" in script
    assert "data-hatch-video-src" in script
    assert "currentSrc || v.src" in script
    assert "6000" in script
    assert "100" in script


def test_dom_media_urls_snapshot_script():
    script = dom.MEDIA_URLS_SNAPSHOT
    assert "data-hatch-video-src" in script
    assert "currentSrc || el.src" in script
    assert "new Set()" in script


# =========================================================================
# P1-2: Driver _wait_video_mount polling
# =========================================================================

@pytest.mark.asyncio
async def test_driver_wait_video_mount(settings: Settings):
    driver = BrowserDriver(settings)
    mock_session = MagicMock()
    tab = _Tab(account_id="acc1", context_id="ctx1", target_id="tgt1", session=mock_session)

    # When mounted successfully
    mock_session.evaluate = AsyncMock(return_value={"mounted": True, "elapsed_ms": 350})
    result = await driver._wait_video_mount(tab, timeout=6.0)
    assert result is True

    # When timeout (not mounted)
    mock_session.evaluate = AsyncMock(return_value={"mounted": False, "elapsed_ms": 6005})
    result_timeout = await driver._wait_video_mount(tab, timeout=6.0)
    assert result_timeout is False


# =========================================================================
# P1-3: URL Attachment Correlation Snapshot
# =========================================================================

@pytest.mark.asyncio
async def test_driver_snapshot_media_urls(settings: Settings):
    driver = BrowserDriver(settings)
    mock_session = MagicMock()
    tab = _Tab(account_id="acc1", context_id="ctx1", target_id="tgt1", session=mock_session)

    mock_session.evaluate = AsyncMock(return_value=[
        "https://muse.ai/media/vid1.mp4",
        "https://muse.ai/media/img1.png",
    ])
    urls = await driver._snapshot_media_urls(tab)
    assert urls == {"https://muse.ai/media/vid1.mp4", "https://muse.ai/media/img1.png"}


@pytest.mark.asyncio
async def test_wait_media_correlation_filters_existing_urls_and_mismatched_kind(settings: Settings):
    driver = BrowserDriver(settings)
    mock_session = MagicMock()
    tab = _Tab(account_id="acc1", context_id="ctx1", target_id="tgt1", session=mock_session)

    existing_url = "https://muse.ai/media/old_video.mp4"
    fresh_url = "https://muse.ai/media/fresh_video.mp4"
    image_url = "https://muse.ai/media/unrelated_image.png"

    base = {
        "agentCount": 1,
        "attachments": [{"src": existing_url, "kind": "video", "tid": "att-video-0"}],
        "existing_urls": {existing_url},
    }

    # Simulate page state poll returning:
    # 1. Old video (in existing_urls) -> MUST BE IGNORED
    # 2. Image (kind mismatch when expecting video) -> MUST BE IGNORED
    # 3. Fresh video (NOT in existing_urls, kind == 'video') -> MUST BE CHOSEN!
    st_mock = {
        "agentCount": 2,
        "generating": False,
        "approval": False,
        "lastText": "Here is your generated video!",
        "attachments": [
            {"src": existing_url, "kind": "video", "tid": "att-video-0"},
            {"src": image_url, "kind": "image", "tid": "att-image-1"},
            {"src": fresh_url, "kind": "video", "tid": "att-video-2"},
        ],
    }

    with patch.object(driver, "_state", new_callable=AsyncMock) as mock_st, \
         patch.object(driver, "_wait_video_mount", new_callable=AsyncMock, return_value=True):
        mock_st.return_value = st_mock
        result = await driver._wait_media(tab, base, "video", timeout=5.0, on_progress=None, cancel=None)
        assert result["src"] == fresh_url
        assert result["kind"] == "video"


@pytest.mark.asyncio
async def test_wait_media_fallback_link_bubble_filters_existing_urls(settings: Settings):
    driver = BrowserDriver(settings)
    mock_session = MagicMock()
    tab = _Tab(account_id="acc1", context_id="ctx1", target_id="tgt1", session=mock_session)

    existing_url = "https://musecdn.com/media/old_clip.mp4"
    fresh_url = "https://musecdn.com/media/new_clip.mp4"

    base = {
        "agentCount": 1,
        "attachments": [],
        "existing_urls": {existing_url},
    }

    call_count = 0
    def st_side_effect(*args, **kwargs):
        nonlocal call_count
        call_count += 1
        return {
            "agentCount": 1 + call_count,
            "generating": False,
            "approval": False,
            "lastText": "Done! Please see the link below.",
            "attachments": [],
        }

    bubble_media = {
        "links": [existing_url, fresh_url],
        "html": "<div>...</div>",
    }

    with patch.object(driver, "_state", new_callable=AsyncMock, side_effect=st_side_effect), \
         patch.object(driver, "_wait_video_mount", new_callable=AsyncMock, return_value=False), \
         patch.object(driver, "_send", new_callable=AsyncMock):
        mock_session.evaluate = AsyncMock(return_value=bubble_media)

        # Fast forward grace time
        driver._MEDIA_GRACE = {"video": 0.0, "image": 0.0}
        result = await driver._wait_media(tab, base, "video", timeout=5.0, on_progress=None, cancel=None)
        # Must pick fresh_url, NOT existing_url
        assert result["src"] == fresh_url
        assert result["kind"] == "video"


# =========================================================================
# P1-1: Driver _download with verification
# =========================================================================

@pytest.mark.asyncio
async def test_driver_download_verifies_and_returns_media_result(settings: Settings):
    driver = BrowserDriver(settings)
    mock_session = MagicMock()
    tab = _Tab(account_id="acc1", context_id="ctx1", target_id="tgt1", session=mock_session)

    valid_mp4 = b"\x00\x00\x00\x1cftypmp42\x00\x00\x00\x00mp42mp41isom" + b"\x00" * 64
    b64_str = base64.b64encode(valid_mp4).decode()

    mock_session.evaluate = AsyncMock(return_value={
        "ok": True,
        "mime": "video/mp4",
        "b64": b64_str,
    })

    # With fallback check (no ffmpeg needed)
    with patch("asyncio.create_subprocess_exec", side_effect=FileNotFoundError):
        res = await driver._download(tab, {"src": "blob:dummy", "w": 1280, "h": 720}, "video")
        assert res.data == valid_mp4
        assert res.mime == "video/mp4"
        assert res.kind == "video"
        assert res.width == 1280
        assert res.height == 720


@pytest.mark.asyncio
async def test_driver_download_corrupted_raises_upstream_error(settings: Settings):
    driver = BrowserDriver(settings)
    mock_session = MagicMock()
    tab = _Tab(account_id="acc1", context_id="ctx1", target_id="tgt1", session=mock_session)

    corrupt_bytes = b"CORRUPTED BYTES NO MAGIC"
    b64_str = base64.b64encode(corrupt_bytes).decode()

    mock_session.evaluate = AsyncMock(return_value={
        "ok": True,
        "mime": "video/mp4",
        "b64": b64_str,
    })

    with pytest.raises(UpstreamError, match="invalid magic bytes"):
        await driver._download(tab, {"src": "blob:dummy"}, "video")


# =========================================================================
# M6 Cluster B: GIF / AVIF / WebM Magic Bytes, FFmpeg CancelledError & MIME
# =========================================================================

def test_verify_magic_bytes_gif_avif_and_webm():
    gif87 = b"GIF87a" + b"\x00" * 16
    gif89 = b"GIF89a" + b"\x00" * 16
    assert verify_magic_bytes(gif87, "image") is True
    assert verify_magic_bytes(gif89, "image") is True
    assert verify_magic_bytes(gif87, "video") is False

    avif_major = b"\x00\x00\x00\x1cftypavif\x00\x00\x00\x00avifmif1"
    avis_major = b"\x00\x00\x00\x1cftypavis\x00\x00\x00\x00avismsf1"
    mif1_major = b"\x00\x00\x00\x1cftypmif1\x00\x00\x00\x00mif1heic"
    avif_compat = b"\x00\x00\x00\x1cftypisom\x00\x00\x00\x00isomavif"
    assert verify_magic_bytes(avif_major, "image") is True
    assert verify_magic_bytes(avis_major, "image") is True
    assert verify_magic_bytes(mif1_major, "image") is True
    assert verify_magic_bytes(avif_compat, "image") is True

    # Plain MP4 isom/mp42 without avif compatible brand must still be rejected for "image"
    mp4_isom = b"\x00\x00\x00\x18ftypisom\x00\x00\x00\x00isommp42"
    assert verify_magic_bytes(mp4_isom, "image") is False

    webm = b"\x1a\x45\xdf\xa3\x9f\x42\x86\x81\x01" + b"\x00" * 16
    assert verify_magic_bytes(webm, "video") is True
    assert verify_magic_bytes(webm, "image") is False


@pytest.mark.asyncio
async def test_verify_and_save_download_webm_fallback():
    valid_webm = b"\x1a\x45\xdf\xa3\x9f\x42\x86\x81\x01" + b"\x00" * 32
    with patch("asyncio.create_subprocess_exec", side_effect=FileNotFoundError("ffmpeg missing")):
        verified = await verify_and_save_download(valid_webm, "video")
    assert verified == valid_webm


@pytest.mark.asyncio
async def test_verify_and_save_download_kills_ffmpeg_on_cancelled_error():
    valid_mp4 = b"\x00\x00\x00\x1cftypmp42\x00\x00\x00\x00mp42mp41isom" + b"\x00" * 64
    started = asyncio.Event()
    temp_dir = Path(tempfile.gettempdir()) / "videoflow_downloads"
    before = set(temp_dir.iterdir()) if temp_dir.exists() else set()

    mock_proc = MagicMock()
    mock_proc.returncode = None
    mock_proc.kill = MagicMock()
    mock_proc.wait = AsyncMock(return_value=-9)

    async def blocking_communicate():
        started.set()
        await asyncio.Future()

    mock_proc.communicate = blocking_communicate

    with patch("asyncio.create_subprocess_exec", return_value=mock_proc):
        task = asyncio.create_task(verify_and_save_download(valid_mp4, "video"))
        await started.wait()
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task

    mock_proc.kill.assert_called_once()
    mock_proc.wait.assert_awaited_once()
    after = set(temp_dir.iterdir()) if temp_dir.exists() else set()
    new_files = after - before
    assert not any(p.suffix in (".partial", ".verified", ".mp4") for p in new_files)


@pytest.mark.parametrize(
    "payload,kind,upstream_mime,expected_mime",
    [
        (b"\x1a\x45\xdf\xa3\x9f\x42\x86\x81\x01" + b"\x00" * 32, "video", "", "video/webm"),
        (
            b"\x1a\x45\xdf\xa3\x9f\x42\x86\x81\x01" + b"\x00" * 32,
            "video",
            "application/octet-stream",
            "video/webm",
        ),
        (b"GIF89a" + b"\x00" * 24, "image", "", "image/gif"),
        (b"\x00\x00\x00\x1cftypavif\x00\x00\x00\x00avif", "image", "application/octet-stream", "image/avif"),
        (b"RIFF\x24\x00\x00\x00WEBPVP8 " + b"\x00" * 24, "image", "image/webp", "image/webp"),
        (b"\x00\x00\x00\x1cftypmp42\x00\x00\x00\x00mp42" + b"\x00" * 32, "video", "video/quicktime", "video/quicktime"),
    ],
)
@pytest.mark.asyncio
async def test_driver_download_infers_mime_fallback(
    settings: Settings, payload: bytes, kind: str, upstream_mime: str, expected_mime: str
):
    driver = BrowserDriver(settings)
    mock_session = MagicMock()
    tab = _Tab(account_id="acc1", context_id="ctx1", target_id="tgt1", session=mock_session)

    mock_session.evaluate = AsyncMock(return_value={
        "ok": True,
        "mime": upstream_mime,
        "b64": base64.b64encode(payload).decode(),
    })

    with patch("asyncio.create_subprocess_exec", side_effect=FileNotFoundError):
        res = await driver._download(tab, {"src": "blob:dummy"}, kind)

    assert res.data == payload
    assert res.mime == expected_mime
