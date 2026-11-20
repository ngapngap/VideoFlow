from __future__ import annotations

import re
import time
import uuid
from datetime import datetime
from enum import Enum
from typing import Any

from pydantic import BaseModel, Field


class AccountStatus(str, Enum):
    ACTIVE = "active"
    COOLING = "cooling"
    """Temporarily unavailable (quota exhausted or transient errors); returns after cooldown."""
    INVALID = "invalid"
    """Session rejected by upstream; needs fresh cookies."""
    VIDEO_LOCKED = "video_locked"
    """Video generation feature is locked for this account."""


def _now() -> float:
    return time.time()


class Account(BaseModel):
    id: str = Field(default_factory=lambda: "acc_" + uuid.uuid4().hex[:10])
    label: str = ""
    enabled: bool = True
    status: AccountStatus = AccountStatus.ACTIVE
    proxy: str | None = None

    cookies: dict[str, str] = Field(default_factory=dict)
    cookie_expires: dict[str, int] = Field(
        default_factory=dict, description="cookie name -> unix expiry seconds"
    )

    created_at: float = Field(default_factory=_now)
    last_used_at: float = 0.0
    cooldown_until: float = 0.0
    last_error: str | None = None
    success_count: int = 0
    fail_count: int = 0

    meta: dict[str, Any] = Field(
        default_factory=dict, description="Driver-specific data such as quota snapshots or vm ids"
    )

    def quota_exhausted(self, now: float | None = None) -> bool:
        """True when the last readable quota snapshot says this account is out of
        weekly quota and the reported reset moment has not arrived yet.

        muse.ai reports usage as a percentage of the weekly allowance plus an optional
        extra-token balance. Only what we can actually read is enforced: a snapshot with
        ``weekly_used_pct >= 100`` that still lies before ``resets_at``. Anything unknown
        (no snapshot, unparseable date, count-based shape from the mock driver) counts as
        usable — the pool must never block an account on a guess.
        """
        q = (self.meta or {}).get("quota") or {}
        pct = q.get("weekly_used_pct")
        if not isinstance(pct, (int, float)) or pct < 100:
            return False
        # Extra tokens bought on top of the plan keep the account usable.
        extra = q.get("extra_left")
        if isinstance(extra, str) and extra.strip() and not _extra_is_zero(extra):
            return False
        checked_at = q.get("checked_at") or q.get("fetched_at")
        reset_in = _reset_deadline(q.get("resets_at"), now or _now(), checked_at=checked_at)
        if reset_in is None:
            return False
        return reset_in > 0

    def is_available(self, now: float | None = None) -> bool:
        if not self.enabled or self.status in (AccountStatus.INVALID, AccountStatus.VIDEO_LOCKED):
            return False
        # Quota cạn chặn cả account đang cooling đã hết hạn cooldown — nếu chỉ xét
        # cooldown thì account bị chặn do hết quota sẽ được lease lại ngay khi hết
        # cooldown, đốt một request vào upstream vô ích.
        if self.quota_exhausted(now):
            return False
        if self.status == AccountStatus.COOLING:
            return (now or _now()) >= self.cooldown_until
        return True

    def public(self) -> dict[str, Any]:
        """Serialisable view with cookie values masked."""
        data = self.model_dump(mode="json")
        data["cookies"] = {k: _mask(v) for k, v in self.cookies.items()}
        return data


def _mask(value: str) -> str:
    if len(value) <= 8:
        return "***"
    return f"{value[:4]}...{value[-4:]}"


_ZERO_EXTRA = ("0", "0.0", "none", "null", "n/a", "-", "—")
# Hậu tố số lượng muse.ai render: "1.2B tokens", "500K tokens", "2M", "0B tokens left"
_QUANTITY_SUFFIX = re.compile(r"^([0-9]*\.?[0-9]+)\s*([kmb])?\b", re.IGNORECASE)


def _extra_is_zero(text: str) -> bool:
    """``"0B tokens left"``/``"none"`` → hết; ``"1,200 tokens"``/``"2B tokens"`` → còn.

    muse.ai renders the extra-token balance with SI-ish suffixes ("1.2B tokens left"), so a
    bare ``float()`` on the first word reads "0b" as unparseable and wrongly reports a
    balance. Parse the leading number and its K/M/B suffix; anything unparseable counts as
    non-zero so an unknown balance never blocks the account.
    """
    cleaned = text.strip().lower().replace(",", "")
    if not cleaned:
        return True
    head = cleaned.split()[0]
    if head in _ZERO_EXTRA:
        return True
    m = _QUANTITY_SUFFIX.match(cleaned)
    if not m:
        return False
    try:
        value = float(m.group(1))
    except ValueError:
        return False
    return value <= 0


