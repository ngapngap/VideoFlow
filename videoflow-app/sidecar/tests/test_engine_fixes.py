"""Unit tests cho các bản vá VideoFlow trong engine (không cần mạng, không cần Edge).

Chạy: pytest -q  (trong thư mục videoflow-app/sidecar)
"""

from __future__ import annotations

import asyncio
import time
from contextlib import aclosing
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from muse2api.accounts.model import Account, AccountStatus, _extra_is_zero, _reset_deadline


def _acc(quota: dict | None = None, **kw) -> Account:
    a = Account(**kw)
    if quota is not None:
        a.meta["quota"] = quota
    return a


# ── Enforce hạn mức khi lease ────────────────────────────────────────────────


def test_quota_khong_co_snapshot_thi_van_dung_duoc():
    a = _acc()
    assert a.quota_exhausted() is False
    assert a.is_available() is True


def test_quota_duoi_100_van_dung_duoc():
    a = _acc({"weekly_used_pct": 87, "resets_at": "Jan 1, 2099"})
    assert a.is_available() is True


def test_quota_100_va_reset_tuong_lai_thi_bi_chan_lease():
    a = _acc({"weekly_used_pct": 100, "resets_at": "Jan 1, 2099", "extra_left": "0 tokens"})
    assert a.quota_exhausted() is True
    assert a.is_available() is False


def test_quota_100_nhung_con_quota_bo_sung_thi_van_dung_duoc():
    a = _acc({"weekly_used_pct": 100, "resets_at": "Jan 1, 2099", "extra_left": "1,200 tokens"})
    assert a.quota_exhausted() is False
    assert a.is_available() is True


def test_quota_100_nhung_reset_da_qua_thi_hoat_dong_lai():
    a = _acc({"weekly_used_pct": 100, "resets_at": "Jan 1, 2020", "extra_left": None})
    assert a.quota_exhausted() is False
    assert a.is_available() is True


def test_quota_dang_cooling_van_uu_tien_cooldown():
    a = _acc({"weekly_used_pct": 100, "resets_at": "Jan 1, 2099"},
             status=AccountStatus.COOLING)
    a.cooldown_until = time.time() + 3600
    assert a.is_available() is False
    a.cooldown_until = time.time() - 1
    # Cooldown hết nhưng quota vẫn cạn → vẫn bị chặn (đúng ý: hết quota là hết quota).
    assert a.is_available() is False


def test_quota_account_invalid_hoac_disabled():
    a = _acc(status=AccountStatus.INVALID)
    assert a.is_available() is False
    b = _acc(enabled=False)
    assert b.is_available() is False


def test_quota_shape_mock_driver_khong_bi_hieu_nham_la_het():
    """Driver mock trả messages_used/messages_limit, không có weekly_used_pct."""
    a = _acc({"messages_used": 100, "messages_limit": 100})
    assert a.quota_exhausted() is False
    assert a.is_available() is True


@pytest.mark.parametrize(
    "text,expected",
    [
        ("0 tokens", True),
        ("none", True),
        ("-", True),
        ("", True),
        ("1,200 tokens", False),
        ("0 / 5000", True),
        ("unknown balance", False),  # không parse được → coi như còn, không chặn
    ],
)
def test_extra_is_zero(text: str, expected: bool):
    assert _extra_is_zero(text) is expected


def test_reset_deadline_parse_duoc_nhieu_dinh_dang():
    now = time.time()
    assert _reset_deadline("Jan 1, 2099", now) > 0
    assert _reset_deadline("January 1, 2099", now) > 0
    assert _reset_deadline("2099-01-01", now) > 0
    assert _reset_deadline("Jan 1, 2020", now) < 0
    assert _reset_deadline(None, now) is None
    assert _reset_deadline("not a date", now) is None
    assert _reset_deadline("Jan 1, 2099 at 10:00 (GMT+7)", now) > 0


# ── Chặn trùng session (Fix 3) ───────────────────────────────────────────────


class _FakePool:
    def __init__(self, accounts):
        self._accounts = {a.id: a for a in accounts}

    def all(self):
        return list(self._accounts.values())


