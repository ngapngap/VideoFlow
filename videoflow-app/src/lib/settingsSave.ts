import type { Settings, SettingsView } from "./api";

/** Returns true when `val` is a finite integer within `[min, max]`. */
export function isValidIntInRange(val: unknown, min: number, max: number): boolean {
  if (val === null || val === undefined || (typeof val === "string" && val.trim() === "")) {
    return false;
  }
  const n = Number(val);
  return Number.isFinite(n) && Number.isInteger(n) && n >= min && n <= max;
}

/** Global hard ceiling on concurrent worker threads per pipeline stage. */
export const MAX_CONCURRENCY = 20;

/**
 * Counts Muse accounts eligible to run video tasks (`enabled !== false`, not `invalid` / `video_locked`).
 * Prefers live sidecar account list when running, while avoiding a transient 0-flicker when the
 * sidecar reports accounts present (`sidecarAccountTotal > 0`) before `muse_list_accounts()` resolves.
 */
export function countUsableMuseVideoAccounts(
  accounts: { enabled?: boolean; status?: string }[] | null | undefined,
  sidecarRunning: boolean,
  fallbackCount?: number | null,
  sidecarAccountTotal?: number | null
): number {
  if (sidecarRunning && Array.isArray(accounts)) {
    const listStillLoading =
      accounts.length === 0 &&
      typeof sidecarAccountTotal === "number" &&
      Number.isFinite(sidecarAccountTotal) &&
      sidecarAccountTotal > 0;
    if (!listStillLoading) {
      return accounts.filter(
        (a) => a.enabled !== false && a.status !== "invalid" && a.status !== "video_locked"
      ).length;
    }
  }
  if (typeof fallbackCount === "number" && Number.isFinite(fallbackCount)) {
    return Math.max(0, Math.floor(fallbackCount));
  }
  return 0;
}

/**
 * Maximum concurrency selectable in UI:
 * - When `videoSource === "internal"`, strictly capped by usable Muse accounts (minimum 1 so settings
 *   remain savable before logging in an account) to guarantee at most 1 video per Muse account.
 * - Otherwise (external gateway), up to `MAX_CONCURRENCY`.
 */
export function getConcurrencyMax(
  videoSource?: string | null,
  museVideoAccounts?: number | null
): number {
  if (videoSource === "internal") {
    const count =
      typeof museVideoAccounts === "number" && Number.isFinite(museVideoAccounts)
        ? Math.max(0, Math.floor(museVideoAccounts))
        : 0;
    return Math.min(MAX_CONCURRENCY, Math.max(1, count));
  }
  return MAX_CONCURRENCY;
}

/** Validates `concurrency` against video source and usable Muse account count, returning a Vietnamese error or null. */
export function validateConcurrency(
  concurrency: unknown,
  videoSource?: string | null,
  museVideoAccounts?: number | null
): string | null {
  const max = getConcurrencyMax(videoSource, museVideoAccounts);
  if (isValidIntInRange(concurrency, 1, max)) {
    return null;
  }
  if (videoSource === "internal") {
    const count =
      typeof museVideoAccounts === "number" && Number.isFinite(museVideoAccounts)
        ? Math.max(0, Math.floor(museVideoAccounts))
        : 0;
    if (count <= 0) {
      return "Chưa có tài khoản Muse khả dụng — chỉ cho phép 1 luồng cho đến khi thêm tài khoản Muse.";
    }
    return `Số luồng chạy đồng thời phải từ 1 đến ${max} (không vượt quá ${count} tài khoản Muse khả dụng để đảm bảo mỗi tài khoản chỉ chạy tối đa 1 video).`;
  }
  return `Số luồng chạy đồng thời phải là số nguyên từ 1 đến ${MAX_CONCURRENCY}.`;
}

/** Maximum `video_seconds` allowed by the selected video backend (`internal` Muse caps at 60s). */
export function getVideoSecondsMax(videoSource?: string | null): number {
  return videoSource === "internal" ? 60 : 120;
}

/** Validates `video_seconds` against the active video source, returning a Vietnamese error or null. */
export function validateVideoSeconds(
  videoSeconds: unknown,
  videoSource?: string | null
): string | null {
  const max = getVideoSecondsMax(videoSource);
  if (isValidIntInRange(videoSeconds, 1, max)) {
    return null;
  }
  return videoSource === "internal"
    ? "Thời lượng video (VideoFlow Muse nội bộ) phải là số nguyên từ 1 đến 60 giây."
    : "Thời lượng video phải là số nguyên từ 1 đến 120 giây.";
}

export interface SettingsSaveDeps {
  saveSettings: (payload: Settings) => Promise<boolean>;
  setApiKey: (which: "anh" | "video", value: string) => Promise<boolean>;
  getSettings: () => Promise<SettingsView>;
}

export interface ExecuteSettingsSaveParams {
  payload: Settings;
  imageKeyInput: string;
  videoKeyInput: string;
  pendingClearImageKey: boolean;
  pendingClearVideoKey: boolean;
  deps: SettingsSaveDeps;
  onImageKeyApplied?: () => void;
  onVideoKeyApplied?: () => void;
}

export type ExecuteSettingsSaveResult =
  | {
      ok: true;
      settingsPersisted: true;
      view: SettingsView;
      imageKeyCleared: boolean;
      videoKeyCleared: boolean;
    }
  | {
      ok: false;
      settingsPersisted: boolean;
      error: unknown;
      refreshedView?: SettingsView;
      imageKeyCleared: boolean;
      videoKeyCleared: boolean;
    };

/**
 * Saves settings.json BEFORE mutating OS Keyring API keys.
 * Invokes `onImageKeyApplied` / `onVideoKeyApplied` immediately after each individual
 * Keyring write succeeds so partial failures preserve the failed key input/clear flag.
 */
export async function executeSettingsSave(
  params: ExecuteSettingsSaveParams
): Promise<ExecuteSettingsSaveResult> {
  let settingsPersisted = false;
  let imageKeyCleared = false;
  let videoKeyCleared = false;

  try {
    await params.deps.saveSettings(params.payload);
    settingsPersisted = true;

    const trimmedImg = params.imageKeyInput.trim();
    if (trimmedImg) {
      await params.deps.setApiKey("anh", trimmedImg);
      imageKeyCleared = true;
      params.onImageKeyApplied?.();
    } else if (params.pendingClearImageKey) {
      await params.deps.setApiKey("anh", "");
      imageKeyCleared = true;
      params.onImageKeyApplied?.();
    }

    const trimmedVid = params.videoKeyInput.trim();
    if (trimmedVid) {
      await params.deps.setApiKey("video", trimmedVid);
      videoKeyCleared = true;
      params.onVideoKeyApplied?.();
    } else if (params.pendingClearVideoKey) {
      await params.deps.setApiKey("video", "");
      videoKeyCleared = true;
      params.onVideoKeyApplied?.();
    }

    const view = await params.deps.getSettings();
    return {
      ok: true,
      settingsPersisted: true,
      view,
      imageKeyCleared,
      videoKeyCleared,
    };
  } catch (error) {
    let refreshedView: SettingsView | undefined;
    if (settingsPersisted) {
      try {
        refreshedView = await params.deps.getSettings();
      } catch {
        // Preserve original save/keyring error if secondary refresh fails
      }
    }
    return {
      ok: false,
      settingsPersisted,
      error,
      refreshedView,
      imageKeyCleared,
      videoKeyCleared,
    };
  }
}
