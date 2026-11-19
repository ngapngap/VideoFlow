"""Unit tests for Milestone 5: Issue #8 [P1.2]
- P1-6: Poll waiting for upload progressbar to disappear before send (timeout 45s)
- P1-4: Video locked detection, AccountStatus.VIDEO_LOCKED, and non-retryable handling
"""

from __future__ import annotations

import asyncio
import time
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest
from muse2api.accounts.model import Account, AccountStatus
from muse2api.accounts.pool import AccountPool
from muse2api.accounts.store import JsonAccountStore
from muse2api.config import Settings
from muse2api.drivers.base import InputImage, MediaResult, VideoRequest
from muse2api.drivers.browser import dom
from muse2api.drivers.browser.driver import BrowserDriver, _Tab
from muse2api.errors import (
    NoAccountAvailable,
    UpstreamError,
    UpstreamRefused,
    UpstreamTimeout,
    UpstreamVideoLocked,
)
from muse2api.services.gateway import Gateway


@pytest.fixture
def settings(tmp_path: Path) -> Settings:
    s = Settings(data_dir=tmp_path, media_dir=tmp_path / "media")
    s.media_dir.mkdir(parents=True, exist_ok=True)
    return s


# =========================================================================
# P1-6: Reference Image Upload Polling (ProgressBar / aria-busy)
# =========================================================================

def test_upload_busy_dom_script_defined():
    """Verify CHECK_UPLOAD_BUSY script checks role=progressbar, aria-busy=true, and input area."""
    script = dom.CHECK_UPLOAD_BUSY
    assert "progressbar" in script
    assert "aria-busy" in script
    assert "chat-input" in script or "input" in script


@pytest.mark.asyncio
async def test_wait_upload_complete_instant_when_not_busy(settings: Settings):
    """When no upload progressbar appears, _wait_upload_complete finishes promptly."""
    driver = BrowserDriver(settings)
    mock_session = MagicMock()
    mock_session.evaluate = AsyncMock(return_value=False)
    tab = _Tab("acc_1", "ctx_1", "t_1", mock_session)

    start = time.monotonic()
    await driver._wait_upload_complete(tab, timeout=5.0)
    elapsed = time.monotonic() - start

    assert elapsed < 1.5
    assert mock_session.evaluate.called


@pytest.mark.asyncio
async def test_wait_upload_complete_polls_until_progressbar_disappears(settings: Settings):
    """When progressbar is active for several polls, polls every 200ms and returns when done."""
    driver = BrowserDriver(settings)
    mock_session = MagicMock()
    # Return True for 3 polls, then False
    mock_session.evaluate = AsyncMock(side_effect=[True, True, True, False, False])
    tab = _Tab("acc_1", "ctx_1", "t_1", mock_session)

    start = time.monotonic()
    await driver._wait_upload_complete(tab, timeout=10.0)
    elapsed = time.monotonic() - start

    # Sửa: Có ít nhất 3 lần True (mỗi lần ~0.2s) nên elapsed >= 0.5s
    assert elapsed >= 0.5
    assert mock_session.evaluate.call_count >= 4


@pytest.mark.asyncio
async def test_wait_upload_complete_timeout_raises_upstream_timeout(settings: Settings):
    """When progressbar stays active past timeout, raises UpstreamTimeout."""
    driver = BrowserDriver(settings)
    mock_session = MagicMock()
    mock_session.evaluate = AsyncMock(return_value=True)
    tab = _Tab("acc_1", "ctx_1", "t_1", mock_session)

    # Use a short timeout for test speed
    with pytest.raises(UpstreamTimeout, match="reference image upload timed out after 0s"):
        await driver._wait_upload_complete(tab, timeout=0.3)