def test_patch_cung_bi_chan_khi_doi_sang_cookie_trung():
    """PATCH /admin/accounts/{id} phải kiểm tra trùng như POST (trước đây bị bypass)."""
    from muse2api.api.routes.admin import _check_duplicate_session
    from muse2api.errors import InvalidRequest

    other = Account(label="other", cookies={"hatch_sess": "SAME", "hatch_native_auth_device": "d"})
    target = Account(label="target", cookies={"hatch_sess": "OLD", "hatch_native_auth_device": "d"})
    pool = _FakePool([other, target])

    probe = Account(id=target.id, cookies={"hatch_sess": "SAME", "hatch_native_auth_device": "d"})
    with pytest.raises(InvalidRequest) as exc:
        _check_duplicate_session(probe, type("S", (), {"pool": pool})())
    assert "already belongs to account" in str(exc.value)

    # Chính account đó giữ cookie của nó → không được coi là trùng với bản thân.
    same = Account(id=target.id, cookies={"hatch_sess": "OLD", "hatch_native_auth_device": "d"})
    _check_duplicate_session(same, type("S", (), {"pool": pool})())


def test_duplicate_session_bo_qua_khi_hatch_sess_khac():
    from muse2api.api.routes.admin import _check_duplicate_session

    a = Account(cookies={"hatch_sess": "A"})
    pool = _FakePool([a])
    fresh = Account(cookies={"hatch_sess": "B"})
    _check_duplicate_session(fresh, type("S", (), {"pool": pool})())


# ── Fix 1: workspace bị cắt ─────────────────────────────────────────────────


def test_upstream_vm_disabled_khong_retry_va_danh_dau_invalid():
    from muse2api.accounts.pool import AccountPool
    from muse2api.errors import UpstreamVmDisabled

    err = UpstreamVmDisabled("workspace disabled")
    assert err.status_code == 503
    assert err.code == "upstream_vm_disabled"
    assert err.retryable is False

    acc = Account()
    pool = AccountPool.__new__(AccountPool)  # chỉ cần _apply_result
    pool.cooldown = 30
    pool._apply_result(acc, err)
    assert acc.status == AccountStatus.INVALID


# ── Issue #5 [P0.2] Fixes ───────────────────────────────────────────────────


def test_reset_deadline_b_d_parser_upcoming_occurrence():
    from datetime import datetime

    # Giả lập now vào ngày 15/09/2026 12:00:00 (trước Sep 30 năm nay)
    now_before = datetime(2026, 9, 15, 12, 0, 0).timestamp()
    deadline1 = _reset_deadline("Sep 30", now_before)
    assert deadline1 is not None
    assert deadline1 > 0
    dt1 = datetime.fromtimestamp(now_before + deadline1)
    assert (dt1.year, dt1.month, dt1.day) == (2026, 9, 30)

    # Giả lập now vào ngày 05/10/2026 12:00:00 (sau Sep 30 năm nay -> phải lấy năm tiếp theo)
    now_after = datetime(2026, 10, 5, 12, 0, 0).timestamp()
    deadline2 = _reset_deadline("Sep 30", now_after)
    assert deadline2 is not None
    assert deadline2 > 0
    dt2 = datetime.fromtimestamp(now_after + deadline2)
    assert (dt2.year, dt2.month, dt2.day) == (2027, 9, 30)

    # Có đuôi giờ/timezone
    deadline3 = _reset_deadline("Sep 30 at 10:00 (GMT+7)", now_before)
    assert deadline3 is not None
    assert deadline3 > 0

    # Format tên tháng đầy đủ %B %d
    deadline4 = _reset_deadline("September 30", now_before)
    assert deadline4 is not None
    assert deadline4 > 0


