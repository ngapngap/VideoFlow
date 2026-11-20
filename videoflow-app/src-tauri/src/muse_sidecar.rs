//! Quản lý engine VideoFlow Muse — sidecar PyInstaller (muse2api) đóng gói sẵn, biến
//! muse.ai thành API OpenAI-compatible.
//!
//! Engine là exe `videoflow-muse.exe` đi kèm bundle (dev: `binaries/videoflow-muse-<triple>.exe`),
//! chạy ngầm trên localhost với driver browser (CDP Edge/Chrome). Cookie muse.ai nằm
//! trong `data/accounts.json` của engine (một nguồn sự thật, tự renew session); app
//! chỉ relay qua Admin API. API key nội bộ sinh mỗi phiên, chỉ giữ trong RAM.

use crate::error::{AppError, AppResult};
use crate::settings::Settings;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Tên exe engine trong bundle: Tauri externalBin bỏ hậu tố target-triple khi cài đặt.
pub const SIDECAR_EXE_BUNDLED: &str = "videoflow-muse.exe";
/// Tên exe engine trong `src-tauri/binaries/` lúc dev (giữ hậu tố target-triple MSVC / GNU).
pub const SIDECAR_EXE_DEV: &str = "videoflow-muse-x86_64-pc-windows-msvc.exe";
pub const SIDECAR_EXE_DEV_GNU: &str = "videoflow-muse-x86_64-pc-windows-gnu.exe";
/// Thư mục dữ liệu engine (accounts.json, browser profile, log) trong %APPDATA%.
pub const MUSE_DATA_DIR: &str = "muse";

#[derive(Debug)]
struct SidecarHandle {
    child: Child,
    port: u16,
    api_key: String,
    /// Cấu hình đã dùng để spawn — so với Settings hiện tại để biết cần restart.
    cfg_port: u16,
    cfg_chromium: String,
    cfg_headless: bool,
}

static SIDECAR: OnceLock<parking_lot::RwLock<Option<SidecarHandle>>> = OnceLock::new();
static START_CANCELLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn sidecar_cell() -> &'static parking_lot::RwLock<Option<SidecarHandle>> {
    SIDECAR.get_or_init(|| parking_lot::RwLock::new(None))
}

#[cfg(target_os = "windows")]
fn windows_system32_exe(rel: &str) -> PathBuf {
    let sys_root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    sys_root.join("System32").join(rel)
}

/// Trạng thái sidecar cho UI.
#[derive(Debug, Clone, Serialize)]
pub struct SidecarStatus {
    pub running: bool,
    pub port: Option<u16>,
    pub version: Option<String>,
    pub driver: Option<String>,
    pub account_total: Option<u32>,
    pub account_available: Option<u32>,
    /// Cấu hình lúc spawn (để UI biết cài đặt đã đổi mà chưa áp dụng).
    pub cfg_port: Option<u16>,
    pub cfg_chromium: Option<String>,
    pub cfg_headless: Option<bool>,
    /// True khi Cài đặt hiện tại khác cấu hình engine đang chạy → cần Khởi động lại.
    pub restart_required: bool,
}

/// Thông tin phiên cần cho lớp adapter gọi API nội bộ.
#[derive(Debug, Clone)]
pub struct SidecarSession {
    pub port: u16,
    pub api_key: String,
}

impl SidecarSession {
    /// Base URL nội bộ của lớp `/v1` (`http://127.0.0.1:{port}/v1`).
    pub fn v1_base(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }
}

fn exe_name() -> &'static str {
    SIDECAR_EXE_BUNDLED
}

/// Đường dẫn exe sidecar: cạnh exe app (bundle: `videoflow-muse.exe`) hoặc `binaries/` từ
/// ancestor (dev: `target/debug/...` → `src-tauri/binaries/videoflow-muse-<triple>.exe`).
pub fn sidecar_exe_path() -> AppResult<PathBuf> {
    let names = [SIDECAR_EXE_BUNDLED, SIDECAR_EXE_DEV, SIDECAR_EXE_DEV_GNU];
    if let Ok(cur) = std::env::current_exe() {
        if let Some(dir) = cur.parent() {
            for name in names {
                let p = dir.join(name);
                if p.is_file() {
                    return Ok(p);
                }
            }
        }
        for anc in cur.ancestors().skip(1).take(8) {
            for name in names {
                let p = anc.join("binaries").join(name);
                if p.is_file() {
                    return Ok(p);
                }
            }
        }
    }
    Err(AppError::new(
        "MUSE_SIDECAR_MISSING",
        format!(
            "Không tìm thấy {} cạnh ứng dụng. Đảm bảo đã build sidecar \
             (pyinstaller sidecar/muse2api.spec).",
            exe_name()
        ),
    ))
}

