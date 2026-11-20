"""Account pool: scheduling, per-account concurrency limits, health state and failover.

Usage::

    async with pool.lease(exclude={...}, hint=conversation_key) as lease:
        ... use lease.account ...
        lease.fail(exc)   # optional; success is recorded automatically otherwise
"""

from __future__ import annotations

import asyncio
import contextlib
import itertools
import logging
import time
from collections import defaultdict
from collections.abc import AsyncIterator, Callable, Iterable
from contextlib import asynccontextmanager
from typing import Protocol, TypeVar

from ..errors import (
    NoAccountAvailable,
    UpstreamAuthError,
    UpstreamQuotaError,
    UpstreamTimeout,
    UpstreamVideoLocked,
    UpstreamVmDisabled,
)
from .model import Account, AccountStatus
from .store import AccountStore

log = logging.getLogger(__name__)
T = TypeVar("T")

WAKE_BLOCK_KIND = "video"
WAKE_BLOCK_SECONDS = 600
"""How long an account whose VM wake was not confirmed is skipped for video: a video
on a VM that is not RUNNING stalls for minutes."""


def _kind_blocked_until(acc: Account, kind: str) -> float:
    return float((acc.meta.get("kind_blocked_until") or {}).get(kind, 0.0))


def note_wake(acc: Account, session_meta: dict) -> None:
    """Apply the VM wake outcome of one session renewal (``session_meta`` is that
    renewal's ``SessionInfo.meta``) through the per-kind block in
    ``meta["kind_blocked_until"]``: an unconfirmed wake blocks video for
    ``WAKE_BLOCK_SECONDS``; a confirmed one lifts that block again, but never a
    longer block set for another reason."""
    ok = session_meta.get("vm_wake_ok")
    if ok is None:  # no wake attempted this time
        return
    now = time.time()
    blocks = {k: t for k, t in (acc.meta.get("kind_blocked_until") or {}).items() if t > now}
    ours = acc.meta.pop("wake_blocked_until", None)
    if not ok:
        until = now + WAKE_BLOCK_SECONDS
        if blocks.get(WAKE_BLOCK_KIND, 0.0) < until:
            blocks[WAKE_BLOCK_KIND] = until
            acc.meta["wake_blocked_until"] = until
            log.warning("account %s vm wake not confirmed (state %s); no video on it for %ds",
                        acc.id, session_meta.get("vm_state_after"), WAKE_BLOCK_SECONDS)
    elif ours is not None and blocks.get(WAKE_BLOCK_KIND) == ours:
        del blocks[WAKE_BLOCK_KIND]
    acc.meta["kind_blocked_until"] = blocks


# ---------------------------------------------------------------- strategies
class Strategy(Protocol):
    def choose(self, candidates: list[Account], hint: str | None) -> Account: ...


class RoundRobinStrategy:
    def __init__(self) -> None:
        self._counter = itertools.count()

    def choose(self, candidates: list[Account], hint: str | None) -> Account:
        ordered = sorted(candidates, key=lambda a: a.id)
        return ordered[next(self._counter) % len(ordered)]


class LRUStrategy:
    def choose(self, candidates: list[Account], hint: str | None) -> Account:
        return min(candidates, key=lambda a: a.last_used_at)


class AffinityStrategy:
    """Sticks a hint (e.g. conversation id / user id) to the account that served it last,
    so a driver can reuse warm browser tabs or upstream threads. Falls back to LRU."""

    def __init__(self, max_entries: int = 4096) -> None:
        self._map: dict[str, str] = {}
        self._max = max_entries
        self._fallback = LRUStrategy()

    def choose(self, candidates: list[Account], hint: str | None) -> Account:
        if hint and (acc_id := self._map.get(hint)):
            for acc in candidates:
                if acc.id == acc_id:
                    return acc
        acc = self._fallback.choose(candidates, hint)
        if hint:
            if len(self._map) >= self._max:
                self._map.pop(next(iter(self._map)))
            self._map[hint] = acc.id
        return acc


STRATEGIES: dict[str, type] = {
    "round_robin": RoundRobinStrategy,
    "lru": LRUStrategy,
    "affinity": AffinityStrategy,
}


# ---------------------------------------------------------------- lease
class Lease:
    def __init__(self, account: Account) -> None:
        self.account = account
        self.error: BaseException | None = None

    def fail(self, exc: BaseException) -> None:
        self.error = exc