def test_quota_exhausted_fail_open_khi_khong_parse_duoc_resets_at():
    # Tài khoản 100% quota nhưng resets_at = None -> fail-open (không khóa, trả về False)
    a1 = _acc({"weekly_used_pct": 100, "resets_at": None, "extra_left": "0 tokens"})
    assert a1.quota_exhausted() is False
    assert a1.is_available() is True

    # Tài khoản 100% quota nhưng resets_at không thể parse -> fail-open (trả về False)
    a2 = _acc({"weekly_used_pct": 100, "resets_at": "unknown_format_xyz", "extra_left": "0 tokens"})
    assert a2.quota_exhausted() is False
    assert a2.is_available() is True

    # Đối chiếu: resets_at hợp lệ trong tương lai -> quota_exhausted là True
    a3 = _acc({"weekly_used_pct": 100, "resets_at": "Jan 1, 2099", "extra_left": "0 tokens"})
    assert a3.quota_exhausted() is True
    assert a3.is_available() is False


@pytest.mark.asyncio
async def test_patch_cookies_xoa_cookie_expires_cu():
    from muse2api.api.routes.admin import update_account
    from muse2api.api.schemas import AccountUpdate

    acc = Account(
        label="test_acc",
        cookies={"c1": "old_val1", "c2": "untouched_val2"},
        cookie_expires={"c1": 1700000000, "c2": 1800000000},
    )

    class _AdminPool:
        def __init__(self, item: Account):
            self.item = item

        def all(self):
            return [self.item]

        def get(self, account_id: str):
            return self.item if self.item.id == account_id else None

        async def upsert(self, account: Account):
            self.item = account

    pool = _AdminPool(acc)
    svc = type("S", (), {"pool": pool})()

    # PATCH cập nhật cookie c1 mới
    patch_body = AccountUpdate(cookies={"c1": "new_val1"})
    await update_account(acc.id, patch_body, svc=svc)

    # c1 có giá trị mới, c2 giữ nguyên
    assert acc.cookies["c1"] == "new_val1"
    assert acc.cookies["c2"] == "untouched_val2"

    # cookie_expires của c1 phải bị xóa; c2 không bị đụng tới phải được giữ nguyên
    assert "c1" not in acc.cookie_expires
    assert acc.cookie_expires["c2"] == 1800000000


@pytest.mark.asyncio
async def test_patch_cookies_kem_cookie_expires_moi():
    from muse2api.api.routes.admin import update_account
    from muse2api.api.schemas import AccountUpdate

    acc = Account(
        label="test_acc",
        cookies={"c1": "old_val1", "c2": "untouched_val2"},
        cookie_expires={"c1": 1700000000, "c2": 1800000000},
    )

    class _AdminPool:
        def __init__(self, item: Account):
            self.item = item

        def all(self):
            return [self.item]

        def get(self, account_id: str):
            return self.item if self.item.id == account_id else None

        async def upsert(self, account: Account):
            self.item = account

    pool = _AdminPool(acc)
    svc = type("S", (), {"pool": pool})()

    # PATCH cập nhật c1 kèm expiry mới
    patch_body = AccountUpdate(
        cookies={"c1": "new_val1"},
        cookie_expires={"c1": 1999999999},
    )
    await update_account(acc.id, patch_body, svc=svc)

    assert acc.cookies["c1"] == "new_val1"
    assert acc.cookie_expires["c1"] == 1999999999
    assert acc.cookie_expires["c2"] == 1800000000


def test_user_agent_chuan_hoa_windows_nt():
    from muse2api.upstream.muse import USER_AGENT

    assert "Windows NT 10.0; Win64; x64" in USER_AGENT
    assert USER_AGENT == (
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) "
        "Chrome/131.0.0.0 Safari/537.36"
    )


# =========================================================================
# M6 Cluster B: Startup & Driver Prewarm Eligibility + chat_stream finally
# =========================================================================


