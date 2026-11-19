//! Cấu hình app theo bảng settings (13.1) - giá trị dạng JSON nguyên tử, khóa API nằm trong
//! kho khóa hệ điều hành (mục 9), không nằm ở đây.

use crate::error::{AppError, AppResult};
use parking_lot::RwLock;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

pub const KEYRING_SERVICE: &str = "VideoFlow Desktop";
pub const KEY_IMAGE: &str = "endpoint_image_key";
pub const KEY_VIDEO: &str = "endpoint_video_key";

/// Bộ nhớ đệm các khóa bí mật thực tế (đã lưu/nạp trong phiên) để `redact_secrets` lọc sạch cả
/// các khóa API tùy chỉnh không mang tiền tố `sk-` hoặc `Bearer ` mà không cần gọi OS Keyring mỗi dòng log.
static KNOWN_SECRETS: OnceLock<RwLock<Vec<String>>> = OnceLock::new();

fn known_secrets() -> &'static RwLock<Vec<String>> {
    KNOWN_SECRETS.get_or_init(|| RwLock::new(Vec::new()))
}

pub fn register_secret_for_redaction(secret: &str) {
    let trimmed = secret.trim();
    if trimmed.chars().count() < 4 {
        return;
    }
    let mut guard = known_secrets().write();
    if !guard.iter().any(|s| s == trimmed) {
        guard.push(trimmed.to_string());
        // Sắp xếp khóa dài trước để luôn che trọn vẹn khi một khóa là tiền tố của khóa khác
        guard.sort_by_key(|b| std::cmp::Reverse(b.len()));
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub endpoint_image_base_url: String,
    pub endpoint_image_model: String,
    /// Nguồn sinh ảnh: "gateway" (gateway ngoài, mặc định) | "internal" (VideoFlow Muse).
    pub image_source: String,
    pub endpoint_video_base_url: String,
    pub endpoint_video_model: String,
    /// Nguồn sinh video: "gateway" (gateway ngoài, mặc định) | "internal" (muse2api nội bộ).
    pub video_source: String,
    /// Đường dẫn trình duyệt (Edge/Chrome) sidecar muse2api điều khiển; rỗng = tự dò.
    pub muse_chromium_path: String,
    /// Sidecar chạy Chromium headless (mặc định true).
    pub muse_headless: bool,
    /// Port sidecar muse2api ưa thích (18610; bận thì tự xin port khác).
    pub muse_sidecar_port: u16,
    pub video_size: String,
    pub video_seconds: u32,
    pub image_timeout_s: u64,        // mặc định 300 giây (6.6 Thẻ 2)
    pub video_create_timeout_s: u64, // mặc định 120 giây (5.6)
    pub poll_interval_s: u64,        // mặc định 15 giây
    pub poll_max_s: u64,             // mặc định 600 giây (Phụ lục quyết định 2)
    pub backoff_s: [u64; 3],         // mặc định 30s / 2 phút / 5 phút (Phụ lục quyết định 1)
    pub max_attempts: u32,           // mặc định 3 (13.5)
    pub concurrency: u32,            // 1-3 dòng chạy song song
    pub schedule_enabled: bool,      // tự chạy theo lịch (6.6 Thẻ 4)
    pub schedule_hours: u64,         // chu kỳ N giờ
    pub auto_cleanup_days: u32,      // 30
    pub auto_merge: bool,            // tự động ghép nối video sau batch (P2-2)
    pub confirm_start: bool,         // hộp xác nhận Start "Không hỏi lại"
    pub notify_on_finish: bool,
    pub minimize_to_tray: bool,
    pub start_with_windows: bool,
    pub recent_projects: Vec<String>, // Phụ lục quyết định 4: tối đa 5
    pub current_project: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            endpoint_image_base_url: "https://gpt.photowrapped.com/v1".into(),
            endpoint_image_model: "gpt-image-2".into(),
            image_source: "gateway".into(),
            endpoint_video_base_url: "https://muse.photowrapped.com/v1".into(),
            endpoint_video_model: "muse-video".into(),
            video_source: "gateway".into(),
            muse_chromium_path: String::new(),
            muse_headless: true,
            muse_sidecar_port: 18610,
            video_size: "720x1280".into(),
            video_seconds: 8,
            image_timeout_s: 300,
            video_create_timeout_s: 120,
            poll_interval_s: 15,
            poll_max_s: 600,
            backoff_s: [30, 120, 300],
            max_attempts: 3,
            concurrency: 1,
            schedule_enabled: false,
            schedule_hours: 3,
            auto_cleanup_days: 30,
            auto_merge: false,
            confirm_start: true,
            notify_on_finish: true,
            minimize_to_tray: false,
            start_with_windows: false,
            recent_projects: Vec::new(),
            current_project: None,
        }
    }
}

