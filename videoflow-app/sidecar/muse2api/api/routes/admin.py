"""Admin API: account pool management, status, tasks, client keys and the request log.

The ``/dashboard`` page is built on these endpoints, and so is the browser
extension for one-click cookie import (served by ``GET /admin/extension.zip``).
"""

from __future__ import annotations

import io
import time
import zipfile
from pathlib import Path
from typing import Literal

from fastapi import APIRouter, Depends, Query
from fastapi.responses import Response

from ... import __version__
from ...accounts.keepalive import renew_account
from ...accounts.model import Account, AccountStatus
from ...auth.keys import ApiKey
from ...drivers.browser.driver import AccountBusy
from ...errors import InvalidRequest, Muse2APIError, NotFound
from ...services.container import Services
from ...upstream.muse import missing_session_cookies
from ..deps import get_services, require_admin_key
from ..schemas import AccountCreate, AccountsImportRequest, AccountUpdate, KeyCreate, KeyUpdate

router = APIRouter(prefix="/admin", tags=["admin"], dependencies=[Depends(require_admin_key)])

EXTENSION_DIR = Path(__file__).resolve().parents[2] / "web" / "extension"
"""Unpacked Chrome extension that imports muse.ai cookies into this gateway."""


def parse_cookies(value: dict[str, str] | str | list[dict]) -> tuple[dict[str, str], dict[str, int]]:
    """Normalise the three cookie import formats into ``(cookies, cookie_expires)``.

    ``cookie_expires`` maps cookie name -> unix expiry seconds; only cookie-export
    lists carry expiry info, the other formats return an empty dict.
    """
    if isinstance(value, dict):
        return dict(value), {}
    if isinstance(value, str):
        cookies: dict[str, str] = {}
        for part in value.split(";"):
            part = part.strip()
            if not part or "=" not in part:
                continue
            name, _, val = part.partition("=")
            name = name.strip()
            if name:
                cookies[name] = val.strip()
        return cookies, {}
    if isinstance(value, list):
        cookies, expires = {}, {}
        for i, entry in enumerate(value):
            if not isinstance(entry, dict) or not entry.get("name"):
                raise InvalidRequest(f"cookies[{i}] must be an object with 'name' and 'value'")
            name = str(entry["name"])
            cookies[name] = str(entry.get("value", ""))
            exp = entry.get("expirationDate")
            if exp is not None:
                try:
                    expires[name] = int(float(exp))
                except (TypeError, ValueError):
                    raise InvalidRequest(f"cookies[{i}].expirationDate is not a number") from None
        return cookies, expires
    raise InvalidRequest("cookies must be a dict, a raw 'Cookie' header string, or a cookie-export list")


def _build_account(item: AccountCreate) -> Account:
    """Build an Account from a create payload, normalising the cookie format first."""
    if not item.cookies:
        cookies, expires = {}, {}
    else:
        cookies, expires = parse_cookies(item.cookies)
        cookies = {k: v for k, v in cookies.items() if v}
        expires = {k: v for k, v in expires.items() if k in cookies}
    status = AccountStatus.ACTIVE if cookies else AccountStatus.INVALID
    proxy_val = item.proxy.strip() if item.proxy and item.proxy.strip() else None
    return Account(
        label=item.label,
        cookies=cookies,
        cookie_expires={**expires, **item.cookie_expires},
        enabled=item.enabled,
        status=status,
        proxy=proxy_val,
    )


def _check_duplicate_session(acc: Account, svc: Services) -> None:
    """Reject a cookie whose ``hatch_sess`` already belongs to another account: the same
    browser session imported twice would race over its own cookie rotations."""
    session = acc.cookies.get("hatch_sess")
    if not session:
        return
    for other in svc.pool.all():
        if other.id != acc.id and other.cookies.get("hatch_sess") == session:
            label = other.label or other.id
            raise InvalidRequest(
                f"hatch_sess already belongs to account '{other.id}' ({label}); "
                "log in again in the browser and export the fresh cookies")


def _get(svc: Services, account_id: str) -> Account:
    acc = svc.pool.get(account_id)
    if acc is None:
        raise NotFound(f"account '{account_id}' not found")
    return acc


@router.get("/status")
async def status(svc: Services = Depends(get_services)) -> dict:
    driver_health = await svc.driver.health()
    browser_alive = getattr(svc.driver, "browser_alive", bool(driver_health.get("ok", False)))
    return {
        "version": __version__,
        "browser_alive": browser_alive,
        "driver": driver_health,
        "capabilities": svc.driver.capabilities.__dict__,
        "accounts": svc.pool.stats(),
    }


@router.get("/accounts")
async def list_accounts(svc: Services = Depends(get_services)) -> dict:
    return {"data": [a.public() for a in svc.pool.all()]}


@router.post("/accounts")
async def create_account(body: AccountCreate, svc: Services = Depends(get_services),
                         allow_duplicate_session: bool = False) -> dict:
    acc = _build_account(body)
    if not allow_duplicate_session:
        _check_duplicate_session(acc, svc)
    await svc.pool.upsert(acc)
    return {"account": acc.public(), "missing_cookies": missing_session_cookies(acc.cookies)}


