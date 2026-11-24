import { derived, writable } from "svelte/store";

export type ThemeMode = "system" | "light" | "dark";
export type Locale = "vi" | "en";

const THEME_KEY = "vf_theme";
const LOCALE_KEY = "vf_lang";

export function normalizeThemeMode(val: unknown): ThemeMode {
  return val === "light" || val === "dark" || val === "system" ? val : "system";
}

export function normalizeLocale(val: unknown): Locale {
  return val === "en" || val === "vi" ? val : "vi";
}

function readStored<T extends string>(key: string, normalize: (v: unknown) => T): T {
  try {
    if (typeof localStorage !== "undefined") {
      return normalize(localStorage.getItem(key));
    }
  } catch {
    // ignore storage errors in restricted environments
  }
  return normalize(undefined);
}

function writeStored(key: string, val: string): void {
  try {
    if (typeof localStorage !== "undefined") {
      localStorage.setItem(key, val);
    }
  } catch {
    // ignore storage errors
  }
}

export const themeMode = writable<ThemeMode>(readStored(THEME_KEY, normalizeThemeMode));
export const locale = writable<Locale>(readStored(LOCALE_KEY, normalizeLocale));

let mediaCleanup: (() => void) | null = null;

/** Resolves effective `"light" | "dark"` given user `mode` and OS preference `systemPrefersDark`. */
export function resolveEffectiveTheme(mode: ThemeMode, systemPrefersDark: boolean): "light" | "dark" {
  if (mode === "light" || mode === "dark") return mode;
  return systemPrefersDark ? "dark" : "light";
}

/** Applies `mode` to `document.documentElement` and persists to `localStorage`. */
export function setThemeMode(mode: ThemeMode): void {
  const clean = normalizeThemeMode(mode);
  themeMode.set(clean);
  writeStored(THEME_KEY, clean);
  applyThemeToDocument(clean);
}

/** Cycles ThemeMode: `system -> light -> dark -> system`. */
export function cycleThemeMode(current: ThemeMode): ThemeMode {
  if (current === "system") return "light";
  if (current === "light") return "dark";
  return "system";
}

export function applyThemeToDocument(mode: ThemeMode): void {
  if (typeof document === "undefined") return;
  if (mediaCleanup) {
    mediaCleanup();
    mediaCleanup = null;
  }

  const root = document.documentElement;
  const applyResolved = (prefersDark: boolean) => {
    const eff = resolveEffectiveTheme(mode, prefersDark);
    root.dataset.theme = eff;
    root.dataset.themeMode = mode;
    root.style.colorScheme = eff;
  };

  if (typeof window !== "undefined" && typeof window.matchMedia === "function") {
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    applyResolved(mq.matches);
    if (mode === "system") {
      const handler = (ev: MediaQueryListEvent) => applyResolved(ev.matches);
      if (typeof mq.addEventListener === "function") {
        mq.addEventListener("change", handler);
        mediaCleanup = () => mq.removeEventListener("change", handler);
      }
    }
  } else {
    applyResolved(false);
  }
}

export function setLocale(next: Locale): void {
  const clean = normalizeLocale(next);
  locale.set(clean);
  writeStored(LOCALE_KEY, clean);
  if (typeof document !== "undefined") {
    document.documentElement.lang = clean;
  }
}

export function toggleLocale(current: Locale): Locale {
  return current === "vi" ? "en" : "vi";
}

export function initPreferences(): () => void {
  let currentTheme: ThemeMode = readStored(THEME_KEY, normalizeThemeMode);
  let currentLocale: Locale = readStored(LOCALE_KEY, normalizeLocale);
  themeMode.set(currentTheme);
  locale.set(currentLocale);
  applyThemeToDocument(currentTheme);
  if (typeof document !== "undefined") {
    document.documentElement.lang = currentLocale;
  }
  return () => {
    if (mediaCleanup) {
      mediaCleanup();
      mediaCleanup = null;
    }
  };
}

