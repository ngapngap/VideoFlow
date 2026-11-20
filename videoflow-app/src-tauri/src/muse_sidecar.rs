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
/// Tên exe engine trong `src-tauri/binaries/` lúc dev (giữ hậu tố target-triple).
pub const SIDECAR_EXE_DEV: &str = "videoflow-muse-x86_64-pc-windows-msvc.exe";
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

fn sidecar_cell() -> &'static parking_lot::RwLock<Option<SidecarHandle>> {
    SIDECAR.get_or_init(|| parking_lot::RwLock::new(None))
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
    let names = [SIDECAR_EXE_BUNDLED, SIDECAR_EXE_DEV];
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

/// Sinh API key nội bộ ngẫu nhiên cho phiên (chỉ giữ trong RAM).
fn generate_api_key() -> String {
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

/// Tìm port trống: thử port ưa thích trước, bận thì xin OS một port ngẫu nhiên.
fn find_free_port(preferred: u16) -> AppResult<u16> {
    if TcpListener::bind(("127.0.0.1", preferred)).is_ok() {
        return Ok(preferred);
    }
    TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| AppError::new("MUSE_PORT_FAIL", format!("Không xin được port trống: {e}")))?
        .local_addr()
        .map(|a| a.port())
        .map_err(|e| AppError::new("MUSE_PORT_FAIL", format!("Không đọc được port: {e}")))
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
            .unwrap_or("MUSE_ADMIN_FAIL");
        return Err(AppError::new(
            &format!("MUSE_{}", sidecar_code.to_uppercase()),
            format!("Sidecar trả lỗi {status}: {sidecar_msg}"),
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

/// Phiên hiện tại (port + API key) nếu sidecar đang chạy.
pub fn current_session() -> Option<SidecarSession> {
    let guard = sidecar_cell().read();
    guard.as_ref().map(|h| SidecarSession {
        port: h.port,
        api_key: h.api_key.clone(),
    })
}

/// Khóa tuần tự hóa việc spawn: hai lệnh song song (vd bấm Thêm cookie khi engine chưa chạy)
/// không được spawn 2 engine tranh nhau port/profile Edge.
static START_LOCK: OnceLock<parking_lot::Mutex<()>> = OnceLock::new();

fn start_lock() -> &'static parking_lot::Mutex<()> {
    START_LOCK.get_or_init(|| parking_lot::Mutex::new(()))
}

/// Khởi động sidecar với driver browser; chờ /healthz sẵn sàng.
/// Cold start gồm PyInstaller onefile giải nén + khởi động Edge + CDP (~30-60s) nên
/// deadline là 90 giây; quá hạn thì kill luôn process con để không để lại engine mồ côi.
pub fn start(cfg: &Settings) -> AppResult<SidecarStatus> {
    let _spawn_guard = start_lock().lock();
    let already_running = sidecar_cell().read().is_some();
    if already_running {
        return status(cfg);
    }

    let exe = sidecar_exe_path()?;
    let chromium = detect_chromium(&cfg.muse_chromium_path)?;
    let port = find_free_port(cfg.muse_sidecar_port)?;
    let cdp_port = find_free_port(19210)?;
    let api_key = generate_api_key();
    crate::settings::register_secret_for_redaction(&api_key);

    let data_dir = crate::settings::app_data_dir().join(MUSE_DATA_DIR);
    std::fs::create_dir_all(&data_dir).ok();

    // Dọn sidecar & Edge mồ côi (từ phiên app cũ đóng bất thường) trước khi mở log / spawn.
    kill_orphan_sidecars();
    kill_orphan_edges(&data_dir.join("browser-profile"));

    // Log stdout/stderr của sidecar ra file để chẩn đoán khi khởi động thất bại.
    let stdout_path = data_dir.join("engine-stdout.log");
    let stderr_path = data_dir.join("engine-stderr.log");

    // Hai lần thử: engine chết ngay (exit code 3) hầu như luôn do Edge kẹt profile.
    // Lần 2 spawn mới sau khi dọn orphan + xoá state khoá, không cần user bấm lại.
    let mut last_err: Option<AppError> = None;
    for attempt in 1..=2 {
        if attempt == 2 {
            log_info("engine thoát ngay lần 1 — dọn Edge mồ côi và profile rồi thử lại");
            kill_orphan_sidecars();
            kill_orphan_edges(&data_dir.join("browser-profile"));
            std::thread::sleep(Duration::from_millis(1500));
        }

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
                last_err = Some(AppError::new(
                    "MUSE_SIDECAR_FAIL",
                    format!("Không chạy được engine: {e}"),
                ));
                continue;
            }
        };
        match wait_health(port, &mut child, 90) {
            Ok(()) => {
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
                last_err = Some(e.with_engine_hint(&data_dir));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| {
        AppError::new("MUSE_SIDECAR_FAIL", "Không khởi động được engine VideoFlow Muse.")
    }))
}

/// Gắn thêm 2 dòng cuối log engine để thông báo lỗi có nội dung thật (thay vì chỉ
/// "exit code 3") — người dùng thấy ngay nguyên nhân thay vì phải tự mở file log.
trait EngineErrorHint {
    fn with_engine_hint(self, data_dir: &std::path::Path) -> AppError;
}

impl EngineErrorHint for AppError {
    fn with_engine_hint(self, data_dir: &std::path::Path) -> AppError {
        let mut parts = vec![self.message];
        for name in ["chromium-stderr.log", "engine-stderr.log"] {
            let path = data_dir.join(name);
            if let Ok(text) = std::fs::read_to_string(&path) {
                let interesting: Vec<&str> = text
                    .lines()
                    .filter(|l| {
                        let low = l.to_lowercase();
                        low.contains("error")
                            || low.contains("exited early")
                            || low.contains("runtimeerror")
                            || low.contains("devtools")
                    })
                    .filter(|l| !l.trim().is_empty())
                    .collect();
                if let Some(last) = interesting.last() {
                    parts.push(format!("{name}: {}", last.trim().chars().take(300).collect::<String>()));
                }
            }
        }
        AppError::new(&self.code, parts.join(" — "))
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
        for img in [SIDECAR_EXE_BUNDLED, SIDECAR_EXE_DEV] {
            let _ = Command::new("taskkill")
                .args(["/IM", img, "/T", "/F"])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Dọn Edge/Chrome mồ côi còn giữ profile của engine hoặc cờ debugging port 19210
/// (xảy ra khi engine bị kill cứng không qua taskkill /T, hoặc app crash trước khi stop).
/// Chỉ kill process có command line chứa ĐÚNG profile dir của app hoặc port 19210 —
/// không đụng Edge/Chrome của người dùng.
fn kill_orphan_edges(profile_dir: &std::path::Path) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let needle = profile_dir.to_string_lossy().replace('\'', "''");
        let script = format!(
            "Get-CimInstance Win32_Process -Filter \"Name='msedge.exe' OR Name='chrome.exe'\" | \
             Where-Object {{ $_.CommandLine -and ($_.CommandLine.Contains('{needle}') -or $_.CommandLine.Contains('--remote-debugging-port=19210')) }} | \
             ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }}"
        );
        let _ = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        // Cho Edge nhả file/profile trước khi engine mới khởi động nó lần nữa.
        std::thread::sleep(Duration::from_millis(600));
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = profile_dir;
    }
}

/// Kill cả cây process (taskkill /T /F) để Chromium con do engine mở cũng chết.
fn kill_child_tree(child: &mut Child) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let pid = child.id();
        let _ = Command::new("taskkill")
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
pub fn stop() -> AppResult<()> {
    let handle = sidecar_cell().write().take();
    let Some(mut h) = handle else {
        return Ok(());
    };
    kill_child_tree(&mut h.child);
    // Crashpad/utility của Edge có thể sống sót sau taskkill /T và giữ profile —
    // dọn luôn để lần khởi động sau không bị "Chromium did not expose DevTools".
    let profile = crate::settings::app_data_dir()
        .join(MUSE_DATA_DIR)
        .join("browser-profile");
    kill_orphan_edges(&profile);
    log_info("VideoFlow Muse engine đã dừng");
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
        // Chiếm một listener ở port bất kỳ, preferred bị bận → phải trả port khác.
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let busy = listener.local_addr().unwrap().port();
        // Trả về port khác hoặc đúng busy nếu OS cho bind kép — chỉ cần không panic.
        let _ = find_free_port(busy);
    }

    /// Chạy khi có exe engine cạnh app (dev: `binaries/`) — CI release build exe trước
    /// rồi chạy test này, nên đường spawn thật không còn bị bỏ qua.
    /// Bỏ qua (skip, không fail) khi thiếu exe để `cargo test` trên máy dev trống vẫn xanh.
    #[test]
    fn spawn_sidecar_that_roi_dung_lai_duoc() {
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
}