fn trim_or_default(val: &str, fallback: &str) -> String {
    let t = val.trim();
    if t.is_empty() {
        fallback.to_string()
    } else {
        t.to_string()
    }
}

impl Settings {
    /// Chuẩn hóa chuỗi URL/model/size (`.trim()`, fallback về mặc định nếu rỗng)
    /// và giới hạn biên an toàn cho toàn bộ tham số số học.
    pub fn sanitized(&self) -> Self {
        let def = Self::default();
        let mut clean = self.clone();

        clean.endpoint_image_base_url = trim_or_default(
            clean.endpoint_image_base_url.trim().trim_end_matches('/'),
            &def.endpoint_image_base_url,
        );
        clean.endpoint_image_model =
            trim_or_default(&clean.endpoint_image_model, &def.endpoint_image_model);
        clean.endpoint_video_base_url = trim_or_default(
            clean.endpoint_video_base_url.trim().trim_end_matches('/'),
            &def.endpoint_video_base_url,
        );
        clean.endpoint_video_model =
            trim_or_default(&clean.endpoint_video_model, &def.endpoint_video_model);
        // Nguồn ảnh/video chỉ nhận 2 giá trị hợp lệ; sai định dạng về gateway ngoài.
        if clean.image_source != "internal" {
            clean.image_source = "gateway".into();
        }
        if clean.video_source != "internal" {
            clean.video_source = "gateway".into();
        }
        clean.muse_chromium_path = clean.muse_chromium_path.trim().to_string();
        clean.muse_sidecar_port = clean.muse_sidecar_port.max(1);
        clean.video_size = trim_or_default(&clean.video_size, &def.video_size);

        clean.concurrency = clean.concurrency.clamp(1, 3);
        clean.max_attempts = clean.max_attempts.clamp(1, 10);
        clean.video_seconds = clean.video_seconds.max(1);
        clean.image_timeout_s = clean.image_timeout_s.max(1);
        clean.video_create_timeout_s = clean.video_create_timeout_s.max(1);
        clean.poll_interval_s = clean.poll_interval_s.max(1);
        clean.poll_max_s = clean.poll_max_s.max(1);
        clean.backoff_s = [
            clean.backoff_s[0].max(1),
            clean.backoff_s[1].max(1),
            clean.backoff_s[2].max(1),
        ];
        clean.schedule_hours = clean.schedule_hours.max(1);
        clean.auto_cleanup_days = clean.auto_cleanup_days.max(1);

        clean.recent_projects = clean
            .recent_projects
            .into_iter()
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .take(5)
            .collect();
        clean.current_project = clean
            .current_project
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty());

        clean
    }
}

/// Vị trí dữ liệu app (không phải dự án): %APPDATA%/VideoFlowDesktop
pub fn app_data_dir() -> PathBuf {
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs_fallback());
    base.join("VideoFlowDesktop")
}

