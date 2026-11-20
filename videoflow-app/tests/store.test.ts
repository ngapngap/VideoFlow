import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { get } from "svelte/store";
import type { MuseAccount, MuseStatusView } from "../src/lib/api.js";
import {
  activeRows,
  assetCacheBust,
  bumpRowAssetRev,
  clearEventLockedAccount,
  currentRun,
  dismissToast,
  eventLockedAccount,
  formatApiError,
  lastRunFinished,
  museAccounts,
  museLoading,
  museSidecarStatus,
  pauseToast,
  progressPercent,
  progressStep,
  progressText,
  refreshMuseState,
  resetProjectRunState,
  resumeToast,
  rowAssetRevs,
  rowsUiState,
  selectedRows,
  showToast,
  toasts,
} from "../src/lib/store.js";

function makeStatus(running: boolean): MuseStatusView {
  return {
    sidecar: {
      running,
      port: running ? 18610 : null,
      version: running ? "0.4.0" : null,
      driver: running ? "browser" : null,
      account_total: running ? 1 : 0,
      account_available: running ? 1 : 0,
      cfg_port: 18610,
      cfg_chromium: "",
      cfg_headless: true,
      restart_required: false,
    },
    video_source: "internal",
    chromium_path: "",
    headless: true,
  };
}

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

async function withMockInvoke(
  handler: (cmd: string, args?: Record<string, unknown>) => Promise<unknown>,
  fn: () => Promise<void>
): Promise<void> {
  const g = globalThis as { window?: unknown };
  const hadWindow = Object.prototype.hasOwnProperty.call(g, "window");
  const prevWindow = g.window;
  g.window = {
    __TAURI_INTERNALS__: {
      invoke: handler,
    },
  };
  try {
    await fn();
  } finally {
    if (hadWindow) {
      g.window = prevWindow;
    } else {
      delete g.window;
    }
  }
}

function clearAllToasts(): void {
  for (const t of get(toasts)) {
    dismissToast(t.id);
  }
}

describe("formatApiError", () => {
  it("formats nullish, string, JSON-encoded, and structured error values", () => {
    assert.equal(formatApiError(null), "Đã xảy ra lỗi không xác định.");
    assert.equal(formatApiError(undefined), "Đã xảy ra lỗi không xác định.");
    assert.equal(formatApiError(""), "Đã xảy ra lỗi không xác định.");

    assert.equal(formatApiError("  plain failure  "), "plain failure");
    assert.equal(
      formatApiError('{"code":"E_TIMEOUT","message":"Timed out"}'),
      "[E_TIMEOUT] Timed out"
    );
    assert.equal(
      formatApiError('{"code":"E_TIMEOUT","message":"[E_TIMEOUT] Already tagged"}'),
      "[E_TIMEOUT] Already tagged"
    );
    assert.equal(formatApiError('{"code":"ONLY_CODE"}'), "ONLY_CODE");
    assert.equal(formatApiError("{not valid json}"), "{not valid json}");

    assert.equal(
      formatApiError({ code: "MUSE_422", message: "Invalid duration" }),
      "[MUSE_422] Invalid duration"
    );
    assert.equal(
      formatApiError({ code: "MUSE_422", message: "MUSE_422 already present" }),
      "MUSE_422 already present"
    );
    assert.equal(formatApiError({ message: "Only message" }), "Only message");
    assert.equal(formatApiError({ code: "ONLY_OBJ_CODE" }), "ONLY_OBJ_CODE");
    assert.equal(formatApiError({ detail: 500 }), '{"detail":500}');
  });
});

