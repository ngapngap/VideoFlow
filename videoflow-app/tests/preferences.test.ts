import { describe, it } from "node:test";
import assert from "node:assert/strict";
import {
  cycleThemeMode,
  normalizeLocale,
  normalizeThemeMode,
  resolveEffectiveTheme,
  toggleLocale,
  translate,
  translateStatus,
} from "../src/lib/preferences.js";

describe("preferences theme & i18n helpers", () => {
  it("normalizes, resolves, and cycles theme modes safely", () => {
    assert.equal(normalizeThemeMode("dark"), "dark");
    assert.equal(normalizeThemeMode("light"), "light");
    assert.equal(normalizeThemeMode("system"), "system");
    assert.equal(normalizeThemeMode("unknown"), "system");

    assert.equal(resolveEffectiveTheme("system", true), "dark");
    assert.equal(resolveEffectiveTheme("system", false), "light");
    assert.equal(resolveEffectiveTheme("light", true), "light");
    assert.equal(resolveEffectiveTheme("dark", false), "dark");

    assert.equal(cycleThemeMode("system"), "light");
    assert.equal(cycleThemeMode("light"), "dark");
    assert.equal(cycleThemeMode("dark"), "system");
  });

  it("normalizes locale, interpolates parameters, and translates row statuses", () => {
    assert.equal(normalizeLocale("en"), "en");
    assert.equal(normalizeLocale("vi"), "vi");
    assert.equal(normalizeLocale("fr"), "vi");
    assert.equal(toggleLocale("vi"), "en");
    assert.equal(toggleLocale("en"), "vi");

    assert.equal(translate("vi", "header.rowsCount", { n: 12 }), "12 dòng");
    assert.equal(translate("en", "header.rowsCount", { n: 12 }), "12 rows");
    assert.equal(translate("en", "header.runningSummary", { n: 3, pct: 45 }), "Running 3 rows · 45%");

    assert.equal(translateStatus("Đang chờ", "vi"), "Đang chờ");
    assert.equal(translateStatus("Đang chờ", "en"), "Pending");
    assert.equal(translateStatus("Đã tạo video", "en"), "Video ready");
    assert.equal(translateStatus("Lỗi tạm thời", "en"), "Transient error");
  });
});