/// Tìm Edge/Chrome để sidecar điều khiển qua CDP: override trong Settings → Edge → Chrome.
pub fn detect_chromium(override_path: &str) -> AppResult<PathBuf> {
    let trimmed = override_path.trim();
    if !trimmed.is_empty() {
        let p = PathBuf::from(trimmed);
        if p.is_file() {
            return Ok(p);
        }
        return Err(AppError::new(
            "MUSE_CHROMIUM_NOT_FOUND",
            format!("Đường dẫn trình duyệt trong Cài đặt không tồn tại: {trimmed}"),
        ));
    }
    let candidates = [
        ("ProgramFiles(x86)", r"Microsoft\Edge\Application\msedge.exe"),
        ("ProgramFiles", r"Microsoft\Edge\Application\msedge.exe"),
        ("LOCALAPPDATA", r"Microsoft\Edge\Application\msedge.exe"),
        ("ProgramFiles", r"Google\Chrome\Application\chrome.exe"),
        ("ProgramFiles(x86)", r"Google\Chrome\Application\chrome.exe"),
        ("LOCALAPPDATA", r"Google\Chrome\Application\chrome.exe"),
    ];
    for (env_var, rel) in candidates {
        if let Ok(base) = std::env::var(env_var) {
            let p = PathBuf::from(base).join(rel);
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    Err(AppError::new(
        "MUSE_CHROMIUM_NOT_FOUND",
        "Không tìm thấy Microsoft Edge/Chrome trên máy. Cài Edge/Chrome hoặc nhập đường \
         dẫn thủ công trong Cài đặt → Nguồn video VideoFlow Muse.",
    ))
}

/// Sinh API key nội bộ ngẫu nhiên bằng CSPRNG cho phiên (chỉ giữ trong RAM).
fn generate_api_key() -> String {
    let mut buf = [0u8; 16];
    if getrandom::getrandom(&mut buf).is_ok() {
        let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
        return format!("vf_{hex}");
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let raw = format!("videoflow-muse-{nanos}-{pid}");
    let digest = Sha256::digest(raw.as_bytes());
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    format!("vf_{hex}")
}

/// Tìm port trống: thử port ưa thích trước (nếu không nằm trong `exclude`),
/// bận thì xin OS một port ngẫu nhiên không nằm trong `exclude`.
fn find_free_port_excluding_any(preferred: u16, exclude: &[u16]) -> AppResult<u16> {
    if preferred > 0
        && !exclude.contains(&preferred)
        && TcpListener::bind(("127.0.0.1", preferred)).is_ok()
    {
        return Ok(preferred);
    }
    for _ in 0..32 {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .map_err(|e| AppError::new("MUSE_PORT_FAIL", format!("Không xin được port trống: {e}")))?;
        let port = listener
            .local_addr()
            .map(|a| a.port())
            .map_err(|e| AppError::new("MUSE_PORT_FAIL", format!("Không đọc được port: {e}")))?;
        if !exclude.contains(&port) {
            return Ok(port);
        }
    }
    Err(AppError::new(
        "MUSE_PORT_FAIL",
        "Không tìm được port trống không trùng.",
    ))
}

/// Tìm port trống: thử port ưa thích trước (nếu khác `exclude`), bận thì xin OS một port ngẫu nhiên.
#[cfg(test)]
fn find_free_port_excluding(preferred: u16, exclude: Option<u16>) -> AppResult<u16> {
    match exclude {
        Some(p) => find_free_port_excluding_any(preferred, &[p]),
        None => find_free_port_excluding_any(preferred, &[]),
    }
}

#[cfg(test)]
fn find_free_port(preferred: u16) -> AppResult<u16> {
    find_free_port_excluding(preferred, None)
}

fn blocking_client_with_timeout(timeout_secs: u64) -> AppResult<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .map_err(|e| AppError::new("MUSE_HTTP_FAIL", format!("Không tạo được HTTP client: {e}")))
}

/// GET request tới Admin API của sidecar với timeout tùy chỉnh, trả JSON.
pub fn admin_get_timeout(
    port: u16,
    api_key: &str,
    path: &str,
    timeout_secs: u64,
) -> AppResult<serde_json::Value> {
    let url = format!("http://127.0.0.1:{port}{path}");
    let resp = blocking_client_with_timeout(timeout_secs)?
        .get(&url)
        .bearer_auth(api_key)
        .send()
        .map_err(|e| AppError::new("MUSE_ADMIN_UNREACHABLE", format!("Sidecar không phản hồi: {e}")))?;
    parse_admin_response(resp)
}

/// GET request tới Admin API của sidecar, trả JSON.
pub fn admin_get(port: u16, api_key: &str, path: &str) -> AppResult<serde_json::Value> {
    admin_get_timeout(port, api_key, path, 10)
}

/// POST/DELETE/PATCH tới Admin API với body JSON và timeout tùy chỉnh.
pub fn admin_send_timeout(
    port: u16,
    api_key: &str,
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
    timeout_secs: u64,
) -> AppResult<serde_json::Value> {
    let url = format!("http://127.0.0.1:{port}{path}");
    let client = blocking_client_with_timeout(timeout_secs)?;
    let method = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|e| AppError::new("MUSE_ADMIN_FAIL", format!("Sai HTTP method: {e}")))?;
    let mut req = client.request(method, &url).bearer_auth(api_key);
    if let Some(b) = body {
        req = req.json(b);
    }
    let resp = req
        .send()
        .map_err(|e| AppError::new("MUSE_ADMIN_UNREACHABLE", format!("Sidecar không phản hồi: {e}")))?;
    parse_admin_response(resp)
}

/// POST/DELETE/PATCH tới Admin API với body JSON tùy chọn.
pub fn admin_send(
    port: u16,
    api_key: &str,
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
) -> AppResult<serde_json::Value> {
    admin_send_timeout(port, api_key, method, path, body, 10)
}

pub fn open_login(port: u16, api_key: &str, account_id: &str) -> AppResult<serde_json::Value> {
    let path = format!("/admin/accounts/{account_id}/open-login");
    admin_send(port, api_key, "POST", &path, None)
}

pub fn open_view(port: u16, api_key: &str, account_id: &str) -> AppResult<serde_json::Value> {
    let path = format!("/admin/accounts/{account_id}/open-view");
    admin_send(port, api_key, "POST", &path, None)
}

/// Chuyển response Admin API thành JSON; lỗi HTTP → AppError giữ `code` của sidecar
/// ( ví dụ `invalid_request` khi cookie trùng) để UI hiển thị đúng message tiếng Anh gốc.
fn parse_admin_response(resp: reqwest::blocking::Response) -> AppResult<serde_json::Value> {
    let status = resp.status();
    let body = resp
        .text()
        .map_err(|e| AppError::new("MUSE_ADMIN_FAIL", format!("Đọc response thất bại: {e}")))?;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        let sidecar_msg = json
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or(&body);
        let sidecar_code = json
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_str())
            .unwrap_or("ADMIN_FAIL");
        let upper_code = sidecar_code.to_ascii_uppercase();
        let err_code = if upper_code.starts_with("MUSE_") {
            upper_code
        } else {
            format!("MUSE_{upper_code}")
        };
        return Err(AppError::new(
            &err_code,
            crate::settings::redact_secrets(&format!("Sidecar trả lỗi {status}: {sidecar_msg}")),
        ));
    }
    Ok(json)
}

/// Chờ `/healthz` sẵn sàng (onefile PyInstaller giải nén mất vài giây lần đầu).
fn wait_health(port: u16, child: &mut Child, deadline_secs: u64) -> AppResult<()> {
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|e| AppError::new("MUSE_HTTP_FAIL", format!("Không tạo được HTTP client: {e}")))?;
    let url = format!("http://127.0.0.1:{port}/healthz");
    let deadline = Instant::now() + Duration::from_secs(deadline_secs);
    loop {
        if START_CANCELLED.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(AppError::new(
                "MUSE_START_CANCELLED",
                "Đã hủy khởi động engine.",
            ));
        }
        if Instant::now() >= deadline {
            return Err(AppError::new(
                "MUSE_SIDECAR_TIMEOUT",
                "Engine VideoFlow Muse không phản hồi /healthz sau 90 giây. Xem log trong \
                 %APPDATA%\\VideoFlowDesktop\\muse\\engine-stderr.log",
            ));
        }
        if let Ok(Some(exit)) = child.try_wait() {
            return Err(AppError::new(
                "MUSE_SIDECAR_EXITED",
                format!("Engine thoát ngay khi khởi động (code {exit}). Xem log trong \
                 %APPDATA%\\VideoFlowDesktop\\muse\\engine-stderr.log"),
            ));
        }
        if let Ok(resp) = client.get(&url).send() {
            if resp.status().is_success() {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(400));
    }
}

