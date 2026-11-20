// Lớp gọi lõi theo hợp đồng 13.2: UI không gọi API ngoài, không đọc file trực tiếp.
import { convertFileSrc, invoke } from "@tauri-apps/api/core";

/** Chuẩn hóa lỗi trả về từ Tauri (`{ code, message }`) hoặc JS `Error` thành chuỗi đọc được. */
export function formatError(e: unknown): string {
  if (typeof e === "string") return e;
  if (e instanceof Error) return e.message;
  if (e && typeof e === "object") {
    const obj = e as Record<string, unknown>;
    if (typeof obj.message === "string" && obj.message.trim()) return obj.message;
    if (typeof obj.error === "string" && obj.error.trim()) return obj.error;
    try {
      return JSON.stringify(e);
    } catch {
      return String(e);
    }
  }
  return String(e);
}

export function toAssetUrl(
  absPath: string | null | undefined,
  cacheKey?: string | number | null
): string {
  if (!absPath || !absPath.trim()) return "";
  const normalized = absPath
    .trim()
    .replace(/^\\\\\?\\UNC\\/i, "\\\\")
    .replace(/^\\\\\?\\/, "");
  const url = convertFileSrc(normalized);
  if (cacheKey !== undefined && cacheKey !== null && cacheKey !== "") {
    return `${url}${url.includes("?") ? "&" : "?"}v=${encodeURIComponent(String(cacheKey))}`;
  }
  return url;
}

export interface ChecklistItem {
  key: string;
  ok: boolean;
  message: string;
}

export interface ProjectInfo {
  path: string;
  name: string;
  checklist: ChecklistItem[];
  row_count: number;
}

export interface ProbeResult {
  is_empty_dir: boolean;
  has_full_structure: boolean;
  has_partial: boolean;
  prompts_valid: boolean;
  prompts_error?: string | null;
  row_count?: number;
  checklist: ChecklistItem[];
}

export interface RecentProject {
  path: string;
  name: string;
  row_count?: number;
  exists: boolean;
  is_current: boolean;
}

export interface UntrackedImageItem {
  file_name: string;
  abs_path: string;
  size_bytes: number;
}

export interface ActiveRowProgress {
  row_key: string;
  step: string;
  detail: string;
}

export interface ProgressEvent {
  run_id: number;
  done: number;
  total: number;
  percent: number;
  current_row_key: string;
  current_step: string;
  step_detail: string;
  active_count?: number;
  active_rows?: ActiveRowProgress[];
}

export interface RowSummary {
  /** Khóa dòng = `tên ảnh#hash(prompt ảnh + prompt video)` — dùng cho mọi command. */
  row_key: string;
  /** Tên file ảnh tham chiếu (cột A) — hiển thị cho người dùng. */
  image_name: string;
  status: string;
  image_file: string;
  video_file: string;
  attempts_count: number;
  last_error: string | null;
  row_no: number;
  prompt_modified?: boolean;
  ref_exists?: boolean;
  noi_canh?: boolean;
}

export interface RowPage {
  rows: RowSummary[];
  total: number;
  page: number;
  page_count: number;
  status_counts: Record<string, number>;
}

export interface FileVersionItem {
  kind: string;
  file_name: string;
  file_path: string;
  abs_path: string;
  version: number;
  is_current: boolean;
  size_bytes?: number | null;
  created_at?: string | null;
}

export interface TimelineItem {
  ts: string;
  step: string;
  result: string;
  error: string | null;
  run_id?: number | null;
  attempt_no?: number | null;
  http_status?: number | null;
  duration_ms?: number | null;
}

export interface RowDetail {
  row: RowSummary;
  prompt_image: string;
  prompt_video: string;
  ref_exists: boolean;
  ref_abs_path: string;
  image_abs_path: string | null;
  video_abs_path: string | null;
  files: FileVersionItem[];
  timeline: TimelineItem[];
  video_duration?: number | null;
  video_resolution?: string | null;
  account_used?: string | null;
}

