import { describe, it } from "node:test";
import assert from "node:assert/strict";
import {
  buildProxyInput,
  formatHostPort,
  isValidIpv6Address,
  parseProxyPaste,
  validateProxyHost,
} from "../src/lib/proxyInput.js";

describe("isValidIpv6Address", () => {
  it("accepts valid unbracketed IPv6 literals", () => {
    assert.equal(isValidIpv6Address("::1"), true);
    assert.equal(isValidIpv6Address("2001:db8::1"), true);
    assert.equal(isValidIpv6Address("2001:1234::1"), true);
    assert.equal(isValidIpv6Address("::ffff:192.0.2.1"), true);
  });

  it("rejects IPv4, host:port, bracketed strings, and invalid IPv6", () => {
    assert.equal(isValidIpv6Address("1.2.3.4"), false);
    assert.equal(isValidIpv6Address("1.2.3.4:8080"), false);
    assert.equal(isValidIpv6Address("[::1]"), false);
    assert.equal(isValidIpv6Address("2001:db8:::1"), false);
    assert.equal(isValidIpv6Address("2001:1234:abcd:ef01"), false);
    assert.equal(isValidIpv6Address(""), false);
  });
});

describe("parseProxyPaste", () => {
  it("parses host:port and host:port:user:pass while preserving password spaces", () => {
    assert.deepEqual(parseProxyPaste("1.2.3.4:8080"), {
      host: "1.2.3.4",
      port: 8080,
    });
    assert.deepEqual(parseProxyPaste("proxy.example.com:3128:alice:  sec ret  "), {
      host: "proxy.example.com",
      port: 3128,
      username: "alice",
      password: "  sec ret  ",
    });
  });

  it("parses URL formats including socks5h, default ports :80/:443, and IPv6", () => {
    assert.deepEqual(parseProxyPaste("http://1.2.3.4:80"), {
      kind: "http",
      host: "1.2.3.4",
      port: 80,
    });
    assert.deepEqual(parseProxyPaste("https://proxy.local:443"), {
      kind: "http",
      host: "proxy.local",
      port: 443,
    });
    assert.deepEqual(parseProxyPaste("socks5h://u:p%201@[2001:db8::1]:1080"), {
      kind: "socks5",
      host: "2001:db8::1",
      port: 1080,
      username: "u",
      password: "p 1",
    });
    assert.deepEqual(parseProxyPaste("socks5://user:pass@10.0.0.2:1080"), {
      kind: "socks5",
      host: "10.0.0.2",
      port: 1080,
      username: "user",
      password: "pass",
    });
  });

  it("parses bracketed IPv6 host:port and host:port:user:pass including colons in password", () => {
    assert.deepEqual(parseProxyPaste("[::1]:8080"), {
      host: "::1",
      port: 8080,
    });
    assert.deepEqual(parseProxyPaste("[2001:db8::1]:1080:user:pa:ss:word "), {
      host: "2001:db8::1",
      port: 1080,
      username: "user",
      password: "pa:ss:word ",
    });
    assert.equal(parseProxyPaste("[2001:db8::1]:1080:userOnly"), null);
  });

  it("returns null for unbracketed IPv6, plain hosts, and invalid ports", () => {
    assert.equal(parseProxyPaste("1.2.3.4"), null);
    assert.equal(parseProxyPaste("::1"), null);
    assert.equal(parseProxyPaste("2001:1234::1"), null);
    assert.equal(parseProxyPaste("fe80::1:8080"), null);
    assert.equal(parseProxyPaste("2001:1234:abcd:ef01"), null);
    assert.equal(parseProxyPaste("1.2.3.4:80abc"), null);
    assert.equal(parseProxyPaste("1.2.3.4:0"), null);
    assert.equal(parseProxyPaste("1.2.3.4:65536"), null);
    assert.equal(parseProxyPaste("http://1.2.3.4:0"), null);
    assert.equal(parseProxyPaste(""), null);
  });
});