fn dirs_fallback() -> PathBuf {
    std::env::var("USERPROFILE")
        .map(|h| PathBuf::from(h).join("AppData").join("Roaming"))
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// Settings toàn cục nằm trong thư mục dữ liệu app (file JSON riêng), không nằm
/// trong state.db của dự án - để mở app trước khi chọn dự án vẫn đọc được cấu hình.
#[derive(Debug, Clone)]
pub struct SettingsStore {
    path: PathBuf,
}

impl Default for SettingsStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SettingsStore {
    pub fn new() -> Self {
        let dir = app_data_dir();
        std::fs::create_dir_all(&dir).ok();
        Self {
            path: dir.join("settings.json"),
        }
    }

    pub fn with_path(path: PathBuf) -> Self {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        Self { path }
    }

    pub fn load(&self) -> Settings {
        std::fs::read(&self.path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Settings>(&b).ok())
            .unwrap_or_default()
            .sanitized()
    }

    /// Ghi nguyên tử qua file tạm `.tmp` rồi `fs::rename` để không bao giờ mất cấu hình khi sập nguồn.
    pub fn save(&self, s: &Settings) -> AppResult<()> {
        let clean = s.sanitized();
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let data = serde_json::to_vec_pretty(&clean)
            .map_err(|e| AppError::new("SETTINGS_SERIALIZE", format!("{e}")))?;

        let tmp = self.path.with_extension("json.videoflow_tmp");
        {
            let mut f = std::fs::File::create(&tmp).map_err(|e| {
                AppError::new(
                    "SETTINGS_WRITE_FAIL",
                    format!("Không tạo được file tạm settings.json: {e}"),
                )
            })?;
            if let Err(e) = f.write_all(&data) {
                drop(f);
                let _ = std::fs::remove_file(&tmp);
                return Err(AppError::new(
                    "SETTINGS_WRITE_FAIL",
                    format!("Không ghi được file tạm settings.json: {e}"),
                ));
            }
            let _ = f.sync_all();
        }

        if let Err(e) = std::fs::rename(&tmp, &self.path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(AppError::new(
                "SETTINGS_WRITE_FAIL",
                format!("Không cập nhật được settings.json: {e}"),
            ));
        }
        Ok(())
    }
}

/// Quản lý khởi động cùng Windows qua `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` `"VideoFlowDesktop"`.
/// Trong môi trường `#[cfg(test)]`, hàm là no-op để không bao giờ chạm Registry thật của máy chạy test.
pub fn apply_windows_autostart(enabled: bool) -> Result<(), String> {
    #[cfg(test)]
    {
        let _ = enabled;
        Ok(())
    }

    #[cfg(not(test))]
    {
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
            const VALUE_NAME: &str = "VideoFlowDesktop";

            let sys_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
            let reg_exe = PathBuf::from(sys_root).join("System32").join("reg.exe");

            if enabled {
                let exe = std::env::current_exe()
                    .map_err(|e| format!("Không xác định được đường dẫn ứng dụng: {e}"))?;
                let raw_exe = exe.to_string_lossy();
                let clean_exe = raw_exe.strip_prefix(r"\\?\").unwrap_or(&raw_exe);
                let quoted = format!("\"{clean_exe}\"");

                let out = std::process::Command::new(&reg_exe)
                    .creation_flags(CREATE_NO_WINDOW)
                    .args([
                        "add",
                        RUN_KEY,
                        "/v",
                        VALUE_NAME,
                        "/t",
                        "REG_SZ",
                        "/d",
                        &quoted,
                        "/f",
                    ])
                    .output()
                    .map_err(|e| format!("Không gọi được reg.exe để bật khởi động cùng Windows: {e}"))?;

                if !out.status.success() {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    return Err(format!("Đăng ký khởi động cùng Windows thất bại: {}", stderr.trim()));
                }
            } else {
                // Kiểm tra khóa tồn tại trước để thao tác tắt luôn idempotent
                let exists = std::process::Command::new(&reg_exe)
                    .creation_flags(CREATE_NO_WINDOW)
                    .args(["query", RUN_KEY, "/v", VALUE_NAME])
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false);

                if exists {
                    let out = std::process::Command::new(&reg_exe)
                        .creation_flags(CREATE_NO_WINDOW)
                        .args(["delete", RUN_KEY, "/v", VALUE_NAME, "/f"])
                        .output()
                        .map_err(|e| format!("Không gọi được reg.exe để gỡ khởi động cùng Windows: {e}"))?;
                    if !out.status.success() {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        return Err(format!("Gỡ khởi động cùng Windows thất bại: {}", stderr.trim()));
                    }
                }
            }
            Ok(())
        }

        #[cfg(not(target_os = "windows"))]
        {
            let _ = enabled;
            Ok(())
        }
    }
}