export interface DryRunResult {
  groups: { status: string; count: number; row_keys: string[] }[];
  total: number;
  is_csv_locked?: boolean;
}

export interface RunSummary {
  id: number;
  kind: string;
  status: string;
  started_at: string;
  finished_at: string | null;
  total_rows: number;
  done_count: number;
  error_count: number;
  duration_ms?: number | null;
}

export interface HistoryFilter {
  status?: string;
  kind?: string;
  from?: string;
  to?: string;
}

export interface HistoryPage {
  runs: RunSummary[];
  total: number;
  page: number;
  page_count: number;
  stats_7d?: {
    runs_count: number;
    done_rows: number;
    error_rows: number;
  };
}

export interface LogItem {
  id: number;
  ts: string;
  level: string;
  run_id: number | null;
  row_key: string | null;
  step: string | null;
  message: string;
  detail?: string | null;
}

export interface Settings {
  endpoint_image_base_url: string;
  endpoint_image_model: string;
  /** Nguồn sinh ảnh: "gateway" (mặc định) | "internal" (VideoFlow Muse, model muse-image) */
  image_source: string;
  endpoint_video_base_url: string;
  endpoint_video_model: string;
  /** Nguồn sinh video: "gateway" (mặc định) | "internal" (VideoFlow Muse nội bộ) */
  video_source: string;
  /** Đường dẫn trình duyệt (Edge/Chrome) sidecar điều khiển; rỗng = tự dò */
  muse_chromium_path: string;
  /** Sidecar chạy Chromium headless (mặc định true) */
  muse_headless: boolean;
  /** Port engine VideoFlow Muse ưa thích (18610) */
  muse_sidecar_port: number;
  video_size: string;
  video_seconds: number;
  image_timeout_s: number;
  video_create_timeout_s: number;
  poll_interval_s: number;
  poll_max_s: number;
  backoff_s: [number, number, number];
  max_attempts: number;
  concurrency: number;
  schedule_enabled: boolean;
  schedule_hours: number;
  auto_cleanup_days: number;
  /** Tự động ghép nối các clip video sau khi kết thúc batch (P2-2) */
  auto_merge: boolean;
  confirm_start: boolean;
  notify_on_finish: boolean;
  minimize_to_tray: boolean;
  start_with_windows: boolean;
  recent_projects: string[];
  current_project: string | null;
}

export interface SettingsView {
  settings: Settings;
  image_key_preview: string | null;
  video_key_preview: string | null;
  has_image_key: boolean;
  has_video_key: boolean;
}

// ━━ VideoFlow Muse nội bộ (engine + pool cookie + hạn mức) ━━

export interface SidecarStatus {
  running: boolean;
  port: number | null;
  version: string | null;
  driver: string | null;
  account_total: number | null;
  account_available: number | null;
  /** Cấu hình lúc engine spawn — so với Settings để biết có cần khởi động lại. */
  cfg_port: number | null;
  cfg_chromium: string | null;
  cfg_headless: boolean | null;
  restart_required: boolean;
}

export interface MuseStatusView {
  sidecar: SidecarStatus;
  video_source: string;
  chromium_path: string;
  headless: boolean;
}

export interface MuseAccount {
  id: string;
  label: string;
  enabled: boolean;
  status: "active" | "cooling" | "invalid" | "video_locked";
  proxy?: string | null;
  cookies: Record<string, string>;
  cookie_expires: Record<string, number>;
  created_at: number;
  last_used_at: number;
  cooldown_until: number;
  last_error: string | null;
  success_count: number;
  fail_count: number;
  meta: {
    quota?: MuseQuotaSnapshot;
    [key: string]: unknown;
  };
}