# ---------------------------------------------------------------- pool
class AccountPool:
    def __init__(
        self,
        store: AccountStore,
        *,
        strategy: str = "lru",
        max_concurrency: int = 1,
        cooldown: float = 120.0,
        acquire_timeout: float = 60.0,
        is_blocked_cb: Callable[[str], bool] | None = None,
    ) -> None:
        self.store = store
        self.strategy: Strategy = STRATEGIES[strategy]()
        self.max_concurrency = max(1, max_concurrency)
        self.cooldown = cooldown
        self.acquire_timeout = acquire_timeout
        self.is_blocked_cb = is_blocked_cb
        self._accounts: dict[str, Account] = {}
        self._inflight: dict[str, int] = defaultdict(int)
        self._cond = asyncio.Condition()
        self._persist_lock = asyncio.Lock()

    def is_inflight(self, account_id: str) -> bool:
        return self._inflight.get(account_id, 0) > 0

    # ---- lifecycle / CRUD ----
    async def load(self) -> None:
        self._accounts = {a.id: a for a in await self.store.load()}
        log.info("loaded %d account(s)", len(self._accounts))

    async def persist(self) -> None:
        lock = getattr(self, "_persist_lock", None)
        async with (lock if lock is not None else contextlib.nullcontext()):
            async with self._cond:
                snapshot = [a.model_copy(deep=True) for a in self._accounts.values()]
            await self.store.save(snapshot)

    def all(self) -> list[Account]:
        return list(self._accounts.values())

    def get(self, account_id: str) -> Account | None:
        return self._accounts.get(account_id)

    async def snapshot(self, account_id: str) -> Account | None:
        async with self._cond:
            acc = self._accounts.get(account_id)
            return acc.model_copy(deep=True) if acc is not None else None

    async def try_update(
        self,
        account_id: str,
        fn: Callable[[Account], tuple[bool, T]],
    ) -> tuple[bool, T | None]:
        async with self._cond:
            acc = self._accounts.get(account_id)
            if acc is None:
                return False, None
            changed, result = fn(acc)
            if changed:
                self._cond.notify_all()
        if changed:
            await self.persist()
        return True, result

    async def upsert(self, account: Account) -> Account:
        async with self._cond:
            self._accounts[account.id] = account
            self._cond.notify_all()
        await self.persist()
        return account

    async def remove(self, account_id: str) -> bool:
        async with self._cond:
            removed = self._accounts.pop(account_id, None) is not None
            if removed:
                self._cond.notify_all()
        if removed:
            await self.persist()
        return removed

    def stats(self) -> dict[str, int]:
        now = time.time()
        return {
            "total": len(self._accounts),
            "available": sum(1 for a in self._accounts.values() if a.is_available(now)),
            "invalid": sum(1 for a in self._accounts.values() if a.status == AccountStatus.INVALID),
            "video_locked": sum(1 for a in self._accounts.values() if a.status == AccountStatus.VIDEO_LOCKED),
            "inflight": sum(self._inflight.values()),
        }

    # ---- scheduling ----
    def _kind_blocked(
        self, kind: str | None, now: float, exclude: Iterable[str] = ()
    ) -> set[str]:
        """Ids of accounts still blocked for ``kind`` (see ``note_wake``). Empty when
        every usable account is blocked: a block must never make the kind unusable
        altogether, so then everyone gets tried again."""
        if kind is None:
            return set()
        excl = set(exclude)
        usable = [
            a
            for a in self._accounts.values()
            if a.id not in excl
            and a.enabled
            and a.status not in (AccountStatus.INVALID, AccountStatus.VIDEO_LOCKED)
            and not a.quota_exhausted(now)
            and not (self.is_blocked_cb and self.is_blocked_cb(a.id))
        ]
        blocked = {a.id for a in usable if _kind_blocked_until(a, kind) > now}
        return set() if len(blocked) == len(usable) else blocked

    def _candidates(
        self, exclude: Iterable[str], kind: str | None = None
    ) -> tuple[list[Account], bool, float | None, int, int]:
        """Returns ``(free, busy, cooling_back_at, quota_blocked_count, interactive_blocked_count)``.

        Quota-blocked accounts are counted separately and deliberately **not** folded into
        ``cooling_back_at``: their ``cooldown_until`` stays 0.0 (they were never cooled
        down), so including them made ``back_at`` land in the past — ``acquire()`` then
        slept/re-polled for the whole 60 s timeout before failing, and reported
        "cooling down" for what was really an exhausted quota.
        """
        now = time.time()
        excluded = set(exclude) | self._kind_blocked(kind, now, exclude)
        free, busy, back_at = [], False, None
        quota_blocked = 0
        interactive_blocked = 0
        for acc in self._accounts.values():
            if acc.id in excluded or not acc.enabled or acc.status in (AccountStatus.INVALID, AccountStatus.VIDEO_LOCKED):
                continue
            if acc.quota_exhausted(now):
                quota_blocked += 1
                continue
            if self.is_blocked_cb and self.is_blocked_cb(acc.id):
                interactive_blocked += 1
                continue
            if not acc.is_available(now):  # cooling down
                back_at = acc.cooldown_until if back_at is None else min(back_at, acc.cooldown_until)
                continue
            if self._inflight[acc.id] >= self.max_concurrency:
                busy = True
                continue
            free.append(acc)
        return free, busy, back_at, quota_blocked, interactive_blocked

    def has_alternative(self, exclude: Iterable[str], kind: str | None = None) -> bool:
        """Whether an enabled, valid, not-quota-exhausted account exists outside ``exclude``
        (busy or cooling counts) that is not blocked for ``kind``.

        Quota matters here: the gateway uses this to decide whether to fail over. Counting
        an exhausted account as an alternative made it retry into a pool that could only
        answer 503, turning a fast failure into a 60 s wait.
        """
        now = time.time()
        excluded = set(exclude) | self._kind_blocked(kind, now, exclude)
        return any(
            a.id not in excluded
            and a.enabled
            and a.status not in (AccountStatus.INVALID, AccountStatus.VIDEO_LOCKED)
            and not a.quota_exhausted(now)
            and not (self.is_blocked_cb and self.is_blocked_cb(a.id))
            for a in self._accounts.values()
        )

    async def acquire(
        self, *, exclude: Iterable[str] = (), hint: str | None = None, kind: str | None = None
    ) -> Account:
        exclude = tuple(exclude)
        deadline = time.monotonic() + self.acquire_timeout
        async with self._cond:
            while True:
                free, busy, back_at, quota_blocked, interactive_blocked = self._candidates(exclude, kind)
                if free:
                    acc = self.strategy.choose(free, hint)
                    self._inflight[acc.id] += 1
                    acc.last_used_at = time.time()
                    return acc
                remaining = deadline - time.monotonic()
                back_in = None if back_at is None else back_at - time.time()
                if not busy and back_in is None:
                    if interactive_blocked:
                        raise NoAccountAvailable("all accounts are busy in interactive browser session")
                    raise NoAccountAvailable(
                        "all accounts are out of quota"
                        if quota_blocked
                        else "no usable account in pool"
                    )
                if remaining <= 0 or (not busy and back_in > remaining):
                    # Nothing frees up before the deadline; say why instead of waiting in vain.
                    reason = "busy" if busy else "cooling down"
                    raise NoAccountAvailable(f"all accounts are {reason}, try again later")
                # Wake on release, or when the soonest cooldown ends (nothing notifies that).
                wait = remaining if back_in is None else min(remaining, max(back_in, 0.05))
                try:
                    await asyncio.wait_for(self._cond.wait(), timeout=wait)
                except asyncio.TimeoutError:
                    continue

    async def release(self, account: Account, error: BaseException | None = None) -> None:
        async with self._cond:
            self._inflight[account.id] = max(0, self._inflight[account.id] - 1)
            target = self._accounts.get(account.id, account)
            self._apply_result(target, error)
            if target is not account:
                self._apply_result(account, error)
            self._cond.notify_all()
        await self.persist()

    def _apply_result(self, acc: Account, error: BaseException | None) -> None:
        if error is None:
            acc.success_count += 1
            acc.last_error = None
            if acc.status == AccountStatus.COOLING and time.time() >= acc.cooldown_until:
                acc.status = AccountStatus.ACTIVE
            return
        acc.fail_count += 1
        acc.last_error = str(error)[:500]
        if isinstance(error, (UpstreamAuthError, UpstreamVmDisabled)):
            acc.status = AccountStatus.INVALID
            log.warning("account %s marked invalid: %s", acc.id, error)
        elif isinstance(error, UpstreamVideoLocked):
            acc.status = AccountStatus.VIDEO_LOCKED
            log.warning("account %s marked video_locked: %s", acc.id, error)
        elif isinstance(error, UpstreamQuotaError):
            acc.status = AccountStatus.COOLING
            acc.cooldown_until = time.time() + max(self.cooldown, 3600)
            log.warning("account %s out of quota, cooling down", acc.id)
        elif isinstance(error, UpstreamTimeout):
            # muse.ai being slow says nothing about the account; cooling it down
            # would only fail the other requests running on it in parallel.
            pass
        elif getattr(error, "retryable", False):
            acc.status = AccountStatus.COOLING
            acc.cooldown_until = time.time() + self.cooldown

    @asynccontextmanager
    async def lease(
        self, *, exclude: Iterable[str] = (), hint: str | None = None, kind: str | None = None
    ) -> AsyncIterator[Lease]:
        acc = await self.acquire(exclude=exclude, hint=hint, kind=kind)
        lease = Lease(acc)
        try:
            yield lease
        except (asyncio.CancelledError, GeneratorExit):
            # Client went away; not the account's fault.
            raise
        except BaseException as exc:
            lease.error = lease.error or exc
            raise
        finally:
            await asyncio.shield(self.release(acc, lease.error))
