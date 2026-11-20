// Store toàn cục: dự án đang mở, trạng thái chạy, sự kiện từ lõi & hệ thống Toast (13.2)
import { get, writable } from "svelte/store";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import {
  api,
  type ChecklistItem,
  type MuseAccount,
  type MuseStatusView,
  type ProjectInfo,
  type RunSummary,
} from "./api.js";

export const project = writable<ProjectInfo | null>(null);
export const running = writable(false);
export const currentRun = writable<(Partial<RunSummary> & { run_id?: number; total?: number }) | null>(
  null
);
export const progressText = writable("");
export const progressPercent = writable(0);
export const progressStep = writable<{ rowKey: string; step: string; detail: string } | null>(null);
export const activeRows = writable<Record<string, { step: string; detail: string }>>({});
export const selectedRows = writable<Set<string>>(new Set());
export const rowChangedTick = writable(0);
export const lastRunFinished = writable<any>(null);
export const pendingRowsFilter = writable<string | null>(null);
export const settingsDirty = writable(false);
export const endpointStatus = writable<{ imageConfigured: boolean; videoConfigured: boolean }>({
  imageConfigured: false,
  videoConfigured: false,
});
export const confirmStartEnabled = writable(true);
export const maxAttemptsStore = writable<number>(3);
export const assetCacheBust = writable<number>(Date.now());
export const rowAssetRevs = writable<Record<string, number>>({});
export const closeConfirmOpen = writable(false);

// ━━ VideoFlow Muse centralized state & singleton 5s poller (M4) ━━
export interface LockedAccountEvent {
  id: string;
  label?: string;
}

export interface MuseRefreshResult {
  status: MuseStatusView | null;
  accounts: MuseAccount[];
  error: string | null;
}

export const museSidecarStatus = writable<MuseStatusView | null>(null);
export const museAccounts = writable<MuseAccount[]>([]);
export const eventLockedAccount = writable<LockedAccountEvent | null>(null);
export const museLoading = writable<boolean>(false);

let museRefreshSeq = 0;
let museLockSeq = 0;
let museRefreshInFlight: Promise<MuseRefreshResult> | null = null;
let musePollRefs = 0;
let musePollTimer: ReturnType<typeof setInterval> | null = null;

export function clearEventLockedAccount(): void {
  museLockSeq += 1;
  eventLockedAccount.set(null);
}

function isSidecarStoppedError(err: unknown): boolean {
  if (err && typeof err === "object" && (err as { code?: unknown }).code === "MUSE_SIDECAR_STOPPED") {
    return true;
  }
  return formatApiError(err).includes("MUSE_SIDECAR_STOPPED");
}

export function refreshMuseState(opts?: { force?: boolean }): Promise<MuseRefreshResult> {
  if (!opts?.force && museRefreshInFlight) {
    return museRefreshInFlight;
  }

  const seq = ++museRefreshSeq;
  const lockSeqAtStart = museLockSeq;
  museLoading.set(true);

  let run!: Promise<MuseRefreshResult>;
  run = (async (): Promise<MuseRefreshResult> => {
    let needsFollowUp = false;
    try {
      const st = await api.muse_sidecar_status();
      if (seq !== museRefreshSeq) {
        return { status: st, accounts: get(museAccounts), error: null };
      }

      museSidecarStatus.set(st);

      if (!st.sidecar.running) {
        museAccounts.set([]);
        if (lockSeqAtStart === museLockSeq) {
          eventLockedAccount.set(null);
        } else {
          needsFollowUp = true;
        }
        return { status: st, accounts: [], error: null };
      }

      try {
        const accRes = await api.muse_list_accounts();
        if (seq !== museRefreshSeq) {
          return { status: st, accounts: accRes.data ?? [], error: null };
        }

        const accounts = accRes.data ?? [];
        museAccounts.set(accounts);

        if (lockSeqAtStart === museLockSeq) {
          eventLockedAccount.update((cur) => {
            if (!cur) return null;
            const matched = accounts.find((a) => a.id === cur.id);
            if (!matched || !matched.enabled || matched.status !== "video_locked") {
              return null;
            }
            return cur;
          });
        } else {
          needsFollowUp = true;
        }

        return { status: st, accounts, error: null };
      } catch (accErr) {
        if (seq === museRefreshSeq && isSidecarStoppedError(accErr)) {
          museSidecarStatus.set({
            ...st,
            sidecar: { ...st.sidecar, running: false },
          });
          museAccounts.set([]);
          if (lockSeqAtStart === museLockSeq) {
            eventLockedAccount.set(null);
          } else {
            needsFollowUp = true;
          }
          return { status: get(museSidecarStatus), accounts: [], error: formatApiError(accErr) };
        }
        return {
          status: st,
          accounts: get(museAccounts),
          error: formatApiError(accErr),
        };
      }
    } catch (err) {
      if (seq === museRefreshSeq && isSidecarStoppedError(err)) {
        museSidecarStatus.set(null);
        museAccounts.set([]);
        if (lockSeqAtStart === museLockSeq) {
          eventLockedAccount.set(null);
        } else {
          needsFollowUp = true;
        }
        return { status: null, accounts: [], error: formatApiError(err) };
      }
      return {
        status: get(museSidecarStatus),
        accounts: get(museAccounts),
        error: formatApiError(err),
      };
    } finally {
      if (seq === museRefreshSeq) {
        museLoading.set(false);
        if (museRefreshInFlight === run) {
          museRefreshInFlight = null;
        }
      }
      if (needsFollowUp && seq === museRefreshSeq) {
        void refreshMuseState({ force: true });
      }
    }
  })();

  museRefreshInFlight = run;
  return run;
}