@pytest.mark.asyncio
async def test_attach_with_images_triggers_wait_upload_complete(settings: Settings):
    """_attach calls _wait_upload_complete when images are provided."""
    driver = BrowserDriver(settings)
    mock_session = MagicMock()
    mock_session.evaluate = AsyncMock(side_effect=[
        {"hasFileInput": True},  # page_state has file input
        {"ok": True},             # attach_file ok
        False,                    # CHECK_UPLOAD_BUSY poll 1
        False,                    # CHECK_UPLOAD_BUSY confirm
    ])
    tab = _Tab("acc_1", "ctx_1", "t_1", mock_session)

    img = InputImage(data=b"\x89PNG\r\n\x1a\n" + b"\x00" * 32, mime="image/png")
    await driver._attach(tab, [img])

    assert mock_session.evaluate.call_count >= 3


@pytest.mark.asyncio
async def test_attach_without_images_skips_wait(settings: Settings):
    """_attach does nothing and does not wait when images list is empty."""
    driver = BrowserDriver(settings)
    mock_session = MagicMock()
    tab = _Tab("acc_1", "ctx_1", "t_1", mock_session)

    await driver._attach(tab, [])
    assert mock_session.evaluate.call_count == 0


@pytest.mark.asyncio
async def test_attach_timeout_prevents_sending_prompt(settings: Settings):
    """If upload times out in _attach, exception is raised before _send can be called."""
    driver = BrowserDriver(settings)
    mock_session = MagicMock()
    mock_session.evaluate = AsyncMock(side_effect=[
        {"hasFileInput": True},
        {"ok": True},
        True,  # upload busy indefinitely
        True,
        True,
        True,
    ])
    tab = _Tab("acc_1", "ctx_1", "t_1", mock_session)

    img = InputImage(data=b"\x89PNG\r\n\x1a\n" + b"\x00" * 32, mime="image/png")
    with patch.object(driver, "_UPLOAD_TIMEOUT", 0.3):
        with pytest.raises(UpstreamTimeout):
            await driver._attach(tab, [img])


# =========================================================================
# P1-4: Video Locked Detection (Hints & _wait_media)
# =========================================================================

def test_video_locked_hints_contain_required_phrases():
    """Verify all 5 required video locked phrases are in dom.VIDEO_LOCKED_HINTS."""
    required = [
        "tính năng tạo video không khả dụng",
        "video không khả dụng",
        "không thể tạo video",
        "video generation is not available",
        "unable to generate video",
    ]
    for phrase in required:
        assert phrase in dom.VIDEO_LOCKED_HINTS


@pytest.mark.parametrize("locked_text", [
    "Tính năng tạo video không khả dụng trên tài khoản của bạn.",
    "Rất tiếc, video không khả dụng tại khu vực này.",
    "Hệ thống không thể tạo video cho bạn lúc này.",
    "Video generation is not available for this account.",
    "We are unable to generate video at this moment.",
])
@pytest.mark.asyncio
async def test_wait_media_detects_video_locked_phrases(settings: Settings, locked_text: str):
    """When kind=='video', not generating, and Muse replies with locked text, raises UpstreamVideoLocked."""
    driver = BrowserDriver(settings)
    mock_session = MagicMock()
    tab = _Tab("acc_1", "ctx_1", "t_1", mock_session)

    base = {"attachments": [], "agentCount": 0, "existing_urls": set()}
    mock_state = {
        "generating": False,
        "agentCount": 1,
        "lastText": locked_text,
        "tail": locked_text,
        "attachments": [],
    }

    with patch.object(driver, "_state", AsyncMock(return_value=mock_state)):
        with pytest.raises(UpstreamVideoLocked) as exc_info:
            await driver._wait_media(tab, base, kind="video", timeout=5.0, on_progress=None, cancel=None)
        assert "video generation is locked" in exc_info.value.message


