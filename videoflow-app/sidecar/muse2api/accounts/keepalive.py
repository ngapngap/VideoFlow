"""Background session keepalive.

muse.ai sessions expire unless refreshed; this loop periodically asks the active
driver to renew each account and merges refreshed cookies back into the pool.
Drivers that do not support renewal simply raise ``FeatureNotImplemented`` and
the loop skips them.
"""

from __future__ import annotations

import asyncio
import logging
import time

from ..drivers.base import MuseDriver
from ..errors import FeatureNotImplemented, UpstreamAuthError
from .model import Account, AccountStatus
from .pool import AccountPool, note_wake

log = logging.getLogger(__name__)


async def renew_account(pool: AccountPool, driver: MuseDriver, account_id: str) -> dict:
    if hasattr(pool, "snapshot"):
        acc = await pool.snapshot(account_id)
    else:
        raw = pool.get(account_id)
        acc = raw.model_copy(deep=True) if raw is not None else None
    if acc is None:
        return {"ok": False, "error": "not found"}
    if pool.is_blocked_cb and pool.is_blocked_cb(account_id):
        return {"ok": False, "error": "account is busy in an interactive browser session"}
    orig_cookies = dict(acc.cookies)
    try:
        info = await driver.renew_session(acc)
    except FeatureNotImplemented as exc:
        return {"ok": False, "error": exc.message}
    except UpstreamAuthError as exc:
        err_msg = exc.message

        def _on_auth_error(latest: Account) -> tuple[bool, dict]:
            if pool.is_blocked_cb and pool.is_blocked_cb(account_id):
                return False, {"ok": False, "error": "account is busy in an interactive browser session"}
            if latest.cookies != orig_cookies:
                return False, {"ok": False, "error": "account cookies replaced concurrently"}
            latest.status = AccountStatus.INVALID
            latest.last_error = err_msg
            return True, {"ok": False, "error": err_msg}

        if hasattr(pool, "try_update"):
            found, res = await pool.try_update(account_id, _on_auth_error)
            if not found:
                return {"ok": False, "error": "account deleted"}
            return res  # type: ignore[return-value]
        latest = pool.get(account_id)
        if latest is None:
            return {"ok": False, "error": "account deleted"}
        changed, res = _on_auth_error(latest)
        if changed:
            await pool.upsert(latest)
        return res

    def _on_success(latest: Account) -> tuple[bool, dict]:
        if pool.is_blocked_cb and pool.is_blocked_cb(account_id):
            return False, {"ok": False, "error": "account is busy in an interactive browser session"}
        if latest.cookies != orig_cookies:
            return False, {"ok": False, "error": "account cookies replaced concurrently"}
        latest.cookies.update(info.cookies)
        latest.cookie_expires.update(info.cookie_expires)
        latest.meta.update(info.meta)
        latest.meta["renewed_at"] = time.time()
        note_wake(latest, info.meta)
        return True, {"ok": info.ok, "meta": info.meta}

    if hasattr(pool, "try_update"):
        found, res = await pool.try_update(account_id, _on_success)
        if not found:
            return {"ok": False, "error": "account deleted"}
        return res  # type: ignore[return-value]
    latest = pool.get(account_id)
    if latest is None:
        return {"ok": False, "error": "account deleted"}
    changed, res = _on_success(latest)
    if changed:
        await pool.upsert(latest)
    return res


async def keepalive_loop(pool: AccountPool, driver: MuseDriver, interval: float) -> None:
    log.info("keepalive loop started (interval=%ss)", interval)
    while True:
        await asyncio.sleep(interval)
        for acc in pool.all():
            if not acc.enabled or acc.status == AccountStatus.INVALID:
                continue
            if pool.is_blocked_cb and pool.is_blocked_cb(acc.id):
                continue
            try:
                result = await renew_account(pool, driver, acc.id)
                log.info("keepalive %s -> %s", acc.id, result.get("ok"))
            except Exception:  # noqa: BLE001
                log.exception("keepalive failed for %s", acc.id)