export function startMusePolling(): () => void {
  musePollRefs += 1;
  if (musePollRefs === 1) {
    void refreshMuseState();
    musePollTimer = setInterval(() => {
      void refreshMuseState();
    }, 5000);
  }

  let stopped = false;
  return () => {
    if (stopped) return;
    stopped = true;
    musePollRefs = Math.max(0, musePollRefs - 1);
    if (musePollRefs === 0 && musePollTimer !== null) {
      clearInterval(musePollTimer);
      musePollTimer = null;
    }
  };
}

export function bumpRowAssetRev(rowKey?: string | null): void {
  const now = Date.now();
  if (rowKey && rowKey.trim()) {
    rowAssetRevs.update((m) => ({ ...m, [rowKey]: now }));
  } else {
    assetCacheBust.set(now);
  }
}

export interface RowsUiState {
  page: number;
  pageSize: number;
  chip: string;
  search: string;
  viewMode: "table" | "grid";
}

const ROWS_UI_STORAGE_KEY = "videoflow.rowsUi";

function loadPersistedRowsPrefs(): { pageSize: number; viewMode: "table" | "grid" } {
  try {
    const raw = typeof localStorage !== "undefined" ? localStorage.getItem(ROWS_UI_STORAGE_KEY) : null;
    if (raw) {
      const parsed = JSON.parse(raw);
      const ps = [10, 25, 50, 100].includes(Number(parsed?.pageSize))
        ? Number(parsed.pageSize)
        : 25;
      const vm = parsed?.viewMode === "grid" ? "grid" : "table";
      return { pageSize: ps, viewMode: vm };
    }
  } catch {
    // bỏ qua nếu không đọc được localStorage
  }
  return { pageSize: 25, viewMode: "table" };
}

const initialPrefs = loadPersistedRowsPrefs();

export const rowsUiState = writable<RowsUiState>({
  page: 1,
  pageSize: initialPrefs.pageSize,
  chip: "Tất cả",
  search: "",
  viewMode: initialPrefs.viewMode,
});

rowsUiState.subscribe((val) => {
  try {
    if (typeof localStorage !== "undefined") {
      localStorage.setItem(
        ROWS_UI_STORAGE_KEY,
        JSON.stringify({ pageSize: val.pageSize, viewMode: val.viewMode })
      );
    }
  } catch {
    // bỏ qua nếu không ghi được localStorage
  }
});

export function resetProjectRunState(): void {
  lastRunFinished.set(null);
  currentRun.set(null);
  progressText.set("");
  progressPercent.set(0);
  progressStep.set(null);
  activeRows.set({});
  selectedRows.set(new Set());
  assetCacheBust.set(Date.now());
  rowAssetRevs.set({});
  rowsUiState.update((st) => ({
    ...st,
    page: 1,
    chip: "Tất cả",
    search: "",
  }));
}

let projectSwitchSeq = 0;

/** Advances and returns the monotonic project-switch sequence counter. */
export function beginProjectSwitch(): number {
  projectSwitchSeq += 1;
  return projectSwitchSeq;
}

/** Returns the current monotonic project-switch sequence counter. */
export function getProjectSwitchSeq(): number {
  return projectSwitchSeq;
}

export type ToastType = "success" | "warn" | "error" | "info";

export interface ToastItem {
  id: number;
  type: ToastType;
  message: string;
}

interface ToastTimerMeta {
  timer: ReturnType<typeof setTimeout> | null;
  remainingMs: number;
  startedAt: number;
  paused: boolean;
}

export const toasts = writable<ToastItem[]>([]);
const toastTimers = new Map<number, ToastTimerMeta>();
let nextToastId = 1;

export function dismissToast(id: number): void {
  const meta = toastTimers.get(id);
  if (meta?.timer) clearTimeout(meta.timer);
  toastTimers.delete(id);
  toasts.update((list) => list.filter((t) => t.id !== id));
}