describe("toast queue and pause/resume timers", () => {
  it("caps active toasts at 5 and supports dismissToast", () => {
    clearAllToasts();
    const ids: number[] = [];
    try {
      for (let i = 1; i <= 6; i++) {
        ids.push(showToast("info", `toast-${i}`, 0));
      }
      assert.deepEqual(
        get(toasts).map((t) => t.message),
        ["toast-2", "toast-3", "toast-4", "toast-5", "toast-6"]
      );

      dismissToast(ids[2]);
      assert.deepEqual(
        get(toasts).map((t) => t.message),
        ["toast-2", "toast-4", "toast-5", "toast-6"]
      );
    } finally {
      clearAllToasts();
    }
  });

  it("pauses and resumes toast auto-dismiss timer cleanly", () => {
    clearAllToasts();
    const origSetTimeout = globalThis.setTimeout;
    const origClearTimeout = globalThis.clearTimeout;
    const scheduled = new Map<number, { fn: () => void; ms: number }>();
    let fakeHandle = 100;

    (globalThis as { setTimeout: unknown }).setTimeout = (
      fn: (...args: unknown[]) => void,
      ms?: number
    ) => {
      const id = ++fakeHandle;
      scheduled.set(id, { fn: () => fn(), ms: ms ?? 0 });
      return id as unknown as ReturnType<typeof setTimeout>;
    };
    (globalThis as { clearTimeout: unknown }).clearTimeout = (handle: unknown) => {
      scheduled.delete(Number(handle));
    };

    try {
      const id = showToast("warn", "Hold me", 2000);
      assert.equal(get(toasts).length, 1);
      assert.equal(scheduled.size, 1);

      pauseToast(id);
      assert.equal(scheduled.size, 0);
      assert.equal(get(toasts).length, 1);

      resumeToast(id);
      assert.equal(scheduled.size, 1);
      const entry = Array.from(scheduled.values())[0];
      assert.ok(entry.ms >= 800);

      entry.fn();
      assert.equal(get(toasts).length, 0);
    } finally {
      (globalThis as { setTimeout: unknown }).setTimeout = origSetTimeout;
      (globalThis as { clearTimeout: unknown }).clearTimeout = origClearTimeout;
      clearAllToasts();
    }
  });
});

describe("bumpRowAssetRev and resetProjectRunState", () => {
  it("updates per-row or global cache bust and resets run state while preserving layout prefs", () => {
    bumpRowAssetRev("row-a");
    assert.equal(typeof get(rowAssetRevs)["row-a"], "number");

    const prevBust = get(assetCacheBust);
    bumpRowAssetRev("  ");
    assert.ok(get(assetCacheBust) >= prevBust);

    lastRunFinished.set({ run_id: 9 });
    currentRun.set({ run_id: 9, total: 10 });
    progressText.set("5/10");
    progressPercent.set(50);
    progressStep.set({ rowKey: "row-a", step: "video", detail: "running" });
    activeRows.set({ "row-a": { step: "video", detail: "running" } });
    selectedRows.set(new Set(["row-a"]));
    rowsUiState.set({
      page: 4,
      pageSize: 50,
      chip: "Lỗi",
      search: "cat",
      viewMode: "grid",
    });

    resetProjectRunState();

    assert.equal(get(lastRunFinished), null);
    assert.equal(get(currentRun), null);
    assert.equal(get(progressText), "");
    assert.equal(get(progressPercent), 0);
    assert.equal(get(progressStep), null);
    assert.deepEqual(get(activeRows), {});
    assert.equal(get(selectedRows).size, 0);
    assert.deepEqual(get(rowAssetRevs), {});
    assert.deepEqual(get(rowsUiState), {
      page: 1,
      pageSize: 50,
      chip: "Tất cả",
      search: "",
      viewMode: "grid",
    });
  });
});