@pytest.mark.asyncio
async def test_app_startup_prewarm_skips_ineligible_accounts(tmp_path: Path):
    from muse2api.accounts.store import JsonAccountStore
    from muse2api.app import create_app
    from muse2api.config import Settings
    from muse2api.drivers.base import DriverCapabilities, MuseDriver

    settings = Settings(data_dir=tmp_path, media_dir=tmp_path / "media")
    settings.ensure_dirs()

    valid_cookies = {"hatch_sess": "s1", "hatch_native_auth_device": "d1"}
    accounts = [
        Account(id="acc_ok", label="ok", enabled=True, status=AccountStatus.ACTIVE, cookies=valid_cookies),
        Account(id="acc_disabled", label="dis", enabled=False, status=AccountStatus.ACTIVE, cookies=valid_cookies),
        Account(id="acc_invalid", label="inv", enabled=True, status=AccountStatus.INVALID, cookies=valid_cookies),
        Account(id="acc_cooling", label="cool", enabled=True, status=AccountStatus.COOLING, cookies=valid_cookies),
        Account(id="acc_vlocked", label="vl", enabled=True, status=AccountStatus.VIDEO_LOCKED, cookies=valid_cookies),
        _acc({"weekly_used_pct": 100, "resets_at": "Jan 1, 2099", "extra_left": "0 tokens"},
             id="acc_quota", label="q", enabled=True, status=AccountStatus.ACTIVE, cookies=valid_cookies),
        Account(id="acc_nocookie", label="nc", enabled=True, status=AccountStatus.ACTIVE, cookies={"hatch_sess": "s1"}),
    ]
    await JsonAccountStore(settings.accounts_file).save(accounts)

    prewarmed: list[str] = []

    class _FakeDriver(MuseDriver):
        name = "fake"
        capabilities = DriverCapabilities(chat=True)

        async def startup(self) -> None:
            return

        async def shutdown(self) -> None:
            return

        def prewarm(self, account: Account) -> None:
            prewarmed.append(account.id)

        async def chat_stream(self, account, req):
            yield "ok"

    app = create_app(settings, driver=_FakeDriver())
    async with app.router.lifespan_context(app):
        pass

    assert prewarmed == ["acc_ok"]


@pytest.mark.asyncio
async def test_browser_driver_prewarm_skips_ineligible_and_aborts_fill_warm(tmp_path: Path):
    from muse2api.config import Settings
    from muse2api.drivers.browser.driver import BrowserDriver, _Tab
    from muse2api.errors import UpstreamAuthError, UpstreamQuotaError, UpstreamVmDisabled

    settings = Settings(data_dir=tmp_path, media_dir=tmp_path / "media", warm_threads=1)
    drv = BrowserDriver(settings)
    drv._browser = MagicMock()

    valid_cookies = {"hatch_sess": "s1", "hatch_native_auth_device": "d1"}
    ineligible = [
        Account(id="dis", enabled=False, status=AccountStatus.ACTIVE, cookies=valid_cookies),
        Account(id="inv", enabled=True, status=AccountStatus.INVALID, cookies=valid_cookies),
        Account(id="cool", enabled=True, status=AccountStatus.COOLING, cookies=valid_cookies),
        Account(id="vl", enabled=True, status=AccountStatus.VIDEO_LOCKED, cookies=valid_cookies),
        _acc({"weekly_used_pct": 100, "resets_at": "Jan 1, 2099", "extra_left": "0 tokens"},
             id="q", enabled=True, status=AccountStatus.ACTIVE, cookies=valid_cookies),
        Account(id="nc", enabled=True, status=AccountStatus.ACTIVE, cookies={"hatch_sess": "s1"}),
    ]
    for acc in ineligible:
        drv.prewarm(acc)
    assert drv._prewarm_tasks == {}
    assert drv._warming == {}

    # _fill_warm must abort immediately on non-transient errors without sleeping _WARM_RETRY_DELAY
    for exc_cls in (UpstreamAuthError, UpstreamVmDisabled, UpstreamQuotaError):
        acc = Account(id="ok", enabled=True, status=AccountStatus.ACTIVE, cookies=valid_cookies)
        sub = BrowserDriver(settings, account=acc)
        sub._browser = MagicMock()
        tab = _Tab(acc.id, "ctx", "tgt", MagicMock())
        sleep_mock = AsyncMock()
        with (
            patch.object(sub, "_checkout_for_warming", AsyncMock(side_effect=[tab, None])),
            patch.object(sub, "_warm_up", AsyncMock(side_effect=exc_cls("stop"))),
            patch.object(sub, "_checkin", AsyncMock()),
            patch("muse2api.drivers.browser.driver.asyncio.sleep", sleep_mock),
        ):
            await sub._fill_warm(acc)
        sleep_mock.assert_not_called()

    # _fill_warm stops before next round if account becomes ineligible mid-way
    acc2 = Account(id="ok2", enabled=True, status=AccountStatus.ACTIVE, cookies=valid_cookies)
    sub2 = BrowserDriver(settings, account=acc2)
    sub2._browser = MagicMock()
    tab2 = _Tab(acc2.id, "ctx", "tgt", MagicMock())

    async def warm_then_invalidate(_tab):
        acc2.status = AccountStatus.INVALID

    sleep_mock2 = AsyncMock()
    with (
        patch.object(sub2, "_checkout_for_warming", AsyncMock(side_effect=[tab2, None])),
        patch.object(sub2, "_warm_up", side_effect=warm_then_invalidate),
        patch.object(sub2, "_checkin", AsyncMock()),
        patch("muse2api.drivers.browser.driver.asyncio.sleep", sleep_mock2),
    ):
        await sub2._fill_warm(acc2)
    sleep_mock2.assert_not_called()