export function pauseToast(id: number): void {
  const meta = toastTimers.get(id);
  if (!meta || meta.paused) return;
  if (meta.timer) clearTimeout(meta.timer);
  const elapsed = Date.now() - meta.startedAt;
  meta.remainingMs = Math.max(800, meta.remainingMs - elapsed);
  meta.timer = null;
  meta.paused = true;
}

export function resumeToast(id: number): void {
  const meta = toastTimers.get(id);
  if (!meta || !meta.paused) return;
  meta.paused = false;
  meta.startedAt = Date.now();
  meta.timer = setTimeout(() => dismissToast(id), meta.remainingMs);
}

export function showToast(type: ToastType, message: string, durationMs?: number): number {
  const id = nextToastId++;
  const effectiveDuration = durationMs ?? (type === "error" ? 10000 : 4500);
  toasts.update((list) => {
    const dropped = list.length >= 5 ? list.slice(0, list.length - 4) : [];
    for (const d of dropped) {
      const m = toastTimers.get(d.id);
      if (m?.timer) clearTimeout(m.timer);
      toastTimers.delete(d.id);
    }
    return [...list.slice(-4), { id, type, message }];
  });
  if (effectiveDuration > 0) {
    const timer = setTimeout(() => dismissToast(id), effectiveDuration);
    toastTimers.set(id, {
      timer,
      remainingMs: effectiveDuration,
      startedAt: Date.now(),
      paused: false,
    });
  }
  return id;
}

export function formatApiError(err: any): string {
  if (!err) return "Đã xảy ra lỗi không xác định.";
  if (typeof err === "string") {
    const trimmed = err.trim();
    if (trimmed.startsWith("{") && trimmed.endsWith("}")) {
      try {
        const parsed = JSON.parse(trimmed);
        if (parsed && typeof parsed === "object" && (parsed.message || parsed.code)) {
          return parsed.code && parsed.message && !String(parsed.message).includes(parsed.code)
            ? `[${parsed.code}] ${parsed.message}`
            : String(parsed.message ?? parsed.code);
        }
      } catch {
        // giữ nguyên chuỗi gốc
      }
    }
    return trimmed;
  }
  if (typeof err === "object") {
    const code = typeof err.code === "string" ? err.code : "";
    const msg = typeof err.message === "string" ? err.message : "";
    if (code && msg) {
      return msg.includes(code) ? msg : `[${code}] ${msg}`;
    }
    if (msg) return msg;
    if (code) return code;
    try {
      return JSON.stringify(err);
    } catch {
      return String(err);
    }
  }
  return String(err);
}

export interface ProgressEventPayload {
  run_id: number;
  done: number;
  total: number;
  percent: number;
  current_row_key: string;
  current_step: string;
  step_detail: string;
  active_count?: number;
  active_rows?: { row_key: string; step: string; detail: string }[];
}

export interface RunStartedEventPayload {
  run_id: number;
  kind: string;
  total: number;
}

export interface RunFinishedEventPayload {
  run_id: number;
  status: string;
  done_count: number;
  error_count: number;
  duration_ms: number;
}

export interface RunErrorEventPayload {
  run_id: number | null;
  row_key: string | null;
  step: string | null;
  code: string;
  message: string;
}

export interface StructureChangedPayload {
  checklist: ChecklistItem[];
  ok: boolean;
}

export interface DesktopNotifyPayload {
  title?: string;
  body?: string;
}

let eventsInitialized = false;
const unlisteners: UnlistenFn[] = [];

async function safeListen<T>(eventName: string, handler: (payload: T) => void): Promise<void> {
  try {
    const unlisten = await listen<T>(eventName, (e) => handler(e.payload));
    unlisteners.push(unlisten);
  } catch (err) {
    console.warn(`[VideoFlow] Không đăng ký được sự kiện "${eventName}":`, err);
  }
}

