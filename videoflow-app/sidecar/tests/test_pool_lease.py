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


# ── Hard invariant: tối đa 1 luồng video đồng thời trên mỗi tài khoản ───────


@pytest.mark.asyncio
async def test_video_lease_enforces_at_most_one_video_per_account_even_with_high_max_concurrency():
    """Ngay cả khi max_concurrency=4, 1 tài khoản chỉ được lease tối đa 1 luồng video tại một thời điểm."""
    acc = _healthy(id="acc_only", label="only")
    pool = AccountPool(_MemStore([acc]), max_concurrency=4, acquire_timeout=0.2)
    await pool.load()

    async with pool.lease(kind="video") as v1:
        assert v1.account.id == "acc_only"
        assert pool.stats()["video_inflight"] == 1

        # Luồng ảnh (kind=None) VẪN chạy song song được trên cùng tài khoản
        async with pool.lease(kind=None) as img:
            assert img.account.id == "acc_only"
            assert pool.stats()["inflight"] == 2
            assert pool.stats()["video_inflight"] == 1

        # Luồng video thứ 2 trên cùng tài khoản PHẢI bị chặn (không cho phép >1 video/tài khoản)
        with pytest.raises(NoAccountAvailable, match="busy"):
            await pool.acquire(kind="video")

    # Sau khi video 1 nhả, tài khoản sẵn sàng nhận video kế tiếp
    assert pool.stats()["video_inflight"] == 0
    async with pool.lease(kind="video") as v2:
        assert v2.account.id == "acc_only"
        assert pool.stats()["video_inflight"] == 1
    assert pool.stats()["video_inflight"] == 0


@pytest.mark.asyncio
async def test_video_lease_spreads_across_distinct_accounts_ignoring_image_lru_shift():
    """Khi có 2 tài khoản, video 2 PHẢI sang tài khoản 2 ngay cả khi tài khoản 1 có last_used_at cũ hơn."""
    acc1 = _healthy(id="acc_1", label="one")
    acc2 = _healthy(id="acc_2", label="two")
    pool = AccountPool(_MemStore([acc1, acc2]), strategy="lru", max_concurrency=4, acquire_timeout=0.3)
    await pool.load()

    async with pool.lease(kind="video") as v1:
        # Đẩy last_used_at của tài khoản còn lại mới hơn bằng 1 lượt ảnh
        other_id = "acc_2" if v1.account.id == "acc_1" else "acc_1"
        async with pool.lease(exclude={v1.account.id}, kind=None) as img:
            assert img.account.id == other_id
            # Bây giờ v1.account có last_used_at CŨ hơn other_id; video 2 vẫn PHẢI chọn other_id!
            async with pool.lease(kind="video") as v2:
                assert v2.account.id == other_id
                assert v2.account.id != v1.account.id
                assert pool.stats()["video_inflight"] == 2
                # Video thứ 3 khi cả 2 tài khoản đang chạy video -> PHẢI bị chặn
                with pytest.raises(NoAccountAvailable, match="busy"):
                    await pool.acquire(kind="video")

    assert pool.stats()["video_inflight"] == 0


@pytest.mark.asyncio
async def test_video_lease_wakes_waiting_video_immediately_on_release():
    """Video chờ sẵn sẽ được đánh thức ngay lập tức khi video trên tài khoản kết thúc."""
    import asyncio

    acc = _healthy(id="acc_wake", label="wake")
    pool = AccountPool(_MemStore([acc]), max_concurrency=4, acquire_timeout=2.0)
    await pool.load()

    entered = asyncio.Event()
    release_v1 = asyncio.Event()
    events: list[str] = []

    async def run_v1() -> None:
        async with pool.lease(kind="video"):
            events.append("v1_start")
            entered.set()
            await release_v1.wait()
            events.append("v1_end")

    async def run_v2() -> None:
        await entered.wait()
        async with pool.lease(kind="video"):
            events.append("v2_start")

    t1 = asyncio.create_task(run_v1())
    t2 = asyncio.create_task(run_v2())
    await entered.wait()
    await asyncio.sleep(0.05)
    assert events == ["v1_start"], "v2 MUST NOT start while v1 still holds video slot on acc_wake"
    release_v1.set()
    await asyncio.gather(t1, t2)
    assert events == ["v1_start", "v1_end", "v2_start"]
    assert pool.stats()["video_inflight"] == 0


@pytest.mark.asyncio
async def test_video_lease_releases_slot_on_exception_and_cancellation():
    """Lỗi hoặc hủy giữa chừng không bao giờ làm rò rỉ video_inflight."""
    acc = _healthy(id="acc_err", label="err")
    pool = AccountPool(_MemStore([acc]), max_concurrency=4, acquire_timeout=0.5)
    await pool.load()

    with pytest.raises(RuntimeError):
        async with pool.lease(kind="video"):
            assert pool.stats()["video_inflight"] == 1
            raise RuntimeError("boom")

    assert pool.stats()["video_inflight"] == 0
    async with pool.lease(kind="video") as v_ok:
        assert v_ok.account.id == "acc_err"
    assert pool.stats()["video_inflight"] == 0