@router.post("/accounts/import")
async def import_accounts(body: AccountsImportRequest, svc: Services = Depends(get_services),
                          allow_duplicate_session: bool = False) -> dict:
    """Import several accounts in one request; a failing entry does not abort the batch."""
    results = []
    for i, item in enumerate(body.accounts):
        try:
            acc = _build_account(item)
            if not allow_duplicate_session:
                _check_duplicate_session(acc, svc)
        except Muse2APIError as e:
            results.append({"index": i, "ok": False, "error": e.message, "code": e.code})
            continue
        await svc.pool.upsert(acc)
        results.append({"index": i, "ok": True, "account": acc.public(),
                        "missing_cookies": missing_session_cookies(acc.cookies)})
    return {"data": results}


@router.patch("/accounts/{account_id}")
async def update_account(account_id: str, body: AccountUpdate,
                         svc: Services = Depends(get_services),
                         allow_duplicate_session: bool = False) -> dict:
    acc = _get(svc, account_id)
    if body.label is not None:
        acc.label = body.label
    if body.enabled is not None:
        acc.enabled = body.enabled
    if body.cookies is not None:
        # Same guard as POST: a hatching session must not be shared by two accounts,
        # otherwise both fight over its rotation and die together.
        if not allow_duplicate_session:
            probe = Account(id=acc.id, label=acc.label, cookies={**acc.cookies, **body.cookies})
            _check_duplicate_session(probe, svc)
        for k, v in body.cookies.items():
            if v:
                acc.cookies[k] = v
            else:
                acc.cookies.pop(k, None)
        # Expiry belongs to the OLD session: drop it for cookies that were replaced, or
        # the account would look like it expires when the previous session did.
        acc.cookie_expires = {
            name: exp for name, exp in (acc.cookie_expires or {}).items()
            if name not in body.cookies
        }
        # Fresh cookies give an invalid account another chance.
        acc.status = AccountStatus.ACTIVE
        acc.last_error = None
    if body.cookie_expires is not None:
        if acc.cookie_expires is None:
            acc.cookie_expires = {}
        acc.cookie_expires.update(body.cookie_expires)
    if body.proxy is not None:
        acc.proxy = body.proxy.strip() if body.proxy and body.proxy.strip() else None
    await svc.pool.upsert(acc)
    driver = getattr(svc, "driver", None)
    if body.cookies is not None and driver is not None and hasattr(driver, "mark_account_stale"):
        await driver.mark_account_stale(account_id)
    return {"account": acc.public()}


@router.delete("/accounts/{account_id}")
async def delete_account(account_id: str, svc: Services = Depends(get_services)) -> dict:
    if not await svc.pool.remove(account_id):
        raise NotFound(f"account '{account_id}' not found")
    driver = getattr(svc, "driver", None)
    if driver is not None and hasattr(driver, "release_account_driver"):
        await driver.release_account_driver(account_id)
    return {"deleted": account_id}


@router.post("/accounts/{account_id}/reset")
async def reset_account(account_id: str, svc: Services = Depends(get_services)) -> dict:
    acc = _get(svc, account_id)
    acc.status, acc.cooldown_until, acc.last_error = AccountStatus.ACTIVE, 0.0, None
    acc.meta.pop("kind_blocked_until", None)  # e.g. video after an unconfirmed VM wake
    acc.meta.pop("wake_blocked_until", None)
    await svc.pool.upsert(acc)
    return {"account": acc.public()}


@router.post("/accounts/{account_id}/renew")
async def renew(account_id: str, svc: Services = Depends(get_services)) -> dict:
    _get(svc, account_id)
    return await renew_account(svc.pool, svc.driver, account_id)


@router.post("/accounts/{account_id}/open-login")
async def open_login(account_id: str, svc: Services = Depends(get_services)) -> dict:
    acc = _get(svc, account_id)
    if svc.interactive and svc.interactive.is_account_busy(account_id):
        raise AccountBusy("Tài khoản đang bận chạy batch, không thể mở trình duyệt")
    if svc.interactive is None:
        raise InvalidRequest("Interactive browser sessions not available")
    return await svc.interactive.open_login(acc)


@router.post("/accounts/{account_id}/open-view")
async def open_view(account_id: str, svc: Services = Depends(get_services)) -> dict:
    acc = _get(svc, account_id)
    if svc.interactive and svc.interactive.is_account_busy(account_id):
        raise AccountBusy("Tài khoản đang bận chạy batch, không thể mở trình duyệt")
    if svc.interactive is None:
        raise InvalidRequest("Interactive browser sessions not available")
    return await svc.interactive.open_view(acc)


