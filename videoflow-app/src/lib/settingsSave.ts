import type { Settings, SettingsView } from "./api";

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
