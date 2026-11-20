import type { MuseAccount, ProxyInput, ProxyItem } from "./api";

export interface ParsedProxyPaste {
  kind?: "http" | "socks5";
  host: string;
  port?: number;
  username?: string;
  /** Raw password — NEVER trimmed. */
  password?: string;
}

export type HostValidationResult =
  | { ok: true; host: string }
  | { ok: false; error: string };

const STRICT_PORT_RE = /^\d{1,5}$/;
const CONTROL_CHAR_RE = /[\u0000-\u001f\u007f]/;
const HEX_HEXTET_RE = /^[0-9a-fA-F]{0,4}$/;

function parseStrictPort(rawPort: string): number | undefined {
  if (!STRICT_PORT_RE.test(rawPort)) return undefined;
  const n = Number(rawPort);
  return Number.isInteger(n) && n >= 1 && n <= 65535 ? n : undefined;
}

function safeDecodeURIComponent(raw: string): string {
  try {
    return decodeURIComponent(raw);
  } catch {
    return raw;
  }
}

function stripOuterBrackets(host: string): string {
  return host.startsWith("[") && host.endsWith("]") ? host.slice(1, -1) : host;
}

/** Validates an unbracketed IPv6 literal using WHATWG URL parsing (no zone IDs). */
export function isValidIpv6Address(raw: string): boolean {
  const s = raw.trim();
  if (!s || s.includes("[") || s.includes("]") || !s.includes(":")) return false;
  try {
    const u = new URL(`http://[${s}]`);
    return u.hostname.startsWith("[") && u.hostname.endsWith("]");
  } catch {
    return false;
  }
}

/**
 * Parses pasted proxy strings:
 * - URL: http(s)://[user:pass@]host[:port], socks5(h)://[user:pass@]host[:port]
 * - Bracketed IPv6: [ipv6]:port or [ipv6]:port:user:pass (password may contain ':')
 * - Host/IPv4: host:port or host:port:user:pass
 * Returns null if the pasted text should be inserted normally (e.g. plain hostname or unclosed IPv6).
 */