describe("refreshMuseState and clearEventLockedAccount", () => {
  it("coalesces concurrent non-forced refreshes and updates status/accounts", async () => {
    museSidecarStatus.set(null);
    museAccounts.set([]);
    eventLockedAccount.set(null);
    museLoading.set(false);

    let statusCalls = 0;
    let listCalls = 0;
    const acc = makeAccount({ id: "a1", label: "Primary" });

    await withMockInvoke(
      async (cmd) => {
        if (cmd === "muse_sidecar_status") {
          statusCalls += 1;
          await new Promise((r) => setTimeout(r, 10));
          return makeStatus(true);
        }
        if (cmd === "muse_list_accounts") {
          listCalls += 1;
          return { data: [acc] };
        }
        throw new Error(`unexpected cmd: ${cmd}`);
      },
      async () => {
        const p1 = refreshMuseState();
        const p2 = refreshMuseState();
        assert.equal(p1, p2);
        assert.equal(get(museLoading), true);

        const res = await p1;
        assert.equal(res.error, null);
        assert.equal(statusCalls, 1);
        assert.equal(listCalls, 1);
        assert.equal(get(museLoading), false);
        assert.equal(get(museSidecarStatus)?.sidecar.running, true);
        assert.deepEqual(get(museAccounts), [acc]);
      }
    );
  });

  it("discards stale forced refresh results and clears locked account when sidecar stops", async () => {
    museSidecarStatus.set(makeStatus(true));
    museAccounts.set([makeAccount({ id: "old", status: "video_locked" })]);
    eventLockedAccount.set({ id: "old", label: "Old" });

    let releaseFirstList!: () => void;
    const firstListGate = new Promise<void>((res) => {
      releaseFirstList = res;
    });
    let callIndex = 0;

    await withMockInvoke(
      async (cmd) => {
        if (cmd === "muse_sidecar_status") {
          callIndex += 1;
          return makeStatus(callIndex === 1);
        }
        if (cmd === "muse_list_accounts") {
          await firstListGate;
          return { data: [makeAccount({ id: "stale-acc" })] };
        }
        throw new Error(`unexpected cmd: ${cmd}`);
      },
      async () => {
        const pStale = refreshMuseState({ force: true });
        await new Promise((r) => setTimeout(r, 5));

        const pNew = refreshMuseState({ force: true });
        const newRes = await pNew;
        assert.equal(newRes.status?.sidecar.running, false);
        assert.deepEqual(get(museAccounts), []);
        assert.equal(get(eventLockedAccount), null);
        assert.equal(get(museLoading), false);

        releaseFirstList();
        await pStale;
        assert.deepEqual(get(museAccounts), []);
        assert.equal(get(museLoading), false);
      }
    );
  });

  it("handles MUSE_SIDECAR_STOPPED in muse_list_accounts and muse_sidecar_status", async () => {
    const acc = makeAccount({ id: "a1", status: "video_locked" });
    museSidecarStatus.set(makeStatus(true));
    museAccounts.set([acc]);
    eventLockedAccount.set({ id: "a1", label: "Account 1" });

    await withMockInvoke(
      async (cmd) => {
        if (cmd === "muse_sidecar_status") return makeStatus(true);
        if (cmd === "muse_list_accounts") {
          throw { code: "MUSE_SIDECAR_STOPPED", message: "Engine stopped" };
        }
        throw new Error(`unexpected cmd: ${cmd}`);
      },
      async () => {
        const res = await refreshMuseState({ force: true });
        assert.equal(get(museSidecarStatus)?.sidecar.running, false);
        assert.deepEqual(get(museAccounts), []);
        assert.equal(get(eventLockedAccount), null);
        assert.equal(get(museLoading), false);
        assert.match(res.error ?? "", /MUSE_SIDECAR_STOPPED/);
      }
    );

    await withMockInvoke(
      async (cmd) => {
        if (cmd === "muse_sidecar_status") {
          throw { code: "MUSE_SIDECAR_STOPPED", message: "Not running" };
        }
        throw new Error(`unexpected cmd: ${cmd}`);
      },
      async () => {
        clearEventLockedAccount();
        const res = await refreshMuseState({ force: true });
        assert.equal(res.status, null);
        assert.equal(get(museSidecarStatus), null);
        assert.deepEqual(get(museAccounts), []);
        assert.equal(get(museLoading), false);
      }
    );
  });
});