@pytest.mark.asyncio
async def test_wait_media_detects_video_locked_in_tail(settings: Settings):
    """Detects video locked even if lastText is brief but tail contains the hint."""
    driver = BrowserDriver(settings)
    mock_session = MagicMock()
    tab = _Tab("acc_1", "ctx_1", "t_1", mock_session)

    base = {"attachments": [], "agentCount": 0, "existing_urls": set()}
    mock_state = {
        "generating": False,
        "agentCount": 1,
        "lastText": "Thông báo",
        "tail": "...tính năng tạo video không khả dụng...",
        "attachments": [],
    }

    with patch.object(driver, "_state", AsyncMock(return_value=mock_state)):
        with pytest.raises(UpstreamVideoLocked):
            await driver._wait_media(tab, base, kind="video", timeout=5.0, on_progress=None, cancel=None)


@pytest.mark.asyncio
async def test_wait_media_does_not_raise_video_locked_if_video_produced(settings: Settings):
    """If a valid new video attachment was produced, does NOT raise video locked even if text has phrase."""
    driver = BrowserDriver(settings)
    mock_session = MagicMock()
    tab = _Tab("acc_1", "ctx_1", "t_1", mock_session)

    base = {"attachments": [], "agentCount": 0, "existing_urls": set()}
    mock_state = {
        "generating": False,
        "agentCount": 1,
        "lastText": "tính năng tạo video không khả dụng",
        "tail": "",
        "attachments": [{"src": "https://cdn.muse.ai/v1.mp4", "kind": "video"}],
    }

    with patch.object(driver, "_state", AsyncMock(return_value=mock_state)):
        result = await driver._wait_media(tab, base, kind="video", timeout=5.0, on_progress=None, cancel=None)
        assert result["src"] == "https://cdn.muse.ai/v1.mp4"


@pytest.mark.asyncio
async def test_wait_media_does_not_raise_video_locked_while_generating(settings: Settings):
    """While generating is True, does not conclude video locked."""
    driver = BrowserDriver(settings)
    mock_session = MagicMock()
    tab = _Tab("acc_1", "ctx_1", "t_1", mock_session)

    base = {"attachments": [], "agentCount": 0, "existing_urls": set()}
    mock_state_busy = {
        "generating": True,
        "agentCount": 1,
        "lastText": "tính năng tạo video không khả dụng",
        "tail": "",
        "attachments": [],
    }

    # Should time out rather than concluding video locked while generating is still True
    with patch.object(driver, "_state", AsyncMock(return_value=mock_state_busy)):
        with pytest.raises(UpstreamTimeout):
            await driver._wait_media(tab, base, kind="video", timeout=0.8, on_progress=None, cancel=None)


@pytest.mark.asyncio
async def test_wait_media_does_not_raise_video_locked_for_images(settings: Settings):
    """When kind=='image', video locked hints do not trigger UpstreamVideoLocked."""
    driver = BrowserDriver(settings)
    mock_session = MagicMock()
    mock_session.evaluate = AsyncMock(return_value={"links": [], "html": ""})
    tab = _Tab("acc_1", "ctx_1", "t_1", mock_session)

    base = {"attachments": [], "agentCount": 0, "existing_urls": set()}
    mock_state = {
        "generating": False,
        "agentCount": 1,
        "lastText": "tính năng tạo video không khả dụng",
        "tail": "",
        "attachments": [],
    }

    with patch.object(driver, "_state", AsyncMock(return_value=mock_state)), \
         patch.object(driver, "_send", AsyncMock()):
        with pytest.raises(UpstreamError) as exc_info:
            await driver._wait_media(tab, base, kind="image", timeout=0.8, on_progress=None, cancel=None)
        assert not isinstance(exc_info.value, UpstreamVideoLocked)


# =========================================================================
# P1-4: Account Model & Pool video_locked State Handling
# =========================================================================

def test_account_model_video_locked_status():
    """Verify AccountStatus.VIDEO_LOCKED enum value and is_available behavior."""
    assert AccountStatus.VIDEO_LOCKED == "video_locked"

    acc_active = Account(id="acc_1", status=AccountStatus.ACTIVE, enabled=True)
    assert acc_active.is_available() is True

    acc_locked = Account(id="acc_2", status=AccountStatus.VIDEO_LOCKED, enabled=True)
    assert acc_locked.is_available() is False

    acc_invalid = Account(id="acc_3", status=AccountStatus.INVALID, enabled=True)
    assert acc_invalid.is_available() is False


