import { describe, it } from "node:test";
import assert from "node:assert/strict";
import type { Settings, SettingsView } from "../src/lib/api.js";
import {
  countUsableMuseVideoAccounts,
  executeSettingsSave,
  getConcurrencyMax,
  getVideoSecondsMax,
  isValidIntInRange,
  MAX_CONCURRENCY,
  validateConcurrency,
  validateVideoSeconds,
} from "../src/lib/settingsSave.js";

function makeSettings(): Settings {
  return {
    endpoint_image_base_url: "https://img.example.com",
    endpoint_image_model: "img-model",
    image_source: "gateway",
    prompt_source: "gateway", endpoint_prompt_base_url: "", endpoint_prompt_model: "",
    prompt_timeout_s: 900, reels_min_likes: 500, reels_min_views: 30000,
    reels_country: "", reels_require_buy_link: false,
    endpoint_video_base_url: "https://vid.example.com",
    endpoint_video_model: "vid-model",
    video_source: "gateway",
    muse_chromium_path: "",
    muse_headless: true,
    muse_sidecar_port: 18610,
    video_size: "1280x720",
    video_seconds: 5,
    image_timeout_s: 60,
    video_create_timeout_s: 120,
    poll_interval_s: 3,
    poll_max_s: 300,
    backoff_s: [2, 5, 10],
    max_attempts: 3,
    concurrency: 2,
    schedule_enabled: false,
    schedule_hours: 24,
    auto_cleanup_days: 7,
    auto_merge: false,
    confirm_start: true,
    notify_on_finish: false,
    minimize_to_tray: false,
    start_with_windows: false,
    recent_projects: [],
    current_project: null,
  };
}

function makeView(overrides: Partial<SettingsView> = {}): SettingsView {
  return {
    settings: makeSettings(),
    image_key_preview: "sk-...img",
    video_key_preview: "sk-...vid",
    has_image_key: true,
    has_video_key: true,
    ...overrides,
  };
}

describe("executeSettingsSave", () => {
  it("calls saveSettings BEFORE setApiKey and clears both keys on success", async () => {
    const calls: string[] = [];
    const view = makeView();

    const res = await executeSettingsSave({
      payload: makeSettings(),
      imageKeyInput: "  new-img-key  ",
      videoKeyInput: "  new-vid-key  ",
      pendingClearImageKey: false,
      pendingClearVideoKey: false,
      deps: {
        saveSettings: async () => {
          calls.push("saveSettings");
          return true;
        },
        setApiKey: async (which, value) => {
          calls.push(`setApiKey:${which}:${value}`);
          return true;
        },
        getSettings: async () => {
          calls.push("getSettings");
          return view;
        },
      },
      onImageKeyApplied: () => calls.push("onImageKeyApplied"),
      onVideoKeyApplied: () => calls.push("onVideoKeyApplied"),
    });

    assert.equal(res.ok, true);
    if (res.ok) {
      assert.deepEqual(res.view, view);
      assert.equal(res.imageKeyCleared, true);
      assert.equal(res.videoKeyCleared, true);
    }
    assert.deepEqual(calls, [
      "saveSettings",
      "setApiKey:anh:new-img-key",
      "onImageKeyApplied",
      "setApiKey:video:new-vid-key",
      "onVideoKeyApplied",
      "getSettings",
    ]);
  });

  it("leaves Keyring untouched and invokes no callbacks if saveSettings fails", async () => {
    const calls: string[] = [];

    const res = await executeSettingsSave({
      payload: makeSettings(),
      imageKeyInput: "img-key",
      videoKeyInput: "",
      pendingClearImageKey: false,
      pendingClearVideoKey: true,
      deps: {
        saveSettings: async () => {
          calls.push("saveSettings:fail");
          throw new Error("disk full");
        },
        setApiKey: async (which) => {
          calls.push(`setApiKey:${which}`);
          return true;
        },
        getSettings: async () => {
          calls.push("getSettings");
          return makeView();
        },
      },
      onImageKeyApplied: () => calls.push("onImageKeyApplied"),
      onVideoKeyApplied: () => calls.push("onVideoKeyApplied"),
    });

    assert.equal(res.ok, false);
    assert.equal(res.settingsPersisted, false);
    assert.equal(res.imageKeyCleared, false);
    assert.equal(res.videoKeyCleared, false);
    assert.deepEqual(calls, ["saveSettings:fail"]);
  });

  it("preserves failed video key on partial failure after image key succeeds", async () => {
    const calls: string[] = [];
    const refreshed = makeView({ has_video_key: false, video_key_preview: null });

    const res = await executeSettingsSave({
      payload: makeSettings(),
      imageKeyInput: "",
      videoKeyInput: "vid-secret",
      pendingClearImageKey: true,
      pendingClearVideoKey: false,
      deps: {
        saveSettings: async () => {
          calls.push("saveSettings");
          return true;
        },
        setApiKey: async (which, value) => {
          calls.push(`setApiKey:${which}:${value}`);
          if (which === "video") throw new Error("keyring locked");
          return true;
        },
        getSettings: async () => {
          calls.push("getSettings");
          return refreshed;
        },
      },
      onImageKeyApplied: () => calls.push("onImageKeyApplied"),
      onVideoKeyApplied: () => calls.push("onVideoKeyApplied"),
    });

    assert.equal(res.ok, false);
    if (!res.ok) {
      assert.equal(res.settingsPersisted, true);
      assert.equal(res.imageKeyCleared, true);
      assert.equal(res.videoKeyCleared, false);
      assert.deepEqual(res.refreshedView, refreshed);
    }
    assert.deepEqual(calls, [
      "saveSettings",
      "setApiKey:anh:",
      "onImageKeyApplied",
      "setApiKey:video:vid-secret",
      "getSettings",
    ]);
  });
});

