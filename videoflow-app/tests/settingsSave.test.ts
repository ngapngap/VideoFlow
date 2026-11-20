import { describe, it } from "node:test";
import assert from "node:assert/strict";
import type { Settings, SettingsView } from "../src/lib/api.js";
import {
  executeSettingsSave,
  getVideoSecondsMax,
  isValidIntInRange,
  validateVideoSeconds,
} from "../src/lib/settingsSave.js";

function makeSettings(): Settings {
  return {
    endpoint_image_base_url: "https://img.example.com",
    endpoint_image_model: "img-model",
    image_source: "gateway",
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