describe("validateProxyHost", () => {
  it("accepts IPv4, domain, bare IPv6, and unwraps valid bracketed IPv6", () => {
    assert.deepEqual(validateProxyHost(" 192.168.1.1 "), { ok: true, host: "192.168.1.1" });
    assert.deepEqual(validateProxyHost("proxy.example.com"), {
      ok: true,
      host: "proxy.example.com",
    });
    assert.deepEqual(validateProxyHost("2001:db8::1"), { ok: true, host: "2001:db8::1" });
    assert.deepEqual(validateProxyHost("[2001:db8::1]"), { ok: true, host: "2001:db8::1" });
  });

  it("rejects empty, invalid chars, mismatched brackets, and host:port", () => {
    assert.equal(validateProxyHost("   ").ok, false);
    assert.equal(validateProxyHost("bad host.com").ok, false);
    assert.equal(validateProxyHost("host\u0007").ok, false);
    assert.equal(validateProxyHost("host/path").ok, false);
    assert.equal(validateProxyHost("host\\path").ok, false);
    assert.equal(validateProxyHost("user@host").ok, false);
    assert.equal(validateProxyHost("host?a=1").ok, false);
    assert.equal(validateProxyHost("host#frag").ok, false);
    assert.equal(validateProxyHost("[::1").ok, false);
    assert.equal(validateProxyHost("::1]").ok, false);
    assert.equal(validateProxyHost("[not-ipv6]").ok, false);
    assert.equal(validateProxyHost("1.2.3.4:8080").ok, false);
  });
});

describe("buildProxyInput", () => {
  it("creates proxy input without trimming password", () => {
    const out = buildProxyInput({
      name: "  My Proxy  ",
      kind: "socks5",
      host: "1.2.3.4",
      port: 1080,
      username: "  user1  ",
      password: "  raw pass  ",
      note: "  note  ",
      isEditing: false,
      clearPassword: false,
    });
    assert.deepEqual(out, {
      name: "My Proxy",
      kind: "socks5",
      host: "1.2.3.4",
      port: 1080,
      username: "user1",
      password: "  raw pass  ",
      note: "note",
    });
  });

  it("handles edit mode clear_password vs keep-existing vs replace untrimmed", () => {
    const cleared = buildProxyInput({
      name: "P1",
      kind: "http",
      host: "1.2.3.4",
      port: 8080,
      username: "",
      password: "ignored",
      note: "",
      isEditing: true,
      clearPassword: true,
    });
    assert.equal(cleared.password, null);
    assert.equal(cleared.clear_password, true);

    const kept = buildProxyInput({
      name: "P1",
      kind: "http",
      host: "1.2.3.4",
      port: 8080,
      username: "",
      password: "",
      note: "",
      isEditing: true,
      clearPassword: false,
    });
    assert.equal(kept.password, null);
    assert.equal(kept.clear_password, false);

    const replaced = buildProxyInput({
      name: "P1",
      kind: "http",
      host: "1.2.3.4",
      port: 8080,
      username: "",
      password: "   ",
      note: "",
      isEditing: true,
      clearPassword: false,
    });
    assert.equal(replaced.password, "   ");
    assert.equal(replaced.clear_password, false);
  });
});

describe("formatHostPort", () => {
  it("keeps IPv4 and domain hostnames unbracketed", () => {
    assert.equal(formatHostPort("1.2.3.4", 8080), "1.2.3.4:8080");
    assert.equal(formatHostPort(" proxy.corp.local ", 3128), "proxy.corp.local:3128");
  });

  it("brackets bare IPv6 literals and avoids double-bracketing", () => {
    assert.equal(formatHostPort("::1", 1080), "[::1]:1080");
    assert.equal(formatHostPort("2001:db8::1", 8080), "[2001:db8::1]:8080");
    assert.equal(formatHostPort("[::1]", 1080), "[::1]:1080");
  });
});
