import { describe, it } from "node:test";
import assert from "node:assert/strict";
import type { MuseAccount } from "../src/lib/api.js";
import {
  deriveStatusbarIpText,
  formatProxyHostPort,
  resolveLockedAccount,
} from "../src/lib/museStatus.js";

function makeAccount(overrides: Partial<MuseAccount> = {}): MuseAccount {
  return {
    id: "acc-1",
    label: "Account 1",
    enabled: true,
    status: "active",
    cookies: {},
    cookie_expires: {},
    created_at: 0,
    last_used_at: 0,
    cooldown_until: 0,
    last_error: null,
    success_count: 0,
    fail_count: 0,
    meta: {},
    ...overrides,
  };
}

describe("resolveLockedAccount", () => {
  it("returns null whenever sidecar is stopped even if eventLocked is set", () => {
    const locked = resolveLockedAccount(
      [makeAccount({ id: "a1", status: "video_locked", enabled: true })],
      { id: "a1", label: "Account 1" },
      false
    );
    assert.equal(locked, null);
  });

  it("prefers enabled video_locked account from pool", () => {
    const locked = resolveLockedAccount(
      [makeAccount({ id: "a2", label: "Locked Acc", enabled: true, status: "video_locked" })],
      null,
      true
    );
    assert.deepEqual(locked, { id: "a2", label: "Locked Acc" });
  });

  it("clears eventLocked when matched account is disabled, unlocked, or deleted from pool", () => {
    assert.equal(
      resolveLockedAccount(
        [makeAccount({ id: "a1", enabled: false, status: "video_locked" })],
        { id: "a1", label: "A1" },
        true
      ),
      null
    );
    assert.equal(
      resolveLockedAccount(
        [makeAccount({ id: "a1", enabled: true, status: "active" })],
        { id: "a1", label: "A1" },
        true
      ),
      null
    );
    assert.equal(
      resolveLockedAccount(
        [makeAccount({ id: "a2", enabled: true, status: "active" })],
        { id: "a1", label: "A1" },
        true
      ),
      null
    );
  });

  it("keeps eventLocked briefly when sidecar is running and pool is empty", () => {
    assert.deepEqual(
      resolveLockedAccount([], { id: "a1", label: "A1" }, true),
      { id: "a1", label: "A1" }
    );
  });
});

describe("formatProxyHostPort", () => {
  it("strips scheme, credentials (including @ in password), and path while keeping IPv6 brackets", () => {
    assert.equal(formatProxyHostPort("http://user:pass@1.2.3.4:8080/foo"), "1.2.3.4:8080");
    assert.equal(formatProxyHostPort("socks5h://***@[2001:db8::1]:1080"), "[2001:db8::1]:1080");
    assert.equal(formatProxyHostPort("http://u:p@ss@proxy.example.com:80"), "proxy.example.com:80");
    assert.equal(formatProxyHostPort("  "), null);
    assert.equal(formatProxyHostPort(null), null);
  });
});

describe("deriveStatusbarIpText", () => {
  it("never shows IP: trực tiếp when sidecar is stopped", () => {
    const res = deriveStatusbarIpText(false, []);
    assert.equal(res.text, "Muse: đã dừng");
    assert.notEqual(res.text, "IP: trực tiếp");
  });

  it("shows empty pool message when sidecar running with 0 accounts", () => {
    assert.equal(deriveStatusbarIpText(true, []).text, "Muse: chưa có TK");
  });

  it("never falls back to disabled, invalid, or video_locked accounts", () => {
    const res = deriveStatusbarIpText(true, [
      makeAccount({ id: "a1", enabled: false, status: "active", proxy_exit_ip: "9.9.9.9" }),
      makeAccount({ id: "a2", enabled: true, status: "invalid", proxy_exit_ip: "8.8.8.8" }),
      makeAccount({ id: "a3", enabled: true, status: "video_locked", proxy_exit_ip: "7.7.7.7" }),
    ]);
    assert.equal(res.text, "Muse: không có TK khả dụng");
  });

  it("prefers busy accounts over merely active accounts", () => {
    const res = deriveStatusbarIpText(true, [
      makeAccount({ id: "a1", enabled: true, status: "active", proxy_exit_ip: "1.1.1.1", proxy_name: "P1" }),
      makeAccount({
        id: "a2",
        enabled: true,
        status: "active",
        busy: true,
        proxy_exit_ip: "2.2.2.2",
        proxy_name: "P2",
      }),
    ]);
    assert.equal(res.text, "IP: 2.2.2.2 (P2)");
  });

  it("distinguishes IP literal vs unverified domain vs direct vs multi-proxy pool", () => {
    assert.equal(
      deriveStatusbarIpText(true, [
        makeAccount({ proxy: "socks5h://u:p@203.0.113.5:1080" }),
      ]).text,
      "IP: 203.0.113.5:1080"
    );

    assert.equal(
      deriveStatusbarIpText(true, [
        makeAccount({ proxy: "http://proxy.corp.local:3128", proxy_name: "Corp" }),
      ]).text,
      "Proxy: Corp"
    );

    assert.equal(
      deriveStatusbarIpText(true, [
        makeAccount({ proxy: "http://proxy.corp.local:3128" }),
      ]).text,
      "Proxy: proxy.corp.local:3128"
    );

    assert.equal(
      deriveStatusbarIpText(true, [makeAccount({ proxy: null })]).text,
      "IP: trực tiếp"
    );

    assert.equal(
      deriveStatusbarIpText(true, [
        makeAccount({ id: "a1", busy: true, proxy_id: "p1", proxy_exit_ip: "1.1.1.1" }),
        makeAccount({ id: "a2", busy: true, proxy_id: "p2", proxy_exit_ip: "2.2.2.2" }),
      ]).text,
      "IP: Đa proxy (2 đang chạy)"
    );

    assert.equal(
      deriveStatusbarIpText(true, [
        makeAccount({ id: "a1", proxy_id: "p1", proxy_exit_ip: "1.1.1.1" }),
        makeAccount({ id: "a2", proxy: null }),
      ]).text,
      "IP: Đa proxy (2 nguồn)"
    );
  });
});