/// Spawn sidecar (lazy): nếu đang chạy thì dùng lại, chưa thì khởi động mới.
pub fn ensure_running(cfg: &Settings) -> AppResult<SidecarSession> {
    if let Some(session) = current_session() {
        return Ok(session);
    }
    start(cfg)?;
    current_session().ok_or_else(|| {
        AppError::new("MUSE_SIDECAR_FAIL", "Sidecar đã khởi động nhưng mất trạng thái phiên.")
    })
}

/// Phiên hiện tại (port + API key) nếu sidecar đang chạy và tiến trình chưa thoát.
pub fn current_session() -> Option<SidecarSession> {
    let mut guard = sidecar_cell().write();
    if let Some(h) = guard.as_mut() {
        match h.child.try_wait() {
            Ok(Some(_)) | Err(_) => {
                *guard = None;
                return None;
            }
            Ok(None) => {
                return Some(SidecarSession {
                    port: h.port,
                    api_key: h.api_key.clone(),
                });
            }
        }
    }
    None
}

/// Khóa tuần tự hóa việc spawn: hai lệnh song song (vd bấm Thêm cookie khi engine chưa chạy)
/// không được spawn 2 engine tranh nhau port/profile Edge.
static START_LOCK: OnceLock<parking_lot::Mutex<()>> = OnceLock::new();

fn start_lock() -> &'static parking_lot::Mutex<()> {
    START_LOCK.get_or_init(|| parking_lot::Mutex::new(()))
}

/// Đồng bộ cấu hình proxy từ `proxies.db` xuống sidecar vừa khởi động (best-effort, không chặn `start()`).
fn sync_proxies_to_sidecar(port: u16, api_key: &str) {
    let conn = match crate::proxies::open_db() {
        Ok(c) => c,
        Err(e) => {
            log_info(&format!(
                "Bỏ qua đồng bộ proxy khi khởi động sidecar (không mở được proxies.db): {}",
                e.message
            ));
            return;
        }
    };
    sync_proxies_to_sidecar_with_conn(port, api_key, &conn);
}

/// Đồng bộ proxy từ kết nối `proxies.db` cho sẵn xuống sidecar qua `/admin/accounts`.
/// - Luôn gửi URL đầy đủ cho account được gán (xử lý cả trường hợp chỉ đổi password/username khi sidecar dừng).
/// - Xóa proxy trên sidecar (`{"proxy": ""}`) nếu account không còn được gán/proxy đã xóa.
/// - Bỏ qua account đang `busy == true`, hoặc log và tiếp tục nếu gặp lỗi từng account (vd HTTP 409 AccountBusy).
pub(crate) fn sync_proxies_to_sidecar_with_conn(
    port: u16,
    api_key: &str,
    conn: &rusqlite::Connection,
) {
    let acc_to_proxy = match crate::proxies::load_account_to_proxy_map(conn) {
        Ok(m) => m,
        Err(e) => {
            log_info(&format!(
                "Bỏ qua đồng bộ proxy khi khởi động sidecar (đọc account_proxies lỗi): {}",
                e.message
            ));
            return;
        }
    };
    let (proxy_urls_map, failed_proxy_ids) =
        match crate::proxies::resolve_proxy_urls_map_best_effort_in(conn) {
            Ok(pair) => pair,
            Err(e) => {
                log_info(&format!(
                    "Bỏ qua đồng bộ proxy khi khởi động sidecar (resolve proxy URL lỗi): {}",
                    e.message
                ));
                return;
            }
        };
    if !failed_proxy_ids.is_empty() {
        log_info(&format!(
            "Cảnh báo: một số proxy không giải mã được khi khởi động sidecar ({}); giữ nguyên các proxy khỏe mạnh còn lại.",
            failed_proxy_ids.join(", ")
        ));
    }

    let res = match admin_get(port, api_key, "/admin/accounts") {
        Ok(v) => v,
        Err(e) => {
            log_info(&format!(
                "Bỏ qua đồng bộ proxy khi khởi động sidecar (GET /admin/accounts lỗi): {}",
                e.message
            ));
            return;
        }
    };

    let Some(accounts) = res.get("data").and_then(|d| d.as_array()) else {
        return;
    };

    for obj in accounts {
        let Some(acc_id) = obj
            .get("id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let safe_id = match crate::adapters::sanitize_url_segment(acc_id, false) {
            Ok(s) => s,
            Err(e) => {
                log_info(&format!(
                    "Bỏ qua đồng bộ proxy cho account id không hợp lệ '{acc_id}': {}",
                    e.message
                ));
                continue;
            }
        };

        let is_busy = obj.get("busy").and_then(|v| v.as_bool()).unwrap_or(false);
        let current_masked = obj
            .get("proxy")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());

        if let Some(pid) = acc_to_proxy.get(acc_id) {
            let Some(full_url) = proxy_urls_map.get(pid) else {
                log_info(&format!(
                    "Cảnh báo: bỏ qua đồng bộ proxy '{pid}' cho account '{acc_id}' do không giải mã được."
                ));
                continue;
            };
            if is_busy {
                log_info(&format!(
                    "Cảnh báo: account '{acc_id}' đang bận khi khởi động sidecar — bỏ qua đồng bộ proxy."
                ));
                continue;
            }
            if let Err(e) = admin_send(
                port,
                api_key,
                "PATCH",
                &format!("/admin/accounts/{safe_id}"),
                Some(&serde_json::json!({ "proxy": full_url })),
            ) {
                log_info(&format!(
                    "Cảnh báo: không đồng bộ được proxy cho account '{acc_id}' khi khởi động ({}): {}",
                    e.code, e.message
                ));
            }
        } else if current_masked.is_some() {
            if is_busy {
                log_info(&format!(
                    "Cảnh báo: account '{acc_id}' đang bận khi khởi động sidecar — bỏ qua xóa proxy cũ."
                ));
                continue;
            }
            if let Err(e) = admin_send(
                port,
                api_key,
                "PATCH",
                &format!("/admin/accounts/{safe_id}"),
                Some(&serde_json::json!({ "proxy": "" })),
            ) {
                log_info(&format!(
                    "Cảnh báo: không xóa được proxy cũ cho account '{acc_id}' khi khởi động ({}): {}",
                    e.code, e.message
                ));
            }
        }
    }
}

