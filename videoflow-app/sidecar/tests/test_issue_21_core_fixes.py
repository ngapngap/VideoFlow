from __future__ import annotations

import asyncio
import time
from datetime import datetime
from pathlib import Path

import pytest
from pydantic import ValidationError

from muse2api.accounts.model import Account, AccountStatus, _reset_deadline
from muse2api.accounts.pool import AccountPool
from muse2api.accounts.store import JsonAccountStore
from muse2api.api.schemas import AccountCreate, AccountUpdate
from muse2api.drivers.browser.driver import verify_magic_bytes
from muse2api.services.request_log import RequestLog
from muse2api.services.tasks import Task, TaskManager, TaskStatus


def test_reset_deadline_anchors_year_to_checked_at_and_supports_feb29_and_zh():
    checked = datetime(2026, 9, 25, 12, 0, 0).timestamp()
    before_reset = datetime(2026, 9, 28, 12, 0, 0).timestamp()
    after_reset = datetime(2026, 10, 2, 12, 0, 0).timestamp()

    assert (_reset_deadline("Sep 30", before_reset, checked_at=checked) or 0) > 0
    assert (_reset_deadline("Sep 30", after_reset, checked_at=checked) or 0) < 0

    # Feb 29 parses cleanly without crashing
    feb_checked = datetime(2024, 2, 20, 12, 0, 0).timestamp()
    feb_now = datetime(2024, 2, 25, 12, 0, 0).timestamp()
    assert (_reset_deadline("Feb 29", feb_now, checked_at=feb_checked) or 0) > 0

    # Chinese M月D日 format
    zh_checked = datetime(2026, 11, 20, 10, 0, 0).timestamp()
    zh_now = datetime(2026, 11, 21, 10, 0, 0).timestamp()
    assert (_reset_deadline("11月25日", zh_now, checked_at=zh_checked) or 0) > 0

    # ISO timestamp strips T10:00:00Z properly
    iso_now = datetime(2026, 11, 20, 10, 0, 0).timestamp()
    assert (_reset_deadline("2026-11-25T10:00:00Z", iso_now) or 0) > 0


def test_verify_magic_bytes_webp_strict():
    assert verify_magic_bytes(b"RIFF\x00\x00\x00\x00WEBP", "image") is True
    assert verify_magic_bytes(b"RIFF\x00\x00", "image") is False
    assert verify_magic_bytes(b"RIFF\x00\x00\x00\x00WAVE", "image") is False


def test_proxy_url_validation_in_schemas():
    assert AccountCreate(proxy="http://user:pass@1.2.3.4:8188").proxy == "http://user:pass@1.2.3.4:8188"
    assert AccountCreate(proxy="socks5://127.0.0.1:1080").proxy == "socks5://127.0.0.1:1080"
    assert AccountUpdate(proxy="").proxy == ""
    assert AccountUpdate(proxy=None).proxy is None

    with pytest.raises(ValidationError):
        AccountCreate(proxy="ftp://1.2.3.4:21")
    with pytest.raises(ValidationError):
        AccountUpdate(proxy="not-a-proxy")


def test_pool_does_not_clear_cooling_before_cooldown_until(tmp_path: Path):
    pool = AccountPool(JsonAccountStore(tmp_path / "accounts.json"))
    acc = Account(
        id="acc_cool",
        status=AccountStatus.COOLING,
        cooldown_until=time.time() + 600,
    )
    pool._apply_result(acc, None)
    assert acc.status == AccountStatus.COOLING

    acc.cooldown_until = time.time() - 10
    pool._apply_result(acc, None)
    assert acc.status == AccountStatus.ACTIVE


@pytest.mark.asyncio
async def test_json_account_store_backs_up_corrupt_file(tmp_path: Path):
    p = tmp_path / "accounts.json"
    p.write_text("{corrupt json", encoding="utf-8")
    store = JsonAccountStore(p)
    loaded = await store.load()
    assert loaded == []
    assert (tmp_path / "accounts.json.corrupt").is_file()


def test_task_manager_never_evicts_running_tasks(tmp_path: Path):
    mgr = TaskManager(tmp_path / "tasks.json", max_kept=3)
    running = Task(id="task_run", kind="video", status=TaskStatus.RUNNING, created_at=1.0)
    mgr._tasks[running.id] = running
    for i in range(5):
        t = Task(id=f"task_done_{i}", kind="video", status=TaskStatus.SUCCEEDED, created_at=10.0 + i)
        mgr._tasks[t.id] = t
    mgr._persist()
    assert "task_run" in mgr._tasks
    assert len(mgr._tasks) == 3


@pytest.mark.asyncio
async def test_request_log_closed_prevents_reopen(tmp_path: Path):
    rl = RequestLog(tmp_path / "requests.db")
    await rl.open()
    await rl.close()
    with pytest.raises(RuntimeError, match="closed"):
        await asyncio.to_thread(rl._db)