/// API key: lưu/xóa qua Windows Credential Manager (mục 9).
/// Khi `value.trim().is_empty()`, xóa credential đã lưu (bỏ qua lỗi `NoEntry`).
pub fn set_api_key(which: &str, value: &str) -> AppResult<()> {
    let key_name = match which {
        "anh" => KEY_IMAGE,
        "video" => KEY_VIDEO,
        _ => return Err(AppError::new("BAD_KEY_NAME", "Tên khóa không hợp lệ")),
    };
    let entry = keyring::Entry::new(KEYRING_SERVICE, key_name)
        .map_err(|e| AppError::new("KEYRING_FAIL", format!("{e}")))?;

    let trimmed = value.trim();
    if trimmed.is_empty() {
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(AppError::new(
                "KEYRING_FAIL",
                format!("Không xóa được khóa khỏi Credential Manager: {e}"),
            )),
        }
    } else {
        register_secret_for_redaction(trimmed);
        entry
            .set_password(trimmed)
            .map_err(|e| AppError::new("KEYRING_FAIL", format!("Không lưu được khóa: {e}")))
    }
}

pub fn get_api_key(which: &str) -> Option<String> {
    let key_name = match which {
        "anh" => KEY_IMAGE,
        "video" => KEY_VIDEO,
        _ => return None,
    };
    let key = keyring::Entry::new(KEYRING_SERVICE, key_name)
        .ok()
        .and_then(|e| e.get_password().ok())
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty());
    if let Some(ref k) = key {
        register_secret_for_redaction(k);
    }
    key
}

pub fn has_api_key(which: &str) -> bool {
    get_api_key(which).is_some()
}

/// Che khóa API an toàn (không bao giờ làm lộ khóa ngắn `<= 4` ký tự):
/// - Rỗng -> `None`
/// - `1..=4` ký tự -> `Some("••••")`
/// - `5..=8` ký tự -> `Some("••••ab")` (2 ký tự cuối)
/// - `> 8` ký tự -> `Some("••••ab12")` (4 ký tự cuối)
pub fn mask_api_key(raw_key: &str) -> Option<String> {
    let trimmed = raw_key.trim();
    let chars: Vec<char> = trimmed.chars().collect();
    let len = chars.len();
    if len == 0 {
        None
    } else if len <= 4 {
        Some("••••".into())
    } else if len <= 8 {
        let last_2: String = chars[len - 2..].iter().collect();
        Some(format!("••••{last_2}"))
    } else {
        let last_4: String = chars[len - 4..].iter().collect();
        Some(format!("••••{last_4}"))
    }
}

/// Hiển thị bản xem trước khóa đã lưu (6.6b)
pub fn key_preview(which: &str) -> Option<String> {
    get_api_key(which).and_then(|k| mask_api_key(&k))
}

fn is_sk_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn is_bearer_token_char(c: char) -> bool {
    !c.is_whitespace() && !matches!(c, '"' | '\'' | ',' | ';' | ')' | ']' | '}' | '>')
}

/// Che mọi chuỗi nhạy cảm:
/// 1) Các khóa API thực tế đã nạp/lưu từ Keyring (kể cả định dạng không có `sk-`/`Bearer`)
/// 2) `Bearer <token>`
/// 3) `sk-[A-Za-z0-9_-]+` (tôn trọng ranh giới từ để không hỏng `task-123`)
pub fn redact_secrets(input: &str) -> String {
    #[cfg(not(test))]
    {
        static INIT_KEYRING_SECRETS: std::sync::Once = std::sync::Once::new();
        INIT_KEYRING_SECRETS.call_once(|| {
            let _ = get_api_key("anh");
            let _ = get_api_key("video");
        });
    }

    let mut stage0 = input.to_string();
    {
        let secrets = known_secrets().read();
        for sec in secrets.iter() {
            if !sec.is_empty() && stage0.contains(sec.as_str()) {
                stage0 = stage0.replace(sec.as_str(), "[REDACTED]");
            }
        }
    }

    let pass1 = redact_bearer(&stage0);
    redact_sk(&pass1)
}

fn redact_bearer(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut idx = 0;
    while idx < input.len() {
        let rest = &input[idx..];
        if rest.len() >= 7 && rest.is_char_boundary(6) && rest[..6].eq_ignore_ascii_case("bearer") {
            let prev_ok = idx == 0
                || !input[..idx]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_ascii_alphanumeric());
            let after_kw = &rest[6..];
            let ws_bytes: usize = after_kw
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .map(char::len_utf8)
                .sum();
            if prev_ok && ws_bytes > 0 {
                let after_ws = &after_kw[ws_bytes..];
                let tok_bytes: usize = after_ws
                    .chars()
                    .take_while(|c| is_bearer_token_char(*c))
                    .map(char::len_utf8)
                    .sum();
                if tok_bytes > 0 {
                    out.push_str("Bearer [REDACTED]");
                    idx += 6 + ws_bytes + tok_bytes;
                    continue;
                }
            }
        }
        let ch = rest.chars().next().expect("idx < input.len()");
        out.push(ch);
        idx += ch.len_utf8();
    }
    out
}