/** Snapshot hạn mức đọc từ trang Settings → Usage của muse.ai */
export interface MuseQuotaSnapshot {
  /** Gói (vd "free", "pro plan") */
  plan: string | null;
  /** % quota hàng tuần đã dùng (0-100) — trường chính của bar hạn mức */
  weekly_used_pct: number | null;
  /** Ngày reset quota hàng tuần (chuỗi ngày trên trang muse.ai) */
  resets_at: string | null;
  /** Quota bổ sung còn lại (dạng text, vd "1,200 tokens") */
  extra_left: string | null;
  /** true chỉ khi đọc được % hàng tuần */
  found: boolean;
  /** lỗi khi đọc (settings entry not found...) nếu có */
  error?: string;
  /** dạng snapshot đếm (driver mock/format khác) — fallback tính % */
  messages_used?: number;
  messages_limit?: number;
  messages_remaining?: number;
  media_used?: number;
  media_limit?: number;
  media_remaining?: number;
  /** unix ts lần đọc gần nhất */
  checked_at?: number;
  fetched_at?: number;
  source?: string;
}

export const api = {
  list_recent_projects: () => invoke<RecentProject[]>("list_recent_projects"),
  remove_recent_project: (path: string) => invoke<boolean>("remove_recent_project", { path }),
  load_project: (path?: string) => invoke<ProjectInfo>("load_project", { path }),
  probe_folder: (path: string) => invoke<ProbeResult>("probe_folder", { path }),
  check_structure: () => invoke<ChecklistItem[]>("check_structure"),
  pick_folder: () => invoke<string | null>("pick_folder"),
  create_project: (path: string, mode: string) =>
    invoke<ProjectInfo>("create_project", { path, mode }),
  get_rows: (status?: string, query?: string, page = 1, pageSize = 25) =>
    invoke<RowPage>("get_rows", { status, query, page, pageSize }),
  get_row_detail: (rowKey: string) => invoke<RowDetail>("get_row_detail", { rowKey }),
  scan_untracked_input_images: () =>
    invoke<UntrackedImageItem[]>("scan_untracked_input_images"),
  import_input_images: (
    fileNames: string[],
    defaultPromptImage: string,
    defaultPromptVideo: string
  ) =>
    invoke<number>("import_input_images", {
      fileNames,
      defaultPromptImage,
      defaultPromptVideo,
    }),
  set_current_file_version: (rowKey: string, kind: "image" | "video", version: number) =>
    invoke<RowDetail>("set_current_file_version", { rowKey, kind, version }),
  start_dry_run: () => invoke<DryRunResult>("start_dry_run"),
  start_run: (scope: string, rowKeys?: string[]) =>
    invoke<number>("start_run", { scope, rowKeys }),
  run_row: (rowKey: string) => invoke<number>("run_row", { rowKey }),
  rerun_row_step: (rowKey: string, step: "anh" | "video") =>
    invoke<number>("rerun_row_step", { rowKey, step }),
  stop_run: () => invoke<boolean>("stop_run"),
  is_running: () => invoke<boolean>("is_running"),
  engine_done: () => invoke<void>("engine_done"),
  rerun_error_rows: (runId: number) => invoke<number>("rerun_error_rows", { runId }),
  reset_row_status: (rowKey: string) => invoke<RowDetail>("reset_row_status", { rowKey }),
  remap_reference_image: (rowKey: string, newFileName: string) =>
    invoke<RowDetail>("remap_reference_image", { rowKey, newFileName }),
  pick_image_in_input: () => invoke<string | null>("pick_image_in_input"),
  delete_row: (rowKey: string) => invoke<boolean>("delete_row", { rowKey }),
  skip_row: (rowKey: string) => invoke<RowDetail>("skip_row", { rowKey }),
  save_row_prompts: (rowKey: string, promptImage: string, promptVideo: string) =>
    invoke<RowDetail>("save_row_prompts", { rowKey, promptImage, promptVideo }),
  list_models: (which: string, baseUrl?: string, apiKey?: string) =>
    invoke<string[]>("list_models", { which, baseUrl, apiKey }),
  get_settings: () => invoke<SettingsView>("get_settings"),
  save_settings: (settings: Settings) => invoke<boolean>("save_settings", { settings }),
  set_api_key: (which: string, value: string) =>
    invoke<boolean>("set_api_key_cmd", { which, value }),
  test_connection: (which: string, baseUrl?: string, apiKey?: string) =>
    invoke<[boolean, number, string]>("test_connection", { which, baseUrl, apiKey }),
  get_history: (
    page = 1,
    pageSize = 25,
    status?: string,
    kind?: string,
    from?: string,
    to?: string
  ) => invoke<HistoryPage>("get_history", { page, pageSize, status, kind, from, to }),
  get_run_detail: (runId: number) => invoke<[RunSummary, RowSummary[]]>("get_run_detail", { runId }),
  pick_save_csv: () => invoke<string | null>("pick_save_csv"),
  export_history_csv: (savePath: string, filter?: HistoryFilter | string | null) =>
    invoke<boolean>("export_history_csv", {
      filter: typeof filter === "string" ? { status: filter } : (filter ?? null),
      savePath,
    }),
  get_logs: (runId?: number | null, level?: string | null, limit = 200) =>
    invoke<LogItem[]>("get_logs", { runId: runId ?? null, level: level ?? null, limit }),
  pick_save_log: () => invoke<string | null>("pick_save_log"),
  export_logs_txt: (savePath: string, runId?: number | null, level?: string | null) =>
    invoke<boolean>("export_logs_txt", {
      runId: runId ?? null,
      level: level ?? null,
      savePath,
    }),
  open_path_in_explorer: (targetPath: string, reveal = false) =>
    invoke<boolean>("open_path_in_explorer", { targetPath, reveal }),
  force_quit_app: (stopFirst: boolean) => invoke<void>("force_quit_app", { stopFirst }),
  hide_to_tray: () => invoke<void>("hide_to_tray"),
  // ━━ VideoFlow Muse nội bộ ━━
  muse_sidecar_status: () => invoke<MuseStatusView>("muse_sidecar_status"),
  muse_start_sidecar: () => invoke<SidecarStatus>("muse_start_sidecar"),
  muse_stop_sidecar: () => invoke<boolean>("muse_stop_sidecar"),
  muse_list_accounts: () => invoke<{ data: MuseAccount[] }>("muse_list_accounts"),
  muse_add_cookie: (label: string, cookies: unknown, allowDuplicate = false) =>
    invoke<{ account: MuseAccount; missing_cookies: string[] }>("muse_add_cookie", {
      label,
      cookies,
      allowDuplicate,
    }),
  muse_update_account: (accountId: string, enabled?: boolean, label?: string, proxy?: string | null) =>
    invoke<{ account: MuseAccount }>("muse_update_account", {
      accountId,
      enabled: enabled ?? null,
      label: label ?? null,
      proxy: proxy ?? null,
    }),
  muse_open_login: (accountId: string) =>
    invoke<{ ok: boolean; message?: string }>("muse_open_login", { accountId }),
  muse_open_view: (accountId: string) =>
    invoke<{ ok: boolean; message?: string }>("muse_open_view", { accountId }),
  muse_delete_account: (accountId: string) =>
    invoke<{ ok: boolean }>("muse_delete_account", { accountId }),
  muse_renew_account: (accountId: string) =>
    invoke<{ account: MuseAccount }>("muse_renew_account", { accountId }),
  muse_account_quota: (accountId: string, refresh = false) =>
    invoke<{ account_id: string; quota: MuseQuotaSnapshot }>("muse_account_quota", {
      accountId,
      refresh,
    }),
  muse_download_extension: () => invoke<string>("muse_download_extension"),
  // ━━ Cập nhật ứng dụng ━━
  app_version: () => invoke<string>("app_version"),
  check_app_update: () => invoke<UpdateInfo>("check_app_update"),
  install_app_update: () => invoke<boolean>("install_app_update"),
};

export interface UpdateInfo {
  available: boolean;
  current_version: string;
  version: string | null;
  notes: string | null;
  date: string | null;
}

export interface UpdateProgress {
  downloaded: number;
  total: number | null;
  percent: number | null;
}