/// Khởi động sidecar với driver browser; chờ /healthz sẵn sàng.
/// Cold start gồm PyInstaller onefile giải nén + khởi động Edge + CDP (~30-60s) nên
/// deadline là 90 giây; quá hạn thì kill luôn process con để không để lại engine mồ côi.
pub fn start(cfg: &Settings) -> AppResult<SidecarStatus> {
    let _spawn_guard = start_lock().lock();
    START_CANCELLED.store(false, std::sync::atomic::Ordering::SeqCst);
    if current_session().is_some() {
        return status(cfg);
    }

    let exe = sidecar_exe_path()?;
    let chromium = detect_chromium(&cfg.muse_chromium_path)?;
    let data_dir = crate::settings::app_data_dir().join(MUSE_DATA_DIR);
    std::fs::create_dir_all(&data_dir).ok();

    // Dọn sidecar & Edge mồ côi (từ phiên app cũ đóng bất thường) TRƯỚC khi chọn port và mở log.
    kill_orphan_sidecars();
    kill_orphan_edges(&data_dir);

    let api_key = generate_api_key();
    crate::settings::register_secret_for_redaction(&api_key);

    // Log stdout/stderr của sidecar ra file để chẩn đoán khi khởi động thất bại.
    let stdout_path = data_dir.join("engine-stdout.log");
    let stderr_path = data_dir.join("engine-stderr.log");

    // Hai lần thử: engine chết ngay (exit code 3) hầu như luôn do Edge kẹt profile hoặc tranh port.
    // Lần 2 spawn mới với port mới sau khi dọn orphan, không cần user bấm lại.
    let mut last_err: Option<AppError> = None;
    let mut failed_ports: Vec<u16> = Vec::new();
    for attempt in 1..=2 {
        if START_CANCELLED.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(AppError::new(
                "MUSE_START_CANCELLED",
                "Đã hủy khởi động engine.",
            ));
        }
        if attempt == 2 {
            log_info("engine thoát ngay lần 1 — dọn Edge mồ côi và profile rồi thử lại");
            kill_orphan_sidecars();
            kill_orphan_edges(&data_dir);
            for _ in 0..15 {
                if START_CANCELLED.load(std::sync::atomic::Ordering::SeqCst) {
                    return Err(AppError::new(
                        "MUSE_START_CANCELLED",
                        "Đã hủy khởi động engine.",
                    ));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }

        let port = find_free_port_excluding_any(cfg.muse_sidecar_port, &failed_ports)?;
        let mut cdp_exclude = failed_ports.clone();
        cdp_exclude.push(port);
        let cdp_port = find_free_port_excluding_any(19210, &cdp_exclude)?;

        let stdout_file = open_sidecar_log_file(&stdout_path, "stdout")?;
        let stderr_file = open_sidecar_log_file(&stderr_path, "stderr")?;

        let mut cmd = Command::new(&exe);
        cmd.env("MUSE2API_DRIVER", "browser")
            .env("MUSE2API_HOST", "127.0.0.1")
            .env("MUSE2API_PORT", port.to_string())
            .env("MUSE2API_CDP_PORT", cdp_port.to_string())
            .env("MUSE2API_API_KEY", &api_key)
            .env("MUSE2API_ADMIN_KEY", &api_key)
            .env("MUSE2API_DATA_DIR", &data_dir)
            .env("MUSE2API_CHROMIUM_PATH", &chromium)
            .env("MUSE2API_HEADLESS", if cfg.muse_headless { "true" } else { "false" })
            .env("MUSE2API_DEFER_PREWARM", "true")
            .stdout(Stdio::from(stdout_file))
            .stderr(Stdio::from(stderr_file));
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                failed_ports.push(port);
                failed_ports.push(cdp_port);
                last_err = Some(AppError::new(
                    "MUSE_SIDECAR_FAIL",
                    format!("Không chạy được engine: {e}"),
                ));
                continue;
            }
        };
        match wait_health(port, &mut child, 90) {
            Ok(()) => {
                // Đồng bộ proxy TRƯỚC khi kích hoạt prewarm và công bố SidecarHandle.
                sync_proxies_to_sidecar(port, &api_key);
                if let Err(e) = admin_send(port, &api_key, "POST", "/admin/prewarm", None) {
                    log_info(&format!("Bỏ qua /admin/prewarm sau khởi động: {}", e.message));
                }
                *sidecar_cell().write() = Some(SidecarHandle {
                    child,
                    port,
                    api_key: api_key.clone(),
                    cfg_port: cfg.muse_sidecar_port,
                    cfg_chromium: cfg.muse_chromium_path.trim().to_string(),
                    cfg_headless: cfg.muse_headless,
                });
                let st = status(cfg)?;
                log_info(&format!(
                    "VideoFlow Muse engine đã chạy: port={port}, cdp={cdp_port}, chromium={}",
                    chromium.display()
                ));
                return Ok(st);
            }
            Err(e) => {
                // Engine không lên được: kill cây process để không để lại engine mồ côi.
                kill_child_tree(&mut child);
                if e.code == "MUSE_START_CANCELLED"
                    || START_CANCELLED.load(std::sync::atomic::Ordering::SeqCst)
                {
                    return Err(e);
                }
                failed_ports.push(port);
                failed_ports.push(cdp_port);
                last_err = Some(e.with_engine_hint(&data_dir));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| {
        AppError::new("MUSE_SIDECAR_FAIL", "Không khởi động được engine VideoFlow Muse.")
    }))
}

/// Gắn thêm các dòng lỗi cuối từ log engine & Chromium để thông báo lỗi có nội dung thật
/// (thay vì chỉ "exit code 3") — người dùng thấy ngay nguyên nhân thay vì phải tự mở file log.
trait EngineErrorHint {
    fn with_engine_hint(self, data_dir: &std::path::Path) -> AppError;
}

fn read_tail_lossy(path: &std::path::Path, max_bytes: u64) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let seeked = len > max_bytes;
    if seeked {
        file.seek(SeekFrom::End(-(max_bytes as i64)))?;
    }
    let mut buf = Vec::with_capacity(len.min(max_bytes) as usize);
    file.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    if seeked {
        if let Some(pos) = text.find('\n') {
            return Ok(text[pos + 1..].to_string());
        }
    }
    Ok(text)
}

fn extract_interesting_stderr_line(path: &std::path::Path) -> Option<String> {
    const MAX_LOG_TAIL_BYTES: u64 = 64 * 1024;
    let text = read_tail_lossy(path, MAX_LOG_TAIL_BYTES).ok()?;
    text.lines()
        .filter(|l| {
            let low = l.to_lowercase();
            low.contains("error")
                || low.contains("exited early")
                || low.contains("runtimeerror")
                || low.contains("devtools")
        })
        .rfind(|l| !l.trim().is_empty())
        .map(|last| {
            crate::settings::redact_secrets(last.trim())
                .chars()
                .take(300)
                .collect::<String>()
        })
}