export function parseProxyPaste(rawInput: string): ParsedProxyPaste | null {
  // Strip leading whitespace and trailing newlines only — NEVER trim trailing spaces
  // before splitting because trailing spaces may belong to the password.
  const raw = rawInput.replace(/^\s+/, "").replace(/[\r\n]+$/, "");
  const trimmed = raw.trim();
  if (!trimmed) return null;

  // 1. URL format
  if (/^(https?|socks5h?):\/\//i.test(trimmed)) {
    try {
      const u = new URL(trimmed);
      const kind: "http" | "socks5" = u.protocol.toLowerCase().startsWith("socks5")
        ? "socks5"
        : "http";
      const cleanHost = stripOuterBrackets(u.hostname);
      if (!cleanHost) return null;

      let port: number | undefined;
      if (u.port) {
        port = parseStrictPort(u.port);
        if (port === undefined) return null;
      } else {
        // WHATWG URL strips explicit :80 for http:// and :443 for https://
        const authority = trimmed.replace(/^[a-z0-9]+:\/\/(?:[^@/]*@)?/i, "").split(/[/?#]/)[0];
        if (u.protocol === "http:" && /:80$/.test(authority)) port = 80;
        else if (u.protocol === "https:" && /:443$/.test(authority)) port = 443;
        else if (/:0$/.test(authority)) return null;
      }

      const out: ParsedProxyPaste = { kind, host: cleanHost };
      if (port !== undefined) out.port = port;
      if (u.username) out.username = safeDecodeURIComponent(u.username);
      if (u.password) out.password = safeDecodeURIComponent(u.password); // NEVER trim()
      return out;
    } catch {
      return null;
    }
  }

  // 2. Bracketed IPv6: [ipv6]:port or [ipv6]:port:user:pass
  if (raw.startsWith("[")) {
    const closeIdx = raw.indexOf("]");
    if (closeIdx <= 1) return null;
    const innerIpv6 = raw.slice(1, closeIdx).trim();
    if (!isValidIpv6Address(innerIpv6)) return null;

    const rest = raw.slice(closeIdx + 1);
    if (!rest.trim()) {
      return { host: innerIpv6 };
    }
    if (!rest.startsWith(":")) return null;
    const tailParts = rest.slice(1).split(":");
    // Valid tails after `]`: `[port]` (1 part) or `[port, user, ...passParts]` (>= 3 parts).
    if (tailParts.length === 2 || tailParts.length < 1) return null;
    const port = parseStrictPort(tailParts[0].trim());
    if (port === undefined) return null;

    const out: ParsedProxyPaste = { host: innerIpv6, port };
    if (tailParts.length >= 3) {
      const user = tailParts[1].trim();
      const pass = tailParts.slice(2).join(":"); // NEVER trim(); preserves ':' and trailing spaces
      if (!user) return null;
      out.username = user;
      if (pass.length > 0) out.password = pass;
    }
    return out;
  }

  // 3. Unbracketed host:port or host:port:user:pass with IPv6 colon guard
  const colonCount = (trimmed.match(/:/g) || []).length;
  if (colonCount === 0) return null;

  // Guard: never split unclosed IPv6 addresses (e.g. "::1", "2001:1234::1", "fe80::1:8080",
  // "2001:1234:abcd:ef01", or any address with "::" or != 1 and != 3 colons).
  if (trimmed.includes("::")) return null;
  if (colonCount !== 1 && colonCount !== 3) return null;

  const parts = raw.split(":");
  if (parts.length === 2) {
    const host = parts[0].trim();
    const port = parseStrictPort(parts[1].trim());
    if (!host || port === undefined) return null;
    return { host, port };
  }

  // parts.length === 4 (colonCount === 3): reject if all segments look like IPv6 hextets
  if (trimmed.split(":").every((seg) => HEX_HEXTET_RE.test(seg))) {
    return null;
  }
  const host = parts[0].trim();
  const port = parseStrictPort(parts[1].trim());
  const user = parts[2].trim();
  const pass = parts[3]; // NEVER trim()
  if (!host || port === undefined || !user) return null;

  const out: ParsedProxyPaste = { host, port, username: user };
  if (pass.length > 0) out.password = pass;
  return out;
}

/**
 * Client-side host validation matching Rust `proxies.rs::validate_input` (H5).
 * Rejects `/`, `\`, `@`, `?`, `#`, whitespace, control chars, mismatched brackets,
 * and `:` outside valid IPv6 literals.
 */
export function validateProxyHost(rawHost: string): HostValidationResult {
  const trimmed = rawHost.trim();
  if (!trimmed) {
    return { ok: false, error: "Vui lòng nhập địa chỉ Host / IP của proxy." };
  }
  if (
    CONTROL_CHAR_RE.test(trimmed) ||
    /\s/.test(trimmed) ||
    trimmed.includes("/") ||
    trimmed.includes("\\") ||
    trimmed.includes("@") ||
    trimmed.includes("?") ||
    trimmed.includes("#")
  ) {
    return {
      ok: false,
      error:
        "Host/IP proxy không hợp lệ (không chứa khoảng trắng, ký tự điều khiển, '/', '\\', '@', '?' hoặc '#').",
    };
  }

  const hasOpenBracket = trimmed.includes("[");
  const hasCloseBracket = trimmed.includes("]");
  if (hasOpenBracket || hasCloseBracket) {
    if (!trimmed.startsWith("[") || !trimmed.endsWith("]")) {
      return {
        ok: false,
        error: "Địa chỉ IPv6 có dấu ngoặc '[' ']' không khớp.",
      };
    }
    const inner = trimmed.slice(1, -1).trim();
    if (!isValidIpv6Address(inner)) {
      return {
        ok: false,
        error: "Địa chỉ IPv6 trong [...] không hợp lệ.",
      };
    }
    return { ok: true, host: inner };
  }

  if (trimmed.includes(":")) {
    if (isValidIpv6Address(trimmed)) {
      return { ok: true, host: trimmed };
    }
    return {
      ok: false,
      error:
        "Host không được chứa ':' (hãy tách Port sang ô Port, hoặc nhập địa chỉ IPv6 hợp lệ).",
    };
  }

  return { ok: true, host: trimmed };
}

/** Builds ProxyInput payload without trimming password; enforces H3 clear_password contract. */
export function buildProxyInput(params: {
  name: string;
  kind: "http" | "socks5";
  host: string;
  port: number;
  username: string;
  password: string;
  note: string;
  isEditing: boolean;
  clearPassword: boolean;
}): ProxyInput {
  const base: ProxyInput = {
    name: params.name.trim(),
    kind: params.kind,
    host: params.host,
    port: params.port,
    username: params.username.trim() || null,
    note: params.note.trim() || null,
  };

  if (params.isEditing) {
    if (params.clearPassword) {
      base.password = null;
      base.clear_password = true;
    } else if (params.password.length > 0) {
      base.password = params.password; // NEVER .trim()
      base.clear_password = false;
    } else {
      base.password = null;
      base.clear_password = false;
    }
  } else {
    base.password = params.password.length > 0 ? params.password : null; // NEVER .trim()
  }

  return base;
}

/** Formats host:port, wrapping bare IPv6 literals in square brackets. */
export function formatHostPort(host: string, port: number): string {
  const h = host.trim();
  const formattedHost =
    h.includes(":") && !(h.startsWith("[") && h.endsWith("]")) ? `[${h}]` : h;
  return `${formattedHost}:${port}`;
}

/** Returns true when a batch run is active or any assigned Muse account is currently busy. */
export function isProxyBusy(
  proxy: Pick<ProxyItem, "assigned_account_ids"> | null | undefined,
  isRunning: boolean,
  accMap: ReadonlyMap<string, Pick<MuseAccount, "busy">>
): boolean {
  if (!proxy) return false;
  if (isRunning) return true;
  return proxy.assigned_account_ids.some((id) => accMap.get(id)?.busy === true);
}

/** Resolves the freshest proxy object from `proxies` by `id`, falling back to `target`. */
export function resolveLiveProxy<T extends { id: string }>(
  target: T | null,
  proxies: readonly T[]
): T | null {
  if (!target) return null;
  return proxies.find((p) => p.id === target.id) ?? target;
}
