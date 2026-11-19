"""Test tầng POOL (acquire/lease) — lấp khoảng trống khiến bug "treo 60s khi hết quota"
và "has_alternative bỏ qua quota" từng lọt qua.

Trước đây 19 test chỉ phủ model/helper, không test nào gọi `acquire()`.
"""

from __future__ import annotations

import time

import pytest

from muse2api.accounts.model import Account, AccountStatus
from muse2api.accounts.pool import AccountPool
from muse2api.errors import NoAccountAvailable


class _MemStore:
    """AccountStore in-memory — không chạm đĩa."""

    def __init__(self, accounts: list[Account] | None = None) -> None:
        self._items = accounts or []

    async def load(self) -> list[Account]:
        return list(self._items)

    async def save(self, accounts: list[Account]) -> None:
        self._items = list(accounts)


def _exhausted(**kw) -> Account:
    a = Account(**kw)
    a.meta["quota"] = {
        "weekly_used_pct": 100,
        "resets_at": "Jan 1, 2099",
        "extra_left": "0B tokens left",
        "found": True,
    }
    return a


def _healthy(**kw) -> Account:
    a = Account(**kw)
    a.meta["quota"] = {"weekly_used_pct": 12, "resets_at": "Jan 1, 2099", "found": True}
    return a


async def _pool_with(accounts: list[Account], *, timeout: float = 60.0) -> AccountPool:
    pool = AccountPool(_MemStore(accounts), acquire_timeout=timeout)
    await pool.load()
    return pool


# ── Bug 2: account cạn quota làm acquire() treo tới hết timeout ──────────────


@pytest.mark.asyncio
async def test_acquire_fail_nhanh_khi_account_het_quota():
    """1 account ACTIVE nhưng cạn quota, cooldown_until = 0.0 → phải fail NGAY,
    không được coi là 'đang cooling' rồi chờ hết acquire_timeout."""
    pool = await _pool_with([_exhausted()], timeout=60.0)
    started = time.monotonic()
    with pytest.raises(NoAccountAvailable) as exc:
        await pool.acquire()
    elapsed = time.monotonic() - started
    assert elapsed < 2.0, f"acquire() treo {elapsed:.1f}s (phải fail nhanh)"
    assert "out of quota" in str(exc.value), (
        f"message phải nói hết quota, nhận: {exc.value}"
    )


@pytest.mark.asyncio
async def test_acquire_bao_dung_su_that_khi_vua_het_quota_vua_cooling():
    cooling = Account(status=AccountStatus.COOLING)
    cooling.cooldown_until = time.time() + 3600
    pool = await _pool_with([_exhausted(), cooling], timeout=5.0)
    started = time.monotonic()
    with pytest.raises(NoAccountAvailable) as exc:
        await pool.acquire()
    elapsed = time.monotonic() - started
    # Account cooling còn 1 giờ > timeout 5s → fail sớm, message nói cooling.
    assert elapsed < 2.0
    assert "cooling" in str(exc.value)


@pytest.mark.asyncio
async def test_acquire_bo_qua_account_het_quota_va_chon_account_khoe():
    bad = _exhausted(label="bad")
    good = _healthy(label="good")
    pool = await _pool_with([bad, good])
    acc = await pool.acquire()
    assert acc.id == good.id
    await pool.release(acc)


# ── Bug 3: has_alternative bỏ qua quota ──────────────────────────────────────


@pytest.mark.asyncio
async def test_has_alternative_false_khi_account_con_lai_het_quota():
    only = _exhausted()
    pool = await _pool_with([only])
    assert pool.has_alternative(exclude=set()) is False, (
        "account cạn quota không được coi là alternative — nếu không gateway sẽ retry "
        "vào pool chỉ trả 503"
    )
    # Và cũng không có alternative nào khác khi đã loại chính nó.
    assert pool.has_alternative(exclude={only.id}) is False


@pytest.mark.asyncio
async def test_has_alternative_true_voi_account_khoe():
    a, b = _exhausted(), _healthy()
    pool = await _pool_with([a, b])
    assert pool.has_alternative(exclude={b.id}) is False
    assert pool.has_alternative(exclude=set()) is True


# ── hồi phục tự nhiên ───────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_quota_het_nhung_qua_reset_thi_lease_lai_duoc():
    a = _exhausted()
    a.meta["quota"] = {
        "weekly_used_pct": 100,
        "resets_at": "Jan 1, 2020",  # đã qua
        "extra_left": "0 tokens",
        "found": True,
    }
    pool = await _pool_with([a], timeout=2.0)
    acc = await pool.acquire()
    assert acc.id == a.id
    await pool.release(acc)


@pytest.mark.asyncio
async def test_con_quota_bo_sung_thi_van_lease_duoc():
    a = _exhausted()
    a.meta["quota"]["extra_left"] = "1.2B tokens left"
    pool = await _pool_with([a], timeout=2.0)
    acc = await pool.acquire()
    assert acc.id == a.id
    await pool.release(acc)


@pytest.mark.asyncio
async def test_lease_context_manager_tra_account_ve_pool():
    a = _healthy()
    pool = await _pool_with([a])
    async with pool.lease() as lease:
        assert lease.account.id == a.id
    # Sau khi nhả, account vẫn lease lại được (inflight về 0).
    async with pool.lease() as lease2:
        assert lease2.account.id == a.id


def test_extra_is_zero_khongsai_voi_don_vi_billion():
    """muse.ai render extra tokens dạng "(0B tokens left)" / "(1.2B tokens left)"."""
    from muse2api.accounts.model import _extra_is_zero

    assert _extra_is_zero("0B tokens left") is True
    assert _extra_is_zero("0 B tokens") is True
    assert _extra_is_zero("1.2B tokens left") is False
    assert _extra_is_zero("500K tokens") is False
