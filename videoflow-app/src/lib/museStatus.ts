import type { MuseAccount } from "./api";

export interface LockedAccountInfo {
  id: string;
  label?: string;
}

export interface StatusbarIpResult {
  text: string;
  title: string;
}

const IPV4_RE = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/;

function isIpv4Literal(host: string): boolean {
  const m = IPV4_RE.exec(host);
  if (!m) return false;
  for (let i = 1; i <= 4; i++) {
    const octet = Number(m[i]);
    if (!Number.isInteger(octet) || octet < 0 || octet > 255) return false;
  }
  return true;
}

function isIpv6Literal(host: string): boolean {
  const inner = host.startsWith("[") && host.endsWith("]") ? host.slice(1, -1) : host;
  if (!inner || !inner.includes(":") || inner.includes("[") || inner.includes("]")) {
    return false;
  }
  try {
    const u = new URL(`http://[${inner}]`);
    return u.hostname.startsWith("[") && u.hostname.endsWith("]");
  } catch {
    return false;
  }
}

function isIpLiteralHost(host: string): boolean {
  const s = host.trim();
  if (!s) return false;
  return isIpv4Literal(s) || isIpv6Literal(s);
}

function extractHostFromHostPort(hostPort: string): string {
  const s = hostPort.trim();
  if (!s) return "";
  if (s.startsWith("[")) {
    const endIdx = s.indexOf("]");
    return endIdx > 0 ? s.slice(0, endIdx + 1) : s;
  }
  const colonCount = (s.match(/:/g) || []).length;
  if (colonCount === 1) {
    return s.slice(0, s.indexOf(":"));
  }
  return s;
}

/**
 * Resolves the active video-locked account for the top banner.
 * Returns null immediately when sidecar is not running, or when the event-locked
 * account is disabled, no longer video_locked, or absent from a non-empty pool.
 */
export function resolveLockedAccount(
  accounts: readonly MuseAccount[],
  eventLocked: LockedAccountInfo | null,
  sidecarRunning: boolean
): LockedAccountInfo | null {
  if (!sidecarRunning) return null;

  const fromPool = accounts.find((a) => a.enabled && a.status === "video_locked");
  if (fromPool) {
    return { id: fromPool.id, label: fromPool.label || fromPool.id };
  }

  if (eventLocked) {
    const matched = accounts.find((a) => a.id === eventLocked.id);
    if (matched) {
      if (!matched.enabled || matched.status !== "video_locked") {
        return null;
      }
      return { id: matched.id, label: matched.label || eventLocked.label || matched.id };
    }
    if (accounts.length > 0) {
      return null;
    }
    return eventLocked;
  }

  return null;
}

/**
 * Extracts `host:port` (or `host` if port is omitted) from a proxy URL,
 * stripping credentials and path while preserving IPv6 brackets.
 */