@pytest.mark.asyncio
async def test_chat_stream_finally_prewarms_only_on_clean_finish(tmp_path: Path):
    from muse2api.config import Settings
    from muse2api.drivers.base import ChatRequest
    from muse2api.drivers.browser.cdp import CDPError
    from muse2api.drivers.browser.driver import BrowserDriver, _Tab
    from muse2api.errors import UpstreamError, UpstreamQuotaError

    settings = Settings(data_dir=tmp_path, media_dir=tmp_path / "media")
    acc = Account(
        id="acc_1",
        enabled=True,
        status=AccountStatus.ACTIVE,
        cookies={"hatch_sess": "s1", "hatch_native_auth_device": "d1"},
    )

    async def _run_case(mode: str):
        drv = BrowserDriver(settings, account=acc)
        tab = _Tab(acc.id, "ctx", "tgt", MagicMock())
        prewarm_mock = MagicMock()
        cancel = asyncio.Event()

        poll = 0

        async def fake_state(_tab):
            nonlocal poll
            poll += 1
            if mode == "quota":
                raise UpstreamQuotaError("out of quota")
            if mode == "cdp":
                raise CDPError("connection closed")
            if mode == "cancel":
                cancel.set()
                return {"agentCount": 0, "lastText": ""}
            if mode == "early_close":
                return {"agentCount": 1, "lastText": "partial", "generating": True}
            return {"agentCount": 1, "lastText": "hello", "generating": False}

        with (
            patch("muse2api.drivers.browser.driver.POLL_INTERVAL", 0.0),
            patch("muse2api.drivers.browser.driver.STABLE_POLLS_DONE", 1),
            patch.object(drv, "_begin_chat", AsyncMock(return_value=(tab, {"agentCount": 0, "lastText": ""}))),
            patch.object(drv, "_state", side_effect=fake_state),
            patch.object(drv, "_checkin", AsyncMock()),
            patch.object(drv, "_close_tab", AsyncMock()),
            patch.object(drv, "prewarm", prewarm_mock),
        ):
            req = ChatRequest(prompt="hi", model="muse-chat", cancel=cancel)
            if mode == "ok":
                chunks = [c async for c in drv.chat_stream(acc, req)]
                assert chunks == ["hello"]
            elif mode == "cancel":
                chunks = [c async for c in drv.chat_stream(acc, req)]
                assert chunks == []
            elif mode == "early_close":
                async with aclosing(drv.chat_stream(acc, req)) as stream:
                    first = await anext(stream)
                    assert first == "partial"
            elif mode == "quota":
                with pytest.raises(UpstreamQuotaError):
                    async for _ in drv.chat_stream(acc, req):
                        pass
            elif mode == "cdp":
                with pytest.raises(UpstreamError):
                    async for _ in drv.chat_stream(acc, req):
                        pass
        return prewarm_mock

    ok_mock = await _run_case("ok")
    ok_mock.assert_called_once_with(acc)

    for bad_mode in ("quota", "cdp", "cancel", "early_close"):
        bad_mock = await _run_case(bad_mode)
        bad_mock.assert_not_called()
