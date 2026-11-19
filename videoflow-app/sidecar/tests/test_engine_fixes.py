"""Unit tests cho các bản vá VideoFlow trong engine (không cần mạng, không cần Edge).

Chạy: pytest -q  (trong thư mục videoflow-app/sidecar)
"""

from __future__ import annotations

import time

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
