import { describe, it } from "node:test";
import assert from "node:assert/strict";
import type { MuseAccount } from "../src/lib/api.js";
import {
  deriveStatusbarIpText,
  formatProxyHostPort,
  isQuotaExhausted,
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

describe("isQuotaExhausted", () => {
  function quotaSnap(overrides: Partial<NonNullable<MuseAccount["meta"]["quota"]>> = {}) {
    return {
      plan: "free",
      weekly_used_pct: 100,
      resets_at: null,
      extra_left: "0",
      found: true,
      ...overrides,
    };
  }

  it("honors explicit backend quota_exhausted flag when present", () => {
    const flaggedTrue = makeAccount({
      quota_exhausted: true,
      meta: { quota: quotaSnap({ weekly_used_pct: 10 }) },
    });
    const flaggedFalse = makeAccount({
      quota_exhausted: false,
      meta: { quota: quotaSnap({ weekly_used_pct: 100, resets_at: "2099-12-31" }) },
    });
    assert.equal(isQuotaExhausted(flaggedTrue), true);
    assert.equal(isQuotaExhausted(flaggedFalse), false);
  });

  it("handles zero extra tokens, unparseable/missing resets, and expired resets like Python", () => {
    const nowMs = new Date(2026, 4, 10, 12, 0, 0).getTime();

    assert.equal(
      isQuotaExhausted(
        makeAccount({ meta: { quota: quotaSnap({ resets_at: null }) } }),
        nowMs
      ),
      false
    );

    assert.equal(
      isQuotaExhausted(
        makeAccount({ meta: { quota: quotaSnap({ resets_at: "next Tuesday??" }) } }),
        nowMs
      ),
      false
    );

    assert.equal(
      isQuotaExhausted(
        makeAccount({
          meta: {
            quota: quotaSnap({ extra_left: "0B tokens left", resets_at: "May 20, 2026" }),
          },
        }),
        nowMs
      ),
      true
    );

    assert.equal(
      isQuotaExhausted(
        makeAccount({
          meta: {
            quota: quotaSnap({ extra_left: "1,200 tokens", resets_at: "May 20, 2026" }),
          },
        }),
        nowMs
      ),
      false
    );

    assert.equal(
      isQuotaExhausted(
        makeAccount({
          meta: { quota: quotaSnap({ extra_left: "none", resets_at: "2026-05-09" }) },
        }),
        nowMs
      ),
      false
    );
  });

  it("anchors year-less English and Chinese dates against checked_at", () => {
    const checkedAtSec = Math.floor(new Date(2025, 11, 20, 10, 0, 0).getTime() / 1000);
    const beforeJanResetMs = new Date(2026, 0, 4, 12, 0, 0).getTime();
    const afterJanResetMs = new Date(2026, 0, 6, 1, 0, 0).getTime();

    const enAcc = makeAccount({
      meta: {
        quota: quotaSnap({
          extra_left: "0.0",
          resets_at: "Jan 5",
          checked_at: checkedAtSec,
        }),
      },
    });
    assert.equal(isQuotaExhausted(enAcc, beforeJanResetMs), true);
    assert.equal(isQuotaExhausted(enAcc, afterJanResetMs), false);

    const zhAcc = makeAccount({
      meta: {
        quota: quotaSnap({
          extra_left: "0",
          resets_at: "1月5日",
          fetched_at: checkedAtSec,
        }),
      },
    });
    assert.equal(isQuotaExhausted(zhAcc, beforeJanResetMs), true);
    assert.equal(isQuotaExhausted(zhAcc, afterJanResetMs), false);
  });
});