const MESSAGES: Record<Locale, Record<string, string>> = {
  vi: {
    "nav.dash": "Tổng quan",
    "nav.rows": "Dòng việc",
    "nav.accounts": "Tài khoản",
    "nav.proxies": "Proxy",
    "nav.history": "Lịch sử",
    "nav.folder": "Thư mục Local",
    "nav.settings": "Cài đặt",
    "title.dash": "Tổng quan vận hành",
    "title.rows": "Quản lý dòng việc",
    "title.accounts": "Quản lý tài khoản Muse",
    "title.proxies": "Quản lý Proxy (HTTP / SOCKS5)",
    "title.history": "Lịch sử & Nhật ký",
    "title.folder": "Thư mục dự án Local",
    "title.settings": "Cài đặt hệ thống",
    "header.chooseProject": "Chọn dự án...",
    "header.rowsCount": "{n} dòng",
    "header.runningSummary": "Đang chạy {n} dòng · {pct}%",
    "header.stopping": "Đang dừng...",
    "header.stop": "STOP",
    "header.settings": "Cài đặt",
    "header.theme.system": "Giao diện: Hệ thống",
    "header.theme.light": "Giao diện: Sáng",
    "header.theme.dark": "Giao diện: Tối",
    "header.lang": "Ngôn ngữ",
    "status.structOk": "Cấu trúc đủ (5/5)",
    "status.check": "Kiểm tra: {parts}",
    "status.noProject": "Chưa chọn dự án",
    "status.image": "Ảnh:",
    "status.video": "Video:",
    "common.save": "Lưu",
    "common.cancel": "Hủy",
    "common.close": "Đóng",
    "common.delete": "Xóa",
    "common.refresh": "Làm mới",
    "common.moreActions": "Thao tác khác",
    "settings.tab.image": "Nguồn ảnh",
    "settings.tab.video": "Nguồn video",
    "settings.tab.run": "Chạy & Thử lại",
    "settings.tab.schedule": "Lịch & Tự động",
    "settings.tab.system": "Hệ thống",
    "settings.appearance": "Giao diện & Ngôn ngữ",
    "settings.themeLabel": "Chế độ màu (Sáng / Tối)",
    "settings.theme.system": "Theo hệ thống",
    "settings.theme.light": "Sáng (Light)",
    "settings.theme.dark": "Tối (Dark)",
    "settings.langLabel": "Ngôn ngữ hiển thị",
    "settings.ffmpegTitle": "FFmpeg (cho Tự động ghép video & Nối cảnh)",
    "settings.ffmpegOk": "FFmpeg sẵn sàng",
    "settings.ffmpegMissing": "Chưa tìm thấy FFmpeg",
    "settings.ffmpegPathLabel": "Đường dẫn ffmpeg.exe (để trống = tự dò trong PATH)",
    "settings.ffmpegBrowse": "Chọn file...",
    "settings.ffmpegTest": "Kiểm tra FFmpeg",
    "settings.ffmpegTesting": "Đang kiểm tra...",
    "settings.ffmpegHint": "Cần FFmpeg khi bật Tự động ghép video hoặc Nối cảnh. Cài nhanh trên Windows: winget install Gyan.FFmpeg",
    "rows.searchPlaceholder": "Tìm theo tên ảnh, prompt, trạng thái...",
    "rows.runSelected": "Chạy {n} dòng đã chọn",
    "rows.scanInput": "Quét ảnh từ input/",
    "rows.viewTable": "Bảng",
    "rows.viewGrid": "Lưới",
    "rows.detail": "Chi tiết",
    "rows.runRow": "Chạy dòng này",
    "rows.openVideo": "Xem video",
    "rows.resetStatus": "Đặt lại trạng thái",
    "rows.skipRow": "Bỏ qua dòng này",
    "rows.remapImage": "Đổi ảnh tham chiếu",
    "rows.deleteRow": "Xóa dòng khỏi CSV",
  },
  en: {
    "nav.dash": "Overview",
    "nav.rows": "Rows",
    "nav.accounts": "Accounts",
    "nav.proxies": "Proxies",
    "nav.history": "History",
    "nav.folder": "Local Folder",
    "nav.settings": "Settings",
    "title.dash": "Operations Overview",
    "title.rows": "Prompt Rows Manager",
    "title.accounts": "Muse Accounts Pool",
    "title.proxies": "Proxy Manager (HTTP / SOCKS5)",
    "title.history": "Run History & Logs",
    "title.folder": "Local Project Folder",
    "title.settings": "System Settings",
    "header.chooseProject": "Open project...",
    "header.rowsCount": "{n} rows",
    "header.runningSummary": "Running {n} rows · {pct}%",
    "header.stopping": "Stopping...",
    "header.stop": "STOP",
    "header.settings": "Settings",
    "header.theme.system": "Theme: System",
    "header.theme.light": "Theme: Light",
    "header.theme.dark": "Theme: Dark",
    "header.lang": "Language",
    "status.structOk": "Structure OK (5/5)",
    "status.check": "Check: {parts}",
    "status.noProject": "No project open",
    "status.image": "Image:",
    "status.video": "Video:",
    "common.save": "Save",
    "common.cancel": "Cancel",
    "common.close": "Close",
    "common.delete": "Delete",
    "common.refresh": "Refresh",
    "common.moreActions": "More actions",
    "settings.tab.image": "Image Source",
    "settings.tab.video": "Video Source",
    "settings.tab.run": "Run & Retry",
    "settings.tab.schedule": "Schedule & Auto",
    "settings.tab.system": "System",
    "settings.appearance": "Appearance & Language",
    "settings.themeLabel": "Color theme",
    "settings.theme.system": "Follow system",
    "settings.theme.light": "Light",
    "settings.theme.dark": "Dark",
    "settings.langLabel": "Display language",
    "settings.ffmpegTitle": "FFmpeg (required for Auto-merge & Scene chaining)",
    "settings.ffmpegOk": "FFmpeg ready",
    "settings.ffmpegMissing": "FFmpeg not found",
    "settings.ffmpegPathLabel": "Custom ffmpeg.exe path (leave blank to auto-detect from PATH)",
    "settings.ffmpegBrowse": "Browse...",
    "settings.ffmpegTest": "Check FFmpeg",
    "settings.ffmpegTesting": "Checking...",
    "settings.ffmpegHint": "Required for Auto-merge or noi_canh scene chaining. Quick install on Windows: winget install Gyan.FFmpeg",
    "rows.searchPlaceholder": "Search by image, prompt, status...",
    "rows.runSelected": "Run {n} selected",
    "rows.scanInput": "Scan input/",
    "rows.viewTable": "Table",
    "rows.viewGrid": "Grid",
    "rows.detail": "Details",
    "rows.runRow": "Run row",
    "rows.openVideo": "Watch video",
    "rows.resetStatus": "Reset to pending",
    "rows.skipRow": "Skip row",
    "rows.remapImage": "Remap reference image",
    "rows.deleteRow": "Delete row from CSV",
  },
};