describe("video_seconds bounds validation", () => {
  it("caps internal Muse video_seconds at 1..60 while keeping gateway at 1..120", () => {
    assert.equal(getVideoSecondsMax("internal"), 60);
    assert.equal(getVideoSecondsMax("gateway"), 120);
    assert.equal(getVideoSecondsMax(undefined), 120);

    assert.equal(validateVideoSeconds(1, "internal"), null);
    assert.equal(validateVideoSeconds(60, "internal"), null);
    assert.match(validateVideoSeconds(61, "internal") ?? "", /1 đến 60/);
    assert.match(validateVideoSeconds(120, "internal") ?? "", /1 đến 60/);
    assert.match(validateVideoSeconds(0, "internal") ?? "", /1 đến 60/);

    assert.equal(validateVideoSeconds(61, "gateway"), null);
    assert.equal(validateVideoSeconds(120, "gateway"), null);
    assert.match(validateVideoSeconds(121, "gateway") ?? "", /1 đến 120/);

    assert.equal(isValidIntInRange("", 1, 60), false);
    assert.equal(isValidIntInRange(15.5, 1, 60), false);
    assert.equal(isValidIntInRange("30", 1, 60), true);
  });
});

describe("concurrency bounds and Muse account cap", () => {
  it("counts only enabled non-invalid non-video_locked Muse accounts when sidecar runs, falling back to disk count when stopped", () => {
    const liveAccounts = [
      { enabled: true, status: "active" },
      { enabled: true, status: "cooling" },
      { enabled: false, status: "active" },
      { enabled: true, status: "invalid" },
      { enabled: true, status: "video_locked" },
    ];
    assert.equal(countUsableMuseVideoAccounts(liveAccounts, true, 9, 5), 2);
    assert.equal(countUsableMuseVideoAccounts(liveAccounts, false, 4, null), 4);
    assert.equal(countUsableMuseVideoAccounts([], false, undefined, null), 0);
    // Avoids transient 0-flicker while sidecar reports accounts > 0 before muse_list_accounts resolves
    assert.equal(countUsableMuseVideoAccounts([], true, 3, 3), 3);
    // Truly 0 accounts on running sidecar returns 0 even if fallback was stale
    assert.equal(countUsableMuseVideoAccounts([], true, 3, 0), 0);
  });

  it("caps internal Muse concurrency strictly at usable Muse accounts (min 1) while allowing up to MAX_CONCURRENCY for gateway", () => {
    assert.equal(getConcurrencyMax("gateway", 0), MAX_CONCURRENCY);
    assert.equal(getConcurrencyMax("internal", 0), 1);
    assert.equal(getConcurrencyMax("internal", 1), 1);
    assert.equal(getConcurrencyMax("internal", 5), 5);
    assert.equal(getConcurrencyMax("internal", 99), MAX_CONCURRENCY);

    // 0 accounts: 1 allowed (for saving non-concurrency settings), >=2 rejected
    assert.equal(validateConcurrency(1, "internal", 0), null);
    assert.match(validateConcurrency(2, "internal", 0) ?? "", /Chưa có tài khoản Muse/);

    // 1 account: 1 allowed, 2 rejected
    assert.equal(validateConcurrency(1, "internal", 1), null);
    assert.match(validateConcurrency(2, "internal", 1) ?? "", /không vượt quá 1 tài khoản Muse/);

    // 4 accounts: 1..4 allowed, 5 rejected
    assert.equal(validateConcurrency(4, "internal", 4), null);
    assert.match(validateConcurrency(5, "internal", 4) ?? "", /không vượt quá 4 tài khoản Muse/);

    // Gateway: up to MAX_CONCURRENCY allowed
    assert.equal(validateConcurrency(6, "gateway", 0), null);
    assert.match(validateConcurrency(MAX_CONCURRENCY + 1, "gateway", 0) ?? "", /1 đến 20/);
  });
});