fn read_engine_error_hint(data_dir: &std::path::Path) -> Vec<String> {
    const MAX_PROFILE_ENTRIES: usize = 64;
    const MAX_PROFILE_LOGS: usize = 2;

    let mut candidates: Vec<(String, PathBuf)> = vec![
        (
            "browser-profile/chromium-stderr.log".to_string(),
            data_dir.join("browser-profile").join("chromium-stderr.log"),
        ),
        (
            "chromium-stderr.log".to_string(),
            data_dir.join("chromium-stderr.log"),
        ),
    ];

    if let Ok(entries) = std::fs::read_dir(data_dir.join("profiles")) {
        let mut profile_logs: Vec<(std::time::SystemTime, String, PathBuf)> = Vec::new();
        for entry in entries.take(MAX_PROFILE_ENTRIES).flatten() {
            let log_path = entry.path().join("chromium-stderr.log");
            if let Ok(meta) = std::fs::metadata(&log_path) {
                if meta.is_file() {
                    let modified = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                    let dir_name = entry.file_name().to_string_lossy().into_owned();
                    profile_logs.push((
                        modified,
                        format!("profiles/{dir_name}/chromium-stderr.log"),
                        log_path,
                    ));
                }
            }
        }
        profile_logs.sort_by_key(|a| std::cmp::Reverse(a.0));
        for (_, label, path) in profile_logs.into_iter().take(MAX_PROFILE_LOGS) {
            candidates.push((label, path));
        }
    }

    candidates.push((
        "engine-stderr.log".to_string(),
        data_dir.join("engine-stderr.log"),
    ));

    let mut hints = Vec::new();
    for (label, path) in candidates {
        if let Some(line) = extract_interesting_stderr_line(&path) {
            hints.push(format!("{label}: {line}"));
        }
    }
    hints
}

impl EngineErrorHint for AppError {
    fn with_engine_hint(self, data_dir: &std::path::Path) -> AppError {
        let mut parts = vec![self.message];
        parts.extend(read_engine_error_hint(data_dir));
        AppError::new(
            &self.code,
            crate::settings::redact_secrets(&parts.join(" — ")),
        )
    }
}

/// Mở file log sidecar; nếu file chính vẫn bị tiến trình khác khóa (Windows OS error 32),
/// fallback sang append hoặc file log kèm PID để không làm hỏng bước khởi động engine.
fn open_sidecar_log_file(path: &std::path::Path, stream_name: &str) -> AppResult<std::fs::File> {
    match std::fs::File::create(path) {
        Ok(f) => Ok(f),
        Err(first_err) => {
            if let Ok(f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                return Ok(f);
            }
            let fallback = path.with_file_name(format!(
                "engine-{stream_name}-{}.log",
                std::process::id()
            ));
            std::fs::File::create(&fallback).map_err(|e| {
                AppError::new(
                    "MUSE_SIDECAR_FAIL",
                    format!("Không mở được log {stream_name}: {first_err} (fallback: {e})"),
                )
            })
        }
    }
}

/// Dọn tiến trình `videoflow-muse*.exe` mồ côi còn sót từ phiên trước (tránh OS error 32
/// khi tạo lại `engine-stdout.log` / `engine-stderr.log` và tránh tranh port 18610).
fn kill_orphan_sidecars() {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let taskkill = windows_system32_exe("taskkill.exe");
        for img in [SIDECAR_EXE_BUNDLED, SIDECAR_EXE_DEV, SIDECAR_EXE_DEV_GNU] {
            let _ = Command::new(&taskkill)
                .args(["/IM", img, "/T", "/F"])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Dọn Edge/Chrome mồ côi còn giữ profile dưới `data_dir` (`browser-profile` hoặc `profiles/*`)
/// (xảy ra khi engine bị kill cứng không qua taskkill /T, hoặc app crash trước khi stop).
/// Chỉ kill process có command line chứa ĐÚNG `data_dir` của app — không đụng Edge/Chrome của người dùng.
fn kill_orphan_edges(data_dir: &std::path::Path) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let ps = windows_system32_exe(r"WindowsPowerShell\v1.0\powershell.exe");
        let script = "Get-CimInstance Win32_Process -Filter \"Name='msedge.exe' OR Name='chrome.exe'\" | \
             Where-Object { $_.CommandLine -and $env:VF_DATA_DIR -and $_.CommandLine.IndexOf($env:VF_DATA_DIR, [System.StringComparison]::OrdinalIgnoreCase) -ge 0 } | \
             ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }";
        let _ = Command::new(&ps)
            .env("VF_DATA_DIR", data_dir)
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        // Cho Edge nhả file/profile trước khi engine mới khởi động nó lần nữa.
        std::thread::sleep(Duration::from_millis(600));
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = data_dir;
    }
}

/// Kill cả cây process (taskkill /T /F) để Chromium con do engine mở cũng chết.
fn kill_child_tree(child: &mut Child) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let pid = child.id();
        let taskkill = windows_system32_exe("taskkill.exe");
        let _ = Command::new(&taskkill)
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = child.kill();
    }
    let _ = child.wait();
}

/// Dừng sidecar (taskkill /T /F để chết cả Chromium con do sidecar mở).
/// Ngay cả khi handle trong RAM là `None` (vd sau khi sidecar tự thoát hoặc crash),
/// vẫn dọn tiến trình sidecar/Edge mồ côi bám trên `data_dir`.
pub fn stop() -> AppResult<()> {
    let data_dir = crate::settings::app_data_dir().join(MUSE_DATA_DIR);
    stop_with(&data_dir, kill_orphan_sidecars, kill_orphan_edges)
}

fn stop_with<F1, F2>(
    data_dir: &std::path::Path,
    kill_sidecars: F1,
    kill_edges: F2,
) -> AppResult<()>
where
    F1: FnOnce(),
    F2: FnOnce(&std::path::Path),
{
    START_CANCELLED.store(true, std::sync::atomic::Ordering::SeqCst);
    let _spawn_guard = start_lock().lock();
    let handle = sidecar_cell().write().take();
    if let Some(mut h) = handle {
        kill_child_tree(&mut h.child);
        kill_sidecars();
        kill_edges(data_dir);
        log_info("VideoFlow Muse engine đã dừng");
    } else {
        kill_sidecars();
        kill_edges(data_dir);
        log_info("VideoFlow Muse engine không có handle — đã dọn tiến trình mồ côi");
    }
    START_CANCELLED.store(false, std::sync::atomic::Ordering::SeqCst);
    Ok(())
}

/// Dừng sidecar khi app thoát — bỏ qua lỗi (không được cản việc quit).
pub fn stop_for_quit() {
    let _ = stop();
}