fn redact_sk(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut idx = 0;
    while idx < input.len() {
        let rest = &input[idx..];
        if let Some(after_prefix) = rest.strip_prefix("sk-") {
            let prev_ok = idx == 0
                || !input[..idx]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_ascii_alphanumeric());
            let tok_bytes: usize = after_prefix
                .chars()
                .take_while(|c| is_sk_token_char(*c))
                .map(char::len_utf8)
                .sum();
            if prev_ok && tok_bytes > 0 {
                out.push_str("sk-[REDACTED]");
                idx += 3 + tok_bytes;
                continue;
            }
        }
        let ch = rest.chars().next().expect("idx < input.len()");
        out.push(ch);
        idx += ch.len_utf8();
    }
    out
}

/// Ghi log kỹ thuật kèm `detail` tùy chọn vào bảng `logs` (đã tự động lọc che khóa bí mật ở cả `message` & `detail`).
pub fn log_detail(
    conn: &Connection,
    level: &str,
    run_id: Option<i64>,
    row_key: Option<&str>,
    step: Option<&str>,
    message: &str,
    detail: Option<&str>,
) {
    let ts = chrono::Local::now().to_rfc3339();
    let safe_level = match level {
        "info" | "warn" | "error" => level,
        _ => "info",
    };
    let safe_msg = redact_secrets(message);
    let safe_detail = detail.map(redact_secrets);
    let _ = conn.execute(
        "INSERT INTO logs(ts, level, run_id, row_key, step, message, detail) VALUES (?1,?2,?3,?4,?5,?6,?7)",
        rusqlite::params![ts, safe_level, run_id, row_key, step, safe_msg, safe_detail],
    );
}