export async function initEvents(): Promise<void> {
  if (eventsInitialized) return;
  eventsInitialized = true;

  await safeListen<ProgressEventPayload>("progress", (p) => {
    const pct = Math.max(0, Math.min(100, Number.isFinite(p.percent) ? p.percent : 0));
    progressPercent.set(pct);
    progressStep.set({
      rowKey: p.current_row_key ?? "",
      step: p.current_step ?? "",
      detail: p.step_detail ?? "",
    });

    const nextActive: Record<string, { step: string; detail: string }> = {};
    if (Array.isArray(p.active_rows) && p.active_rows.length > 0) {
      for (const item of p.active_rows) {
        if (item.row_key && item.step !== "done") {
          nextActive[item.row_key] = {
            step: item.step ?? "",
            detail: item.detail ?? "",
          };
        }
      }
    } else if (p.current_row_key && p.current_step !== "done") {
      nextActive[p.current_row_key] = {
        step: p.current_step ?? "",
        detail: p.step_detail ?? "",
      };
    }
    activeRows.set(nextActive);

    const activeCount = Object.keys(nextActive).length;
    const rowLabel =
      activeCount > 1
        ? `Đang xử lý ${activeCount} dòng song song · `
        : p.current_row_key
          ? `${p.current_row_key} · `
          : "";
    progressText.set(`${p.done}/${p.total} dòng (${Math.floor(pct)}%) — ${rowLabel}${p.step_detail}`);
  });

  await safeListen<RunStartedEventPayload>("run_started", (p) => {
    running.set(true);
    progressPercent.set(0);
    progressStep.set(null);
    activeRows.set({});
    progressText.set(`0/${p?.total ?? 0} dòng (0%) — Đang bắt đầu lượt #${p?.run_id ?? ""}...`);
    currentRun.set(p);
  });

  await safeListen<RunFinishedEventPayload>("run_finished", (p) => {
    running.set(false);
    if (p?.status === "hoan_tat") {
      progressPercent.set(100);
    }
    progressStep.set(null);
    activeRows.set({});
    lastRunFinished.set(p);
    rowChangedTick.update((n) => n + 1);

    const runTag = p?.run_id ? ` #${p.run_id}` : "";
    if (p?.status === "gian_doan") {
      showToast(
        "error",
        `Lượt${runTag} bị gián đoạn: ${p.done_count} xong, ${p.error_count} lỗi.`
      );
    } else if (p?.status === "da_dung") {
      showToast(
        "warn",
        `Đã dừng an toàn lượt${runTag}: ${p.done_count} xong, ${p.error_count} lỗi.`
      );
    } else if ((p?.error_count ?? 0) > 0) {
      showToast(
        "warn",
        `Lượt${runTag} kết thúc: ${p.done_count} dòng xong, ${p.error_count} dòng lỗi cần xem.`
      );
    } else {
      showToast(
        "success",
        `Hoàn tất lượt${runTag}: ${p?.done_count ?? 0} dòng thành công!`
      );
    }
  });

  await safeListen<RunErrorEventPayload>("run_error", (p) => {
    const prefix = p?.row_key ? `Dòng ${p.row_key}: ` : "";
    showToast("error", `${prefix}${formatApiError(p)}`);
    rowChangedTick.update((n) => n + 1);
    if (!p?.row_key) {
      api
        .is_running()
        .then((r) => {
          if (!r) {
            running.set(false);
            activeRows.set({});
            progressStep.set(null);
          }
        })
        .catch(() => {
          running.set(false);
          activeRows.set({});
          progressStep.set(null);
        });
    }
  });

  await safeListen<any>("row_changed", (p) => {
    const rk =
      typeof p?.row_key === "string" && p.row_key.trim()
        ? p.row_key
        : typeof p === "string" && p.trim()
          ? p
          : "";
    if (rk) {
      bumpRowAssetRev(rk);
    }
    rowChangedTick.update((n) => n + 1);
  });

  await safeListen<StructureChangedPayload>("structure_changed", (p) => {
    if (p?.checklist) {
      project.update((cur) => (cur ? { ...cur, checklist: p.checklist } : cur));
    }
    rowChangedTick.update((n) => n + 1);
  });

  await safeListen<any>("external_files_changed", () => {
    assetCacheBust.set(Date.now());
    rowChangedTick.update((n) => n + 1);
  });

  await safeListen<any>("schedule_missed", (p) => {
    const msg =
      typeof p === "string" && p.trim()
        ? p
        : typeof p?.reason === "string" && p.reason.trim()
          ? p.reason
          : typeof p?.message === "string" && p.message.trim()
            ? p.message
            : "Đến chu kỳ chạy tự động nhưng đang có lượt chạy khác — đã bỏ qua chu kỳ này.";
    showToast("warn", msg, 6000);
  });

  await safeListen<any>("close_requested_while_running", () => {
    closeConfirmOpen.set(true);
  });

  await safeListen<DesktopNotifyPayload>("desktop_notify", (p) => {
    const title = p?.title || "VideoFlow Desktop";
    const body = p?.body || "Lượt chạy đã hoàn tất.";
    try {
      if (typeof Notification !== "undefined") {
        if (Notification.permission === "granted") {
          new Notification(title, { body });
        } else if (Notification.permission === "default") {
          Notification.requestPermission()
            .then((perm) => {
              if (perm === "granted") new Notification(title, { body });
            })
            .catch(() => {});
        }
      }
    } catch {
      // bỏ qua nếu môi trường không hỗ trợ Web Notification
    }
  });

  await safeListen<{ account_id?: string; id?: string; label?: string }>(
    "account_video_locked",
    (p) => {
      const accId = p?.account_id || p?.id || "";
      if (accId) {
        museLockSeq += 1;
        eventLockedAccount.set({ id: accId, label: p?.label || accId });
      }
      void refreshMuseState({ force: true });
    }
  );
}