@router.get("/accounts/{account_id}/quota")
async def account_quota(account_id: str, refresh: bool = False,
                        svc: Services = Depends(get_services)) -> dict:
    """Ask the active driver for the account's quota snapshot and persist it in ``meta``.

    The browser driver caches each account's snapshot for 30 minutes
    (``MUSE2API_QUOTA_CACHE_S``); ``?refresh=1`` reads the page again. It answers
    409 while every tab of the account is serving a request. Drivers that do not
    implement ``MuseDriver.quota`` raise ``FeatureNotImplemented`` (HTTP 501).
    """
    acc = _get(svc, account_id)
    if refresh and (forget := getattr(svc.driver, "forget_quota", None)):
        forget(acc.id)
    snapshot = await svc.driver.quota(acc)
    latest = svc.pool.get(account_id)
    if latest is None:
        raise NotFound(f"account '{account_id}' not found")
    latest.meta["quota"] = {"checked_at": time.time(), **snapshot}
    await svc.pool.upsert(latest)
    return {"account_id": latest.id, "quota": latest.meta["quota"]}


@router.get("/extension.zip")
async def extension_zip() -> Response:
    """The cookie-import browser extension, zipped in memory for "Load unpacked"."""
    root = EXTENSION_DIR
    # No symlinks (they could point outside the directory) and no dotfiles.
    files = sorted(f for f in root.rglob("*")
                   if f.is_file() and not f.is_symlink()
                   and not any(part.startswith(".") for part in f.relative_to(root).parts)
                   ) if root.is_dir() else []
    if not files:
        raise NotFound("the browser extension is not bundled with this server")
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as zf:
        for f in files:
            zf.write(f, f"muse2api-extension/{f.relative_to(root).as_posix()}")
    return Response(buf.getvalue(), media_type="application/zip", headers={
        "Content-Disposition": 'attachment; filename="muse2api-extension.zip"'})


@router.get("/tasks")
async def list_tasks(limit: int = 100, svc: Services = Depends(get_services)) -> dict:
    return {"data": [t.model_dump(mode="json") for t in svc.tasks.list(limit)]}


@router.post("/tasks/{task_id}/cancel")
async def cancel_task(task_id: str, svc: Services = Depends(get_services)) -> dict:
    return {"cancelled": await svc.tasks.cancel(task_id)}


# ---- client API keys ----
def _get_key(svc: Services, key_id: str) -> ApiKey:
    key = svc.keys.get(key_id)
    if key is None:
        raise NotFound(f"key '{key_id}' not found")
    return key


# Identities that authenticate without a stored key (see deps.require_api_key).
BUILTIN_KEYS = ("legacy", "admin")
_NO_USAGE = {"total": 0, "requests_24h": 0, "errors_24h": 0, "last_request_at": None}


def _usage(row: dict | None) -> dict:
    if row is None:
        return dict(_NO_USAGE)
    return {k: row[k] for k in _NO_USAGE}


@router.get("/keys")
async def list_keys(svc: Services = Depends(get_services)) -> dict:
    """Stored keys plus the built-in identities, each with request counts from the log."""
    usage = {r["key_id"]: r for r in (await svc.requests.key_usage())["data"]}
    return {
        "data": [{**k.public(), "usage": _usage(usage.get(k.id))} for k in svc.keys.all()],
        "builtin": [{"id": b, "name": b, "usage": _usage(usage.get(b))} for b in BUILTIN_KEYS],
    }


@router.get("/keys/usage")
async def key_usage(svc: Services = Depends(get_services)) -> dict:
    """Raw per-``key_id`` counts, including deleted keys still present in the log."""
    return await svc.requests.key_usage()


@router.post("/keys")
async def create_key(body: KeyCreate, svc: Services = Depends(get_services)) -> dict:
    key, plaintext = await svc.keys.create(body.name.strip(), body.note)
    # The only time the plaintext key is ever returned.
    return {"key": key.public(), "api_key": plaintext}


@router.patch("/keys/{key_id}")
async def update_key(key_id: str, body: KeyUpdate, svc: Services = Depends(get_services)) -> dict:
    key = _get_key(svc, key_id)
    if body.name is not None:
        key.name = body.name.strip()
    if body.note is not None:
        key.note = body.note
    if body.revoked is not None:
        key.revoked = body.revoked
    await svc.keys.save()
    return {"key": key.public()}


@router.delete("/keys/{key_id}")
async def delete_key(key_id: str, svc: Services = Depends(get_services)) -> dict:
    if not await svc.keys.remove(key_id):
        raise NotFound(f"key '{key_id}' not found")
    return {"deleted": key_id}


# ---- request log ----
@router.get("/requests")
async def list_requests(
    key_id: str | None = None,
    account_id: str | None = None,
    status: Literal["2xx", "3xx", "4xx", "5xx", "failed"] | None = None,
    path: str | None = None,
    since: float | None = Query(default=None, description="Unix seconds"),
    hide_polls: bool = False,
    limit: int = Query(default=100, ge=1, le=500),
    offset: int = Query(default=0, ge=0),
    svc: Services = Depends(get_services),
) -> dict:
    return await svc.requests.query(key_id=key_id, account_id=account_id, status=status,
                                    path=path, since=since, hide_polls=hide_polls,
                                    limit=limit, offset=offset)


@router.get("/stats")
async def stats(window: Literal["1h", "24h", "7d"] = "24h",
                svc: Services = Depends(get_services)) -> dict:
    return {"now": time.time(), **await svc.requests.stats(window)}