export function formatProxyHostPort(proxyUrl: string | null | undefined): string | null {
  if (!proxyUrl) return null;
  const raw = proxyUrl.trim();
  if (!raw) return null;

  if (/^[a-zA-Z][a-zA-Z0-9+.-]*:\/\//.test(raw)) {
    try {
      const u = new URL(raw);
      if (u.hostname) {
        let port = u.port;
        if (!port) {
          const withoutScheme = raw.replace(/^[a-zA-Z][a-zA-Z0-9+.-]*:\/\//, "");
          const authority = withoutScheme.split(/[/?#]/)[0];
          const atIdx = authority.lastIndexOf("@");
          const hostSection = atIdx >= 0 ? authority.slice(atIdx + 1) : authority;
          if (u.protocol === "http:" && /:80$/.test(hostSection)) port = "80";
          else if (u.protocol === "https:" && /:443$/.test(hostSection)) port = "443";
        }
        return port ? `${u.hostname}:${port}` : u.hostname;
      }
    } catch {
      // Fallback to string parsing when URL contains unencoded characters
    }
  }

  let rest = raw.replace(/^[a-zA-Z][a-zA-Z0-9+.-]*:\/\//, "");
  rest = rest.split(/[/?#]/)[0];
  const atIdx = rest.lastIndexOf("@");
  if (atIdx >= 0) {
    rest = rest.slice(atIdx + 1);
  }
  rest = rest.trim();
  return rest || null;
}

function getEndpointKey(acc: MuseAccount): string {
  const id = acc.proxy_id?.trim();
  if (id) return `id:${id}`;
  const hp = formatProxyHostPort(acc.proxy);
  if (hp) return `url:${hp}`;
  const name = acc.proxy_name?.trim();
  if (name) return `name:${name}`;
  return "direct";
}

function describeAccountSource(acc: MuseAccount): string {
  const label = acc.label || acc.id;
  const exitIp = acc.proxy_exit_ip?.trim();
  const proxyName = acc.proxy_name?.trim();
  const hostPort = formatProxyHostPort(acc.proxy);
  if (exitIp) {
    return `${label}: ${exitIp}${proxyName ? ` (${proxyName})` : ""}`;
  }
  if (hostPort) {
    return `${label}: ${proxyName ? `${proxyName} [${hostPort}]` : hostPort}`;
  }
  if (proxyName) {
    return `${label}: ${proxyName}`;
  }
  return `${label}: trực tiếp`;
}

/**
 * Derives statusbar IP / Muse status text & tooltip from live sidecar and account pool state.
 * Never shows "IP: trực tiếp" when sidecar is stopped, and never falls back to disabled,
 * invalid, or video_locked accounts.
 */
export function deriveStatusbarIpText(
  sidecarRunning: boolean,
  accounts: readonly MuseAccount[]
): StatusbarIpResult {
  if (!sidecarRunning) {
    return {
      text: "Muse: đã dừng",
      title: "Engine VideoFlow Muse chưa chạy",
    };
  }

  if (accounts.length === 0) {
    return {
      text: "Muse: chưa có TK",
      title: "Chưa có tài khoản Muse nào trong pool",
    };
  }

  const busyAccounts = accounts.filter((a) => a.enabled && a.busy === true);
  const activeEnabled = accounts.filter((a) => a.enabled && a.status === "active");
  const candidates = busyAccounts.length > 0 ? busyAccounts : activeEnabled;

  if (candidates.length === 0) {
    const coolingCount = accounts.filter((a) => a.enabled && a.status === "cooling").length;
    if (coolingCount > 0) {
      return {
        text: `Muse: tạm chờ (${coolingCount} TK)`,
        title: `Có ${coolingCount} tài khoản đang tạm chờ (cooldown)`,
      };
    }
    return {
      text: "Muse: không có TK khả dụng",
      title: "Không có tài khoản Muse đang bật và khả dụng",
    };
  }

  const endpointMap = new Map<string, MuseAccount[]>();
  for (const acc of candidates) {
    const key = getEndpointKey(acc);
    const list = endpointMap.get(key);
    if (list) {
      list.push(acc);
    } else {
      endpointMap.set(key, [acc]);
    }
  }

  if (endpointMap.size > 1) {
    const suffix = busyAccounts.length > 0 ? "đang chạy" : "nguồn";
    return {
      text: `IP: Đa proxy (${endpointMap.size} ${suffix})`,
      title: candidates.map(describeAccountSource).join("\n"),
    };
  }

  const representative =
    candidates.find((a) => Boolean(a.proxy_exit_ip?.trim())) ?? candidates[0];
  const exitIp = representative.proxy_exit_ip?.trim();
  const proxyName = representative.proxy_name?.trim();
  const hostPort = formatProxyHostPort(representative.proxy);
  const title = candidates.map(describeAccountSource).join("\n");

  if (exitIp) {
    return {
      text: `IP: ${exitIp}${proxyName ? ` (${proxyName})` : ""}`,
      title,
    };
  }

  if (hostPort) {
    const host = extractHostFromHostPort(hostPort);
    if (isIpLiteralHost(host)) {
      return {
        text: `IP: ${hostPort}`,
        title,
      };
    }
    return {
      text: `Proxy: ${proxyName || hostPort}`,
      title,
    };
  }

  if (proxyName) {
    return {
      text: `Proxy: ${proxyName}`,
      title,
    };
  }

  return {
    text: "IP: trực tiếp",
    title,
  };
}

const ZERO_EXTRA = new Set(["0", "0.0", "none", "null", "n/a", "-", "—"]);
const QUANTITY_SUFFIX_RE = /^([0-9]*\.?[0-9]+)\s*([kmb])?\b/i;
const ISO_TIME_TAIL_RE = /T\d{2}:\d{2}.*$/;
const ZH_DATE_RE = /^(?:(\d{4})年)?\s*(\d{1,2})月(\d{1,2})日?$/;

const SHORT_MONTHS: Record<string, number> = {
  jan: 1,
  feb: 2,
  mar: 3,
  apr: 4,
  may: 5,
  jun: 6,
  jul: 7,
  aug: 8,
  sep: 9,
  oct: 10,
  nov: 11,
  dec: 12,
};

const FULL_MONTHS: Record<string, number> = {
  january: 1,
  february: 2,
  march: 3,
  april: 4,
  may: 5,
  june: 6,
  july: 7,
  august: 8,
  september: 9,
  october: 10,
  november: 11,
  december: 12,
};

function isExtraZero(text: string): boolean {
  const cleaned = text.trim().toLowerCase().replace(/,/g, "");
  if (!cleaned) return true;
  const head = cleaned.split(/\s+/)[0];
  if (ZERO_EXTRA.has(head)) return true;
  const m = QUANTITY_SUFFIX_RE.exec(cleaned);
  if (!m) return false;
  const value = Number(m[1]);
  if (!Number.isFinite(value)) return false;
  return value <= 0;
}

function isValidYmd(year: number, month1: number, day: number): boolean {
  if (!Number.isInteger(year) || !Number.isInteger(month1) || !Number.isInteger(day)) {
    return false;
  }
  if (year < 100 || month1 < 1 || month1 > 12 || day < 1 || day > 31) {
    return false;
  }
  const d = new Date(year, month1 - 1, day, 12, 0, 0, 0);
  return d.getFullYear() === year && d.getMonth() === month1 - 1 && d.getDate() === day;
}

function localEndOfDaySec(year: number, month1: number, day: number): number {
  return new Date(year, month1 - 1, day, 23, 59, 59, 0).getTime() / 1000;
}

function safeReplaceYearEndOfDaySec(year: number, month1: number, day: number): number {
  const safeDay = isValidYmd(year, month1, day) ? day : 28;
  return localEndOfDaySec(year, month1, safeDay);
}

function resolveNoYearDeadlineSec(
  month1: number,
  day: number,
  nowSec: number,
  checkedAtSec?: number | null
): number | null {
  if (!isValidYmd(2024, month1, day)) return null;
  const anchorSec =
    typeof checkedAtSec === "number" && Number.isFinite(checkedAtSec) && checkedAtSec > 0
      ? checkedAtSec
      : nowSec;
  const anchorYear = new Date(anchorSec * 1000).getFullYear();
  let candSec = safeReplaceYearEndOfDaySec(anchorYear, month1, day);
  if (candSec < anchorSec) {
    candSec = safeReplaceYearEndOfDaySec(anchorYear + 1, month1, day);
  }
  return candSec - nowSec;
}

function resetDeadlineSec(
  resetsAt: unknown,
  nowSec: number,
  checkedAt?: unknown
): number | null {
  if (resetsAt === null || resetsAt === undefined || resetsAt === "" || resetsAt === 0) {
    return null;
  }
  if (typeof resetsAt === "number") {
    return Number.isFinite(resetsAt) ? resetsAt - nowSec : null;
  }
  const checkedAtSec =
    typeof checkedAt === "number" && Number.isFinite(checkedAt) ? checkedAt : null;

  let text = String(resetsAt).trim();
  if (!text) return null;
  text = text.replace(ISO_TIME_TAIL_RE, "");
  for (const sep of [" at ", " (", " – ", " - "]) {
    const idx = text.indexOf(sep);
    if (idx !== -1) {
      text = text.slice(0, idx);
    }
  }
  text = text.trim().replace(/,+$/, "").trim();
  if ((text.match(/,/g) || []).length === 2) {
    text = text.slice(0, text.lastIndexOf(",")).trim();
  }

  const zm = ZH_DATE_RE.exec(text);
  if (zm) {
    const mo = Number(zm[2]);
    const dy = Number(zm[3]);
    if (zm[1]) {
      const yr = Number(zm[1]);
      return isValidYmd(yr, mo, dy) ? localEndOfDaySec(yr, mo, dy) - nowSec : null;
    }
    return resolveNoYearDeadlineSec(mo, dy, nowSec, checkedAtSec);
  }

  const mCommaYear = /^([A-Za-z]+)\s+(\d{1,2}),\s+(\d{4})$/.exec(text);
  if (mCommaYear) {
    const key = mCommaYear[1].toLowerCase();
    const mo = SHORT_MONTHS[key] ?? FULL_MONTHS[key];
    const dy = Number(mCommaYear[2]);
    const yr = Number(mCommaYear[3]);
    if (mo && isValidYmd(yr, mo, dy)) {
      return localEndOfDaySec(yr, mo, dy) - nowSec;
    }
    return null;
  }

  const mShortNoComma = /^([A-Za-z]+)\s+(\d{1,2})\s+(\d{4})$/.exec(text);
  if (mShortNoComma) {
    const mo = SHORT_MONTHS[mShortNoComma[1].toLowerCase()];
    const dy = Number(mShortNoComma[2]);
    const yr = Number(mShortNoComma[3]);
    if (mo && isValidYmd(yr, mo, dy)) {
      return localEndOfDaySec(yr, mo, dy) - nowSec;
    }
    return null;
  }

  const mIso = /^(\d{4})-(\d{1,2})-(\d{1,2})$/.exec(text);
  if (mIso) {
    const yr = Number(mIso[1]);
    const mo = Number(mIso[2]);
    const dy = Number(mIso[3]);
    return isValidYmd(yr, mo, dy) ? localEndOfDaySec(yr, mo, dy) - nowSec : null;
  }

  const mSlash = /^(\d{1,2})\/(\d{1,2})\/(\d{4})$/.exec(text);
  if (mSlash) {
    const a = Number(mSlash[1]);
    const b = Number(mSlash[2]);
    const yr = Number(mSlash[3]);
    if (isValidYmd(yr, a, b)) {
      return localEndOfDaySec(yr, a, b) - nowSec;
    }
    if (isValidYmd(yr, b, a)) {
      return localEndOfDaySec(yr, b, a) - nowSec;
    }
    return null;
  }

  const mNoYear = /^([A-Za-z]+)\s+(\d{1,2})$/.exec(text);
  if (mNoYear) {
    const key = mNoYear[1].toLowerCase();
    const mo = SHORT_MONTHS[key] ?? FULL_MONTHS[key];
    const dy = Number(mNoYear[2]);
    if (mo) {
      return resolveNoYearDeadlineSec(mo, dy, nowSec, checkedAtSec);
    }
  }

  return null;
}

/**
 * Matches Python `Account.quota_exhausted()` parity rules:
 * honors sidecar-computed `account.quota_exhausted` boolean when present,
 * otherwise requires numeric `weekly_used_pct >= 100`, empty/zero `extra_left`,
 * and a parseable `resets_at` deadline still in the future.
 */
export function isQuotaExhausted(
  account: MuseAccount,
  nowMs: number = Date.now()
): boolean {
  if (typeof account.quota_exhausted === "boolean") {
    return account.quota_exhausted;
  }
  const q = account.meta?.quota;
  if (!q) return false;
  const pct = q.weekly_used_pct;
  if (typeof pct !== "number" || !Number.isFinite(pct) || pct < 100) {
    return false;
  }
  if (typeof q.extra_left === "string" && q.extra_left.trim() && !isExtraZero(q.extra_left)) {
    return false;
  }
  const checkedAt = q.checked_at ?? q.fetched_at;
  const resetIn = resetDeadlineSec(q.resets_at, nowMs / 1000, checkedAt);
  if (resetIn === null) {
    return false;
  }
  return resetIn > 0;
}