const STATUS_EN: Record<string, string> = {
  "Tất cả": "All",
  "Đang chờ": "Pending",
  "Đang tạo image": "Generating image",
  "Đã tạo image": "Image ready",
  "Đang tạo video": "Generating video",
  "Đã tạo video": "Video ready",
  "Lỗi": "All errors",
  "Tất cả lỗi": "All errors",
  "Lỗi tạo image": "Image error",
  "Lỗi tạo video": "Video error",
  "Lỗi tạm thời": "Transient error",
  "Bỏ qua": "Skipped",
  "Đã sửa prompt (chạy lại)": "Prompt edited (rerun)",
};

export function translate(
  lang: Locale,
  key: string,
  params?: Record<string, string | number>
): string {
  const dict = MESSAGES[lang] ?? MESSAGES.vi;
  let str = dict[key] ?? MESSAGES.vi[key] ?? key;
  if (params) {
    for (const [k, v] of Object.entries(params)) {
      str = str.replaceAll(`{${k}}`, String(v));
    }
  }
  return str;
}

export function translateStatus(status: string, lang: Locale): string {
  if (lang === "en") {
    return STATUS_EN[status] ?? status;
  }
  return status;
}

/** Reactive translator store: `$t("nav.dash")` or `$t("header.rowsCount", { n: 12 })`. */
export const t = derived(locale, ($locale) => {
  return (key: string, params?: Record<string, string | number>) =>
    translate($locale, key, params);
});