/// Ghi log kỹ thuật tương thích ngược (`detail = None`).
pub fn log(
    conn: &Connection,
    level: &str,
    run_id: Option<i64>,
    row_key: Option<&str>,
    step: Option<&str>,
    message: &str,
) {
    log_detail(conn, level, run_id, row_key, step, message, None);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn test_mask_api_key_never_leaks_short_keys() {
        assert_eq!(mask_api_key(""), None);
        assert_eq!(mask_api_key("   "), None);
        assert_eq!(mask_api_key("a"), Some("••••".into()));
        assert_eq!(mask_api_key("ab12"), Some("••••".into()));
        assert_eq!(mask_api_key("12345"), Some("••••45".into()));
        assert_eq!(mask_api_key("12345678"), Some("••••78".into()));
        assert_eq!(mask_api_key("123456789"), Some("••••6789".into()));
        assert_eq!(mask_api_key("sk-proj-abcdef9876"), Some("••••9876".into()));
    }

    #[test]
    fn test_redact_secrets_and_log_detail_including_custom_keyring_secret() {
        register_secret_for_redaction("vf_custom_proxy_token_778899");
        let raw_msg = "Lỗi gọi API với Authorization: Bearer eyJhbGciOiJIUzI1NiIs.xyz, key=sk-live_AbCdEf123456 trên task-9988";
        let raw_detail = "raw header X-Api-Key: vf_custom_proxy_token_778899 failed";

        let clean_msg = redact_secrets(raw_msg);
        assert!(!clean_msg.contains("eyJhbGciOiJIUzI1NiIs"));
        assert!(!clean_msg.contains("AbCdEf123456"));
        assert!(clean_msg.contains("Bearer [REDACTED]"));
        assert!(clean_msg.contains("sk-[REDACTED]"));
        assert!(clean_msg.contains("task-9988"), "must not corrupt task-9988");

        let idx = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "videoflow_settings_log_test_{}_{}",
            std::process::id(),
            idx
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let conn = crate::state_db::open(&dir).expect("open db");
        log_detail(
            &conn,
            "error",
            None,
            Some("a.png"),
            Some("B2"),
            raw_msg,
            Some(raw_detail),
        );
        let (saved_msg, saved_detail): (String, Option<String>) = conn
            .query_row("SELECT message, detail FROM logs LIMIT 1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .expect("query log");
        assert_eq!(saved_msg, clean_msg);
        let det = saved_detail.expect("detail stored");
        assert!(!det.contains("vf_custom_proxy_token_778899"));
        assert!(det.contains("[REDACTED]"));

        // Kiểm tra apply_windows_autostart no-op an toàn trong test
        apply_windows_autostart(true).expect("autostart noop true");
        apply_windows_autostart(false).expect("autostart noop false");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_settings_store_trim_atomic_save_and_bounds_validation() {
        let idx = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "videoflow_settings_bounds_{}_{}",
            std::process::id(),
            idx
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let store_file = dir.join("settings.json");
        let store = SettingsStore::with_path(store_file.clone());

        let bad = Settings {
            endpoint_image_base_url: "  https://api.example.com/v1/  ".into(),
            endpoint_image_model: "   ".into(), // rỗng -> fallback về mặc định
            endpoint_video_base_url: " https://vid.example.com/v1 ".into(),
            endpoint_video_model: " sora-2 ".into(),
            video_size: "  ".into(),
            concurrency: 99,
            max_attempts: 0,
            image_timeout_s: 0,
            video_create_timeout_s: 0,
            poll_interval_s: 0,
            poll_max_s: 0,
            backoff_s: [0, 0, 5],
            recent_projects: vec![
                " p1 ".into(),
                "p2".into(),
                "p3".into(),
                "p4".into(),
                "p5".into(),
                "p6".into(),
            ],
            ..Default::default()
        };
        store.save(&bad).expect("save clamped settings atomically");
        assert!(
            !store_file.with_extension("json.videoflow_tmp").exists(),
            "atomic save must not leave .tmp file"
        );

        let loaded = store.load();
        assert_eq!(loaded.endpoint_image_base_url, "https://api.example.com/v1");
        assert_eq!(loaded.endpoint_image_model, "gpt-image-2");
        assert_eq!(loaded.endpoint_video_base_url, "https://vid.example.com/v1");
        assert_eq!(loaded.endpoint_video_model, "sora-2");
        assert_eq!(loaded.video_size, "720x1280");
        assert_eq!(loaded.concurrency, 3);
        assert_eq!(loaded.max_attempts, 1);
        assert_eq!(loaded.image_timeout_s, 1);
        assert_eq!(loaded.video_create_timeout_s, 1);
        assert_eq!(loaded.poll_interval_s, 1);
        assert_eq!(loaded.poll_max_s, 1);
        assert_eq!(loaded.backoff_s, [1, 1, 5]);
        assert_eq!(loaded.recent_projects.len(), 5);
        assert_eq!(loaded.recent_projects[0], "p1");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_settings_auto_merge_default_and_backward_compatibility() {
        let def = Settings::default();
        assert!(!def.auto_merge, "auto_merge default must be false");

        // Backward compatibility: old settings.json without auto_merge
        let legacy_json = r#"{
            "endpoint_image_base_url": "https://api.example.com/v1",
            "concurrency": 2
        }"#;
        let loaded_legacy: Settings = serde_json::from_str(legacy_json).expect("deserialize legacy json");
        assert!(!loaded_legacy.auto_merge, "missing auto_merge in json must default to false");

        // Explicitly enabled auto_merge
        let enabled_json = r#"{
            "auto_merge": true
        }"#;
        let loaded_enabled: Settings = serde_json::from_str(enabled_json).expect("deserialize enabled json");
        assert!(loaded_enabled.auto_merge, "explicit auto_merge true must be parsed");

        // SettingsStore persistence
        let idx = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "videoflow_settings_automerge_{}_{}",
            std::process::id(),
            idx
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let store_file = dir.join("settings.json");
        let store = SettingsStore::with_path(store_file);

        let mut custom = Settings::default();
        custom.auto_merge = true;
        store.save(&custom).expect("save settings with auto_merge = true");

        let reloaded = store.load();
        assert!(reloaded.auto_merge, "reloaded settings must preserve auto_merge = true");

        custom.auto_merge = false;
        store.save(&custom).expect("save settings with auto_merge = false");
        let reloaded_false = store.load();
        assert!(!reloaded_false.auto_merge, "reloaded settings must preserve auto_merge = false");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