/// Đọc trạng thái sidecar: process còn sống + snapshot /admin/status khi gọi được.
pub fn status(live: &Settings) -> AppResult<SidecarStatus> {
    let session = current_session();
    let Some(session) = session else {
        return Ok(SidecarStatus {
            running: false,
            port: None,
            version: None,
            driver: None,
            account_total: None,
            account_available: None,
            cfg_port: None,
            cfg_chromium: None,
            cfg_headless: None,
            restart_required: false,
        });
    };
    // Process đã chết nhưng state chưa dọn → dọn.
    let spawn_cfg = {
        let mut guard = sidecar_cell().write();
        match guard.as_mut() {
            Some(h) => match h.child.try_wait() {
                Ok(Some(_)) => {
                    *guard = None;
                    None
                }
                Ok(None) => Some((h.cfg_port, h.cfg_chromium.clone(), h.cfg_headless)),
                Err(_) => None,
            },
            None => None,
        }
    };
    let Some((cfg_port, cfg_chromium, cfg_headless)) = spawn_cfg else {
        return Ok(SidecarStatus {
            running: false,
            port: None,
            version: None,
            driver: None,
            account_total: None,
            account_available: None,
            cfg_port: None,
            cfg_chromium: None,
            cfg_headless: None,
            restart_required: false,
        });
    };
    // Best-effort fetch /admin/status: sidecar vừa spawn có thể chưa sẵn sàng middleware.
    let detail = admin_get(session.port, &session.api_key, "/admin/status").ok();
    // Cài đặt hiện tại khác cấu hình lúc spawn → cần Khởi động lại mới có hiệu lực.
    let restart_required = live.muse_sidecar_port != cfg_port
        || live.muse_chromium_path.trim() != cfg_chromium
        || live.muse_headless != cfg_headless;
    Ok(SidecarStatus {
        running: true,
        port: Some(session.port),
        version: detail
            .as_ref()
            .and_then(|d| d.get("version"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        driver: detail
            .as_ref()
            .and_then(|d| d.get("driver"))
            .and_then(|v| v.get("driver"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        account_total: detail.as_ref().and_then(|d| d.get("accounts")).and_then(|a| a.get("total")).and_then(|v| v.as_u64()).map(|v| v as u32),
        account_available: detail
            .as_ref()
            .and_then(|d| d.get("accounts"))
            .and_then(|a| a.get("available"))
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
        cfg_port: Some(cfg_port),
        cfg_chromium: Some(cfg_chromium),
        cfg_headless: Some(cfg_headless),
        restart_required,
    })
}

/// Ghi một dòng vào file log sidecar trong thư mục dữ liệu app (fire-and-forget).
/// Dùng file thay vì bảng `logs` SQLite vì log sidecar không thuộc về một dự án nào.
pub fn log_app_event(msg: &str) {
    log_info(msg);
}

fn log_info(msg: &str) {
    let dir = crate::settings::app_data_dir().join(MUSE_DATA_DIR);
    std::fs::create_dir_all(&dir).ok();
    let ts = chrono::Local::now().to_rfc3339();
    let safe = crate::settings::redact_secrets(msg);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("engine-app.log"))
    {
        let _ = writeln!(f, "[{ts}] {safe}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_SIDECAR_LOCK: OnceLock<parking_lot::Mutex<()>> = OnceLock::new();
    fn test_sidecar_guard() -> parking_lot::MutexGuard<'static, ()> {
        TEST_SIDECAR_LOCK
            .get_or_init(|| parking_lot::Mutex::new(()))
            .lock()
    }

    #[test]
    fn api_key_dinh_dang_vf_hex() {
        let k = generate_api_key();
        assert!(k.starts_with("vf_"));
        assert_eq!(k.len(), 3 + 32);
        assert!(k.chars().skip(3).all(|c| c.is_ascii_hexdigit()));
        assert_ne!(generate_api_key(), generate_api_key());
    }

    #[test]
    fn detect_chromium_override_khong_ton_tai() {
        let err = detect_chromium("C:/khong-ton-tai/browser.exe").unwrap_err();
        assert_eq!(err.code, "MUSE_CHROMIUM_NOT_FOUND");
    }

    #[test]
    fn find_free_port_khong_trung() {
        // Chiếm một listener ở port bất kỳ, preferred bị bận → phải trả port khác hợp lệ.
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let busy = listener.local_addr().unwrap().port();
        let chosen = find_free_port(busy).expect("phải chọn được port rảnh");
        assert!(chosen > 0);
        assert_ne!(chosen, busy, "port đang bị giữ phải được đổi sang port rảnh khác");

        // Khi exclude trùng với preferred (ngay cả khi preferred đang rảnh), cũng phải chọn port khác.
        let probe = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let free_port = probe.local_addr().unwrap().port();
        drop(probe);
        let excluded = find_free_port_excluding(free_port, Some(free_port))
            .expect("phải tránh port nằm trong exclude");
        assert_ne!(excluded, free_port);
    }

    /// Chạy khi có exe engine cạnh app (dev: `binaries/`) — CI release build exe trước
    /// rồi chạy test này, nên đường spawn thật không còn bị bỏ qua.
    /// Bỏ qua (skip, không fail) khi thiếu exe để `cargo test` trên máy dev trống vẫn xanh.
    #[test]
    fn spawn_sidecar_that_roi_dung_lai_duoc() {
        let _serial = test_sidecar_guard();
        // CI verify job chỉ tạo stub rỗng để build script hài lòng; engine thật do job
        // `build` tạo ra và được smoke test riêng. Bỏ qua khi exe không phải engine thật.
        let Ok(exe) = sidecar_exe_path() else {
            eprintln!("skip: chưa build engine exe (binaries/videoflow-muse-*.exe)");
            return;
        };
        let size = std::fs::metadata(&exe).map(|m| m.len()).unwrap_or(0);
        if size < 8 * 1024 * 1024 {
            eprintln!("skip: {} là stub rỗng ({size} bytes)", exe.display());
            return;
        }
        let cfg = Settings {
            // headless + Edge mặc định; engine tự dò Edge/Chrome trên máy.
            muse_headless: true,
            ..Settings::default()
        };
        let st = start(&cfg).expect("spawn sidecar");
        assert!(st.running);
        assert!(st.port.is_some());
        assert!(!st.restart_required, "vừa spawn thì không được báo cần restart");
        let after = status(&cfg).expect("status");
        assert_eq!(after.driver.as_deref(), Some("browser"));

        // Đổi cấu hình (headless) khi engine đang chạy → phải báo cần khởi động lại.
        let changed = Settings {
            muse_headless: false,
            ..cfg.clone()
        };
        assert!(
            status(&changed).unwrap().restart_required,
            "đổi headless phải khiến status báo restart_required"
        );

        stop().expect("stop sidecar");
        assert!(!status(&cfg).unwrap().running);
    }

    #[test]
    fn open_login_va_view_offline_tra_loi_unreachable() {
        let err_login = open_login(65530, "vf_test", "acc_123").unwrap_err();
        assert_eq!(err_login.code, "MUSE_ADMIN_UNREACHABLE");

        let err_view = open_view(65530, "vf_test", "acc_123").unwrap_err();
        assert_eq!(err_view.code, "MUSE_ADMIN_UNREACHABLE");
    }

    #[test]
    fn start_khi_da_chay_khong_deadlock() {
        let _serial = test_sidecar_guard();
        // Giả lập sidecar_cell() đã có handle rồi gọi start(&cfg): trước bản sửa #28,
        // start() giữ read lock rồi gọi status() (cần write lock) -> self-deadlock vĩnh viễn.
        #[cfg(target_os = "windows")]
        let child = Command::new("cmd")
            .args(["/C", "exit", "0"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn dummy child");
        #[cfg(not(target_os = "windows"))]
        let child = Command::new("true")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn dummy child");

        {
            let mut guard = sidecar_cell().write();
            *guard = Some(SidecarHandle {
                child,
                port: 65531,
                api_key: "vf_test_deadlock".to_string(),
                cfg_port: 65531,
                cfg_chromium: String::new(),
                cfg_headless: true,
            });
        }

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let res = start(&Settings::default());
            let _ = tx.send(res);
        });

        let result = rx.recv_timeout(Duration::from_secs(5));
        // Dọn dẹp handle nếu còn để không ảnh hưởng test khác.
        if let Some(mut h) = sidecar_cell().write().take() {
            let _ = h.child.kill();
            let _ = h.child.wait();
        }
        assert!(
            result.is_ok(),
            "start() khi sidecar_cell() đang Some bị deadlock quá 5 giây!"
        );
    }

    #[test]
    fn sync_proxies_to_sidecar_updates_assigned_clears_unassigned_and_skips_busy() {
        use std::io::{Read, Write};
        use std::sync::{Arc, Mutex};

        let tmp_dir = std::env::temp_dir().join(format!(
            "vf_sync_proxies_{}_{}",
            std::process::id(),
            chrono::Local::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        let _ = std::fs::create_dir_all(&tmp_dir);
        let db_path = tmp_dir.join("proxies.db");
        let conn = crate::proxies::open_db_at(&db_path).expect("open temp proxies.db");

        let mk_proxy = |name: &str, host: &str, port: u16, user: &str, pass: &str| {
            let input: crate::proxies::ProxyInput = serde_json::from_value(serde_json::json!({
                "name": name,
                "kind": "http",
                "host": host,
                "port": port,
                "username": user,
                "password": pass,
                "note": null
            }))
            .expect("deserialize ProxyInput");
            crate::proxies::create_proxy_in(&conn, &input).expect("create proxy")
        };

        let p1 = mk_proxy("P1", "10.0.0.1", 8080, "u1", "new_pass");
        let p3 = mk_proxy("P3", "10.0.0.3", 8080, "u3", "p3");
        let p4 = mk_proxy("P4", "10.0.0.4", 8080, "u4", "p4");
        let p5 = mk_proxy("P5", "10.0.0.5", 8080, "u5", "p5");

        crate::proxies::set_account_proxy_in(&conn, "acc_assigned", Some(&p1.id)).unwrap();
        crate::proxies::set_account_proxy_in(&conn, "acc_busy", Some(&p3.id)).unwrap();
        crate::proxies::set_account_proxy_in(&conn, "acc_409", Some(&p4.id)).unwrap();
        crate::proxies::set_account_proxy_in(&conn, "acc_after_409", Some(&p5.id)).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock sidecar");
        let port = listener.local_addr().unwrap().port();
        let recorded_patches: Arc<Mutex<Vec<(String, serde_json::Value)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let patches_clone = Arc::clone(&recorded_patches);

        // Expect 1 GET /admin/accounts + 4 PATCHes (acc_assigned, acc_unassigned, acc_409, acc_after_409).
        let server = std::thread::spawn(move || {
            for _ in 0..5 {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req_str = String::from_utf8_lossy(&buf[..n]);
                let first_line = req_str.lines().next().unwrap_or("");
                let mut parts = first_line.split_whitespace();
                let method = parts.next().unwrap_or("");
                let path = parts.next().unwrap_or("");

                if method == "GET" && path == "/admin/accounts" {
                    let body = serde_json::json!({
                        "data": [
                            { "id": "acc_assigned", "busy": false, "proxy": "http://***@10.0.0.1:8080" },
                            { "id": "acc_unassigned", "busy": false, "proxy": "http://***@10.0.0.2:8080" },
                            { "id": "acc_busy", "busy": true, "proxy": "http://***@10.0.0.3:8080" },
                            { "id": "acc_409", "busy": false, "proxy": "http://***@10.0.0.4:8080" },
                            { "id": "acc_after_409", "busy": false, "proxy": "http://***@10.0.0.5:8080" }
                        ]
                    })
                    .to_string();
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes());
                } else if method == "PATCH" {
                    let body_str = req_str.split("\r\n\r\n").nth(1).unwrap_or("{}");
                    let body_json: serde_json::Value =
                        serde_json::from_str(body_str.trim()).unwrap_or(serde_json::Value::Null);
                    patches_clone
                        .lock()
                        .unwrap()
                        .push((path.to_string(), body_json));

                    let (status_line, resp_body) = if path == "/admin/accounts/acc_409" {
                        (
                            "409 Conflict",
                            r#"{"error":{"code":"account_busy","message":"Account is busy"}}"#,
                        )
                    } else {
                        ("200 OK", r#"{"ok":true}"#)
                    };
                    let resp = format!(
                        "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        resp_body.len(),
                        resp_body
                    );
                    let _ = stream.write_all(resp.as_bytes());
                }
            }
        });

        sync_proxies_to_sidecar_with_conn(port, "vf_test_sync", &conn);
        server.join().expect("mock sidecar thread");

        let patches = recorded_patches.lock().unwrap();
        assert_eq!(patches.len(), 4, "busy account must be skipped; all 4 non-busy must be patched");
        assert!(patches.iter().any(|(p, b)| {
            p == "/admin/accounts/acc_assigned"
                && b.get("proxy").and_then(|v| v.as_str()) == Some("http://u1:new_pass@10.0.0.1:8080")
        }));
        assert!(patches.iter().any(|(p, b)| {
            p == "/admin/accounts/acc_unassigned"
                && b.get("proxy").and_then(|v| v.as_str()) == Some("")
        }));
        assert!(!patches.iter().any(|(p, _)| p.contains("acc_busy")));
        assert!(patches.iter().any(|(p, _)| p == "/admin/accounts/acc_409"));
        assert!(patches.iter().any(|(p, b)| {
            p == "/admin/accounts/acc_after_409"
                && b.get("proxy").and_then(|v| v.as_str()) == Some("http://u5:p5@10.0.0.5:8080")
        }));

        drop(conn);
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn engine_error_hint_scans_profile_subdirs_and_redacts() {
        crate::settings::register_secret_for_redaction("hint_secret_tok_987");
        let tmp_dir = std::env::temp_dir().join(format!(
            "vf_hint_test_{}_{}",
            std::process::id(),
            chrono::Local::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        let bp_dir = tmp_dir.join("browser-profile");
        let acc_dir = tmp_dir.join("profiles").join("acc_1");
        std::fs::create_dir_all(&bp_dir).unwrap();
        std::fs::create_dir_all(&acc_dir).unwrap();

        let long_pad = "x".repeat(400);
        std::fs::write(
            bp_dir.join("chromium-stderr.log"),
            format!("INFO startup\nERROR: failed with Bearer hint_secret_tok_987 {long_pad}\n"),
        )
        .unwrap();
        std::fs::write(
            acc_dir.join("chromium-stderr.log"),
            "DevTools listening failed: sk-hint_sk_123\n",
        )
        .unwrap();
        std::fs::write(
            tmp_dir.join("engine-stderr.log"),
            "RuntimeError: boom hint_secret_tok_987\n",
        )
        .unwrap();

        let hinted = AppError::new("MUSE_SIDECAR_EXITED", "exit 3").with_engine_hint(&tmp_dir);
        assert!(hinted.message.contains("browser-profile/chromium-stderr.log:"));
        assert!(hinted.message.contains("profiles/acc_1/chromium-stderr.log:"));
        assert!(hinted.message.contains("engine-stderr.log:"));
        assert!(!hinted.message.contains("hint_secret_tok_987"));
        assert!(!hinted.message.contains("hint_sk_123"));
        assert!(hinted.message.contains("[REDACTED]"));

        // Ensure per-file snippet is capped at 300 chars.
        let bp_seg = hinted
            .message
            .split(" — ")
            .find(|s| s.starts_with("browser-profile/chromium-stderr.log: "))
            .expect("browser-profile hint segment");
        let bp_body = bp_seg.trim_start_matches("browser-profile/chromium-stderr.log: ");
        assert!(bp_body.chars().count() <= 300);

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn stop_with_none_handle_runs_orphan_cleanup() {
        let _serial = test_sidecar_guard();
        if let Some(mut h) = sidecar_cell().write().take() {
            let _ = h.child.kill();
            let _ = h.child.wait();
        }
        let temp_data_dir = std::env::temp_dir().join("vf_stop_none_handle_test");
        let mut sidecars_called = false;
        let mut edges_called = false;
        let mut seen_dir: Option<PathBuf> = None;

        stop_with(
            &temp_data_dir,
            || {
                sidecars_called = true;
            },
            |d| {
                edges_called = true;
                seen_dir = Some(d.to_path_buf());
            },
        )
        .expect("stop_with succeeds");

        assert!(sidecars_called, "kill_orphan_sidecars must run when handle is None");
        assert!(edges_called, "kill_orphan_edges must run when handle is None");
        assert_eq!(seen_dir.as_deref(), Some(temp_data_dir.as_path()));
    }

    #[test]
    fn find_free_port_excluding_any_rotates_past_failed_ports() {
        let l1 = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let p1 = l1.local_addr().unwrap().port();
        drop(l1);

        let l2 = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let p2 = l2.local_addr().unwrap().port();
        drop(l2);

        let chosen = find_free_port_excluding_any(p1, &[p1, p2])
            .expect("must pick a free port excluding failed ports");
        assert!(chosen > 0);
        assert_ne!(chosen, p1);
        assert_ne!(chosen, p2);
    }

    #[test]
    fn pass2_redact_before_truncate_wait_cancel_and_best_effort_sync() {
        let _serial = test_sidecar_guard();
        // 1. Long error with secret ending PAST char 300 must still be redacted cleanly
        let tmp = std::env::temp_dir().join(format!("vf_redact_trunc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let pad = "a".repeat(285);
        std::fs::write(
            tmp.join("stderr.log"),
            format!("ERROR: {pad} http://u:p@1.2.3.4:8080/longpath\n"),
        )
        .unwrap();
        let line = extract_interesting_stderr_line(&tmp.join("stderr.log")).unwrap();
        assert!(!line.contains("u:p@"), "secret before truncate leaked: {line}");
        let _ = std::fs::remove_dir_all(&tmp);

        // 2. START_CANCELLED aborts wait_health immediately with MUSE_START_CANCELLED
        let mut child = Command::new(windows_system32_exe("cmd.exe"))
            .args(["/C", "timeout /T 1 /NOBREAK >NUL"])
            .spawn()
            .unwrap();
        START_CANCELLED.store(true, std::sync::atomic::Ordering::SeqCst);
        let cancel_err = wait_health(1, &mut child, 30).expect_err("must cancel");
        START_CANCELLED.store(false, std::sync::atomic::Ordering::SeqCst);
        kill_child_tree(&mut child);
        assert_eq!(cancel_err.code, "MUSE_START_CANCELLED");

        // 3. sync_proxies_to_sidecar_with_conn keeps syncing healthy accounts when one proxy is corrupt
        let db_path = std::env::temp_dir().join(format!("vf_sync_best_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&db_path);
        let conn = crate::proxies::open_db_at(&db_path).unwrap();
        let px_ok = crate::proxies::create_proxy_in(
            &conn,
            &crate::proxies::ProxyInput {
                name: "OK".into(),
                kind: "http".into(),
                host: "127.0.0.1".into(),
                port: 8888,
                username: None,
                password: None,
                clear_password: None,
                note: None,
            },
        )
        .unwrap();
        conn.execute(
            "INSERT INTO proxies (id, name, kind, host, port, username, password, created_at, updated_at)
             VALUES ('px_corrupt', 'Bad', 'http', '127.0.0.2', 8889, 'u', 'enc:v1:corrupt', '2026-01-01', '2026-01-01')",
            [],
        )
        .unwrap();
        crate::proxies::set_account_proxy_in(&conn, "acc_ok", Some(&px_ok.id)).unwrap();
        crate::proxies::set_account_proxy_in(&conn, "acc_corrupt", Some("px_corrupt")).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let patched_ids = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
        let patched_bg = patched_ids.clone();
        let server = std::thread::spawn(move || {
            use std::io::{Read, Write};
            // 1 GET + 1 PATCH
            for _ in 0..2 {
                if let Ok((mut s, _)) = listener.accept() {
                    let mut buf = [0u8; 4096];
                    let n = s.read(&mut buf).unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    if req.starts_with("GET /admin/accounts ") {
                        let body = r#"{"data":[{"id":"acc_ok","busy":false},{"id":"acc_corrupt","busy":false,"proxy":"http://***@127.0.0.2:8889"}]}"#;
                        let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                        let _ = s.write_all(resp.as_bytes());
                    } else if let Some(line) = req.lines().next() {
                        patched_bg.lock().push(line.to_string());
                        let body = r#"{"ok":true}"#;
                        let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                        let _ = s.write_all(resp.as_bytes());
                    }
                }
            }
        });

        sync_proxies_to_sidecar_with_conn(port, "k", &conn);
        server.join().unwrap();
        let reqs = patched_ids.lock().clone();
        assert_eq!(reqs.len(), 1);
        assert!(reqs[0].contains("/admin/accounts/acc_ok"));
        let _ = std::fs::remove_file(&db_path);
    }
}
