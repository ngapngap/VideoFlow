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