def test_upstream_video_locked_is_non_retryable():
    """UpstreamVideoLocked must have retryable=False and code=upstream_video_locked."""
    err = UpstreamVideoLocked("video feature is locked")
    assert err.retryable is False
    assert err.code == "upstream_video_locked"
    assert err.status_code == 403


@pytest.mark.asyncio
async def test_pool_apply_result_sets_video_locked(tmp_path: Path):
    """Releasing an account with UpstreamVideoLocked sets status to VIDEO_LOCKED."""
    store = JsonAccountStore(tmp_path / "accounts.json")
    pool = AccountPool(store)
    acc = Account(id="acc_1", label="Test", status=AccountStatus.ACTIVE, enabled=True)
    await pool.upsert(acc)

    await pool.release(acc, UpstreamVideoLocked("feature locked"))

    updated = pool.get("acc_1")
    assert updated is not None
    assert updated.status == AccountStatus.VIDEO_LOCKED
    assert updated.fail_count == 1
    assert "feature locked" in (updated.last_error or "")


@pytest.mark.asyncio
async def test_pool_stats_reports_video_locked(tmp_path: Path):
    """pool.stats() accurately reports count of video_locked accounts."""
    store = JsonAccountStore(tmp_path / "accounts.json")
    pool = AccountPool(store)
    acc1 = Account(id="acc_1", status=AccountStatus.ACTIVE, enabled=True)
    acc2 = Account(id="acc_2", status=AccountStatus.VIDEO_LOCKED, enabled=True)
    await pool.upsert(acc1)
    await pool.upsert(acc2)

    stats = pool.stats()
    assert stats["total"] == 2
    assert stats["available"] == 1
    assert stats["video_locked"] == 1


@pytest.mark.asyncio
async def test_pool_does_not_allocate_video_locked_account(tmp_path: Path):
    """A video_locked account is not acquired and raises NoAccountAvailable."""
    store = JsonAccountStore(tmp_path / "accounts.json")
    pool = AccountPool(store, acquire_timeout=0.2)
    acc = Account(id="acc_locked", status=AccountStatus.VIDEO_LOCKED, enabled=True)
    await pool.upsert(acc)

    assert pool.has_alternative(()) is False
    with pytest.raises(NoAccountAvailable, match="no usable account in pool"):
        await pool.acquire()


@pytest.mark.asyncio
async def test_gateway_does_not_retry_on_upstream_video_locked(tmp_path: Path):
    """Gateway halts immediately on UpstreamVideoLocked without retrying."""
    store = JsonAccountStore(tmp_path / "accounts.json")
    pool = AccountPool(store)
    acc1 = Account(id="acc_1", status=AccountStatus.ACTIVE, enabled=True)
    acc2 = Account(id="acc_2", status=AccountStatus.ACTIVE, enabled=True)
    await pool.upsert(acc1)
    await pool.upsert(acc2)

    mock_driver = MagicMock()
    mock_driver.requires_account = True
    call_count = 0

    async def mock_generate_video(account, req):
        nonlocal call_count
        call_count += 1
        raise UpstreamVideoLocked("muse.ai video is locked")

    mock_driver.generate_video = mock_generate_video

    gateway = Gateway(pool, mock_driver, max_failover=2)
    req = VideoRequest(prompt="test video", model="muse-video")

    with pytest.raises(UpstreamVideoLocked):
        await gateway.generate_video(req)

    # Must NOT failover or retry
    assert call_count == 1
    # The account that failed must now be marked VIDEO_LOCKED
    acc1_updated = pool.get("acc_1")
    acc2_updated = pool.get("acc_2")
    locked_accounts = [a for a in (acc1_updated, acc2_updated) if a and a.status == AccountStatus.VIDEO_LOCKED]
    assert len(locked_accounts) == 1