_RESET_FORMATS = (
    "%b %d, %Y",
    "%B %d, %Y",
    "%b %d %Y",
    "%Y-%m-%d",
    "%m/%d/%Y",
    "%d/%m/%Y",
)

_RESET_NO_YEAR_FORMATS = (
    "%b %d",
    "%B %d",
)


_ISO_TIME_TAIL = re.compile(r"T\d{2}:\d{2}.*$")
_ZH_DATE = re.compile(r"^(?:(\d{4})年)?\s*(\d{1,2})月(\d{1,2})日?$")


def _safe_replace_year(dt: datetime, year: int) -> datetime:
    try:
        return dt.replace(year=year, hour=23, minute=59, second=59)
    except ValueError:
        return dt.replace(year=year, day=28, hour=23, minute=59, second=59)


def _reset_deadline(
    resets_at: Any, now: float, *, checked_at: float | None = None
) -> float | None:
    """Seconds from ``now`` until the weekly quota resets, or None when unknown.

    muse.ai renders the reset as a human date ("Oct 12, 2025", "Sep 30", "11月25日").
    When ``resets_at`` has no year and ``checked_at`` is provided, anchor the year
    relative to ``checked_at`` so an expired snapshot stops blocking once its reset
    date passes. Unparseable values return None so callers fall back to "do not block".
    """
    if not resets_at:
        return None
    if isinstance(resets_at, (int, float)):
        return float(resets_at) - now
    text = str(resets_at).strip()
    # Chỉ cắt ISO timestamp dạng T12:34:56... (không cắt chữ 'T' trong "Tue" / "UTC").
    text = _ISO_TIME_TAIL.sub("", text)
    # Bỏ phần giờ/timezone ở cuối: "Oct 12, 2025 at 10:00 (GMT+7)", "Oct 12, 2025 (GMT+7)".
    # Lưu ý KHÔNG cắt ở ", " vì dấu phẩy nằm trong chính định dạng ngày ("Oct 12, 2025").
    for sep in (" at ", " (", " – ", " - "):
        if sep in text:
            text = text.split(sep, 1)[0]
    text = text.strip().rstrip(",").strip()
    # Còn dạng "Oct 12, 2025, 10:00" → bỏ phần giờ sau dấu phẩy thứ hai.
    if text.count(",") == 2:
        text = text.rsplit(",", 1)[0].strip()

    # Hỗ trợ ngày tiếng Trung: "2026年11月25日" hoặc "11月25日".
    if zm := _ZH_DATE.match(text):
        yr_s, mo_s, dy_s = zm.groups()
        mo, dy = int(mo_s), int(dy_s)
        if yr_s:
            try:
                when = datetime(int(yr_s), mo, dy, 23, 59, 59).timestamp()
                return when - now
            except ValueError:
                return None
        try:
            dt = datetime(2024, mo, dy)
        except ValueError:
            return None
        anchor_ts = float(checked_at) if isinstance(checked_at, (int, float)) and checked_at > 0 else now
        anchor_dt = datetime.fromtimestamp(anchor_ts)
        cand = _safe_replace_year(dt, anchor_dt.year)
        if cand.timestamp() < anchor_ts:
            cand = _safe_replace_year(dt, anchor_dt.year + 1)
        return cand.timestamp() - now

    for fmt in _RESET_FORMATS:
        try:
            when = datetime.strptime(text, fmt).replace(
                hour=23, minute=59, second=59
            ).timestamp()
            return when - now
        except ValueError:
            continue

    anchor_ts = float(checked_at) if isinstance(checked_at, (int, float)) and checked_at > 0 else now
    anchor_dt = datetime.fromtimestamp(anchor_ts)
    for fmt in _RESET_NO_YEAR_FORMATS:
        try:
            # Parse kèm năm nhuận 2024 để hỗ trợ "Feb 29".
            dt = datetime.strptime(f"{text} 2024", f"{fmt} %Y")
            cand = _safe_replace_year(dt, anchor_dt.year)
            if cand.timestamp() < anchor_ts:
                cand = _safe_replace_year(dt, anchor_dt.year + 1)
            return cand.timestamp() - now
        except ValueError:
            continue
    return None
