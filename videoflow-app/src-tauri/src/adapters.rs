//! Adapter kết nối ngoài (Lớp 3 - mục 3):
//! - Image: multipart/form-data tới POST /v1/images/edits, nhận b64_json (5.5)
//! - Video: data URL base64 ở cả image và input_reference tới POST /v1/videos,
//!   poll GET /v1/videos/{task_id} theo 13.3 + 2 quy tắc Phụ lục v1.3.1,
//!   tải file theo 13.6 (stream ra .part, kiểm tra Content-Length + MP4 box header,
//!   chỉ gửi Bearer khi cùng origin, chỉ retry download, không tạo task mới).

use crate::error::{AppError, AppResult};
use crate::settings::Settings;
use base64::Engine as _;
use image::ImageEncoder as _;
use serde_json::Value;
use std::collections::HashMap;
use std::io::{Cursor, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const MAX_VIDEO_INPUT_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const DOWNLOAD_BACKOFF_S: [u64; 3] = [2, 5, 10];

static HTTP_CLIENTS: OnceLock<
    parking_lot::Mutex<HashMap<(u64, bool), reqwest::blocking::Client>>,
> = OnceLock::new();

#[cfg(test)]
static TEST_API_KEYS: OnceLock<parking_lot::Mutex<HashMap<String, String>>> = OnceLock::new();

#[cfg(test)]
pub(crate) fn register_test_api_key(base_url: &str, key: &str) {
    let map = TEST_API_KEYS.get_or_init(|| parking_lot::Mutex::new(HashMap::new()));
    map.lock().insert(
        base_url.trim().trim_end_matches('/').to_string(),
        key.to_string(),
    );
}

fn active_api_key(base_url: &str, which: &str) -> String {
    #[cfg(test)]
    {
        if let Some(map) = TEST_API_KEYS.get() {
            if let Some(k) = map.lock().get(base_url.trim().trim_end_matches('/')) {
                return k.clone();
            }
        }
    }
    #[cfg(not(test))]
    let _ = base_url;
    crate::settings::get_api_key(which).unwrap_or_default()
}

/// Resolve base URL + key + model cho nguồn video theo `video_source`:
/// - `"internal"`: sidecar muse2api nội bộ `http://127.0.0.1:{port}/v1`, key phiên,
///   model cố định `"muse-video"` (muse2api tự resolve alias).
///   Sidecar chưa chạy thì tự spawn (lazy). Lỗi spawn → ApiFail NghiepVu (không
///   retry — lỗi cấu hình/môi trường, người dùng phải sửa xong mới chạy lại).
/// - Mặc định `"gateway"`: giữ nguyên endpoint ngoài đã lưu + model đã cấu hình + key keyring.
fn resolve_video_base_and_key(cfg: &Settings) -> Result<(String, String, String), ApiFail> {
    resolve_video_base_and_key_with(cfg, crate::muse_sidecar::ensure_running)
}

fn resolve_video_base_and_key_with<F>(
    cfg: &Settings,
    ensure_sidecar: F,
) -> Result<(String, String, String), ApiFail>
where
    F: FnOnce(&Settings) -> AppResult<crate::muse_sidecar::SidecarSession>,
{
    if cfg.video_source != "internal" {
        let base = cfg.endpoint_video_base_url.trim().trim_end_matches('/').to_string();
        let key = active_api_key(&base, "video");
        return Ok((base, key, cfg.endpoint_video_model.trim().to_string()));
    }
    match ensure_sidecar(cfg) {
        Ok(session) => Ok((session.v1_base(), session.api_key, "muse-video".to_string())),
        Err(e) => Err(ApiFail {
            kind: ApiFailKind::NghiepVu,
            http_status: None,
            code: "MUSE_SIDECAR_UNAVAILABLE".into(),
            message: crate::settings::redact_secrets(&e.message),
        }),
    }
}

fn is_internal_video_source(cfg: &Settings) -> bool {
    cfg.video_source == "internal"
}

/// Resolve base URL + key + model cho nguồn ảnh:
/// - `"internal"`: engine VideoFlow Muse `http://127.0.0.1:{port}/v1`, key phiên,
///   model cố định `muse-image` (muse2api tự resolve alias).
/// - Mặc định: endpoint gateway ngoài + model đã cấu hình + key keyring.
fn resolve_image_base_and_key(cfg: &Settings) -> Result<(String, String, String), ApiFail> {
    if cfg.image_source != "internal" {
        let base = cfg.endpoint_image_base_url.trim().trim_end_matches('/').to_string();
        let key = active_api_key(&base, "anh");
        return Ok((base, key, cfg.endpoint_image_model.trim().to_string()));
    }
    match crate::muse_sidecar::ensure_running(cfg) {
        Ok(session) => Ok((session.v1_base(), session.api_key, "muse-image".to_string())),
        Err(e) => Err(ApiFail {
            kind: ApiFailKind::NghiepVu,
            http_status: None,
            code: "MUSE_SIDECAR_UNAVAILABLE".into(),
            message: crate::settings::redact_secrets(&e.message),
        }),
    }
}

fn is_loopback_url(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url.trim()) else {
        return false;
    };
    matches!(
        parsed.host_str(),
        Some("127.0.0.1" | "localhost" | "::1" | "[::1]")
    )
}

fn client_for_url(timeout_s: u64, url: &str) -> reqwest::blocking::Client {
    let no_proxy = cfg!(test) || is_loopback_url(url);
    let key = (timeout_s.max(1), no_proxy);
    let cache = HTTP_CLIENTS.get_or_init(|| parking_lot::Mutex::new(HashMap::new()));
    let mut map = cache.lock();
    if let Some(c) = map.get(&key) {
        return c.clone();
    }
    let mut builder = reqwest::blocking::Client::builder().timeout(Duration::from_secs(key.0));
    if no_proxy {
        builder = builder.no_proxy();
    }
    let built = builder.build().unwrap_or_else(|_| {
        reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(key.0))
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new())
    });
    map.insert(key, built.clone());
    built
}

/// Ngủ có thể ngắt sớm mỗi 200ms khi người dùng bấm STOP (`stop_flag == true`).
/// Trả về `true` nếu bị ngắt bởi `stop_flag`.
pub fn sleep_interruptible(dur: Duration, stop_flag: &AtomicBool) -> bool {
    let start = Instant::now();
    let tick = Duration::from_millis(200);
    while start.elapsed() < dur {
        if stop_flag.load(Ordering::SeqCst) {
            return true;
        }
        let remain = dur.saturating_sub(start.elapsed());
        std::thread::sleep(remain.min(tick));
    }
    stop_flag.load(Ordering::SeqCst)
}

/// Phân loại lỗi theo bảng 13.5
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiFailKind {
    TamThoi { retry_after_s: Option<u64> },
    NghiepVu,
    HetQuota,
}

#[derive(Debug, Clone)]
pub struct ApiFail {
    pub kind: ApiFailKind,
    pub http_status: Option<u16>,
    pub code: String,
    pub message: String,
}

impl ApiFail {
    pub fn stopped(msg: impl Into<String>) -> Self {
        Self {
            kind: ApiFailKind::TamThoi { retry_after_s: None },
            http_status: None,
            code: "STOPPED".into(),
            message: msg.into(),
        }
    }

    pub fn classify_http(
        status: u16,
        headers: Option<&reqwest::header::HeaderMap>,
        body: &str,
    ) -> Self {
        let parsed = serde_json::from_str::<Value>(body).ok();
        let msg = parsed
            .as_ref()
            .and_then(|v| {
                if let Some(err) = v.get("error") {
                    if let Some(s) = err.as_str() {
                        return Some(s.to_string());
                    }
                    if let Some(m) = err.get("message").and_then(|m| m.as_str()) {
                        return Some(m.to_string());
                    }
                    return Some(err.to_string());
                }
                v.get("message")
                    .or_else(|| v.get("detail"))
                    .and_then(|m| m.as_str().map(|s| s.to_string()).or_else(|| Some(m.to_string())))
            })
            .unwrap_or_else(|| body.chars().take(300).collect::<String>().trim().to_string());
        let msg = crate::settings::redact_secrets(&msg);

        let err_code_json = parsed
            .as_ref()
            .and_then(|v| {
                v.pointer("/error/code")
                    .or_else(|| v.pointer("/error/type"))
                    .or_else(|| v.get("code"))
                    .and_then(|c| c.as_str())
            })
            .unwrap_or("");

        // Phát hiện hết quota theo cả dạng gạch dưới lẫn khoảng trắng
        let combined_low = format!("{} {}", err_code_json, body).to_lowercase();
        let is_quota = status == 402 || contains_quota_keyword(&combined_low);
        if is_quota {
            return Self {
                kind: ApiFailKind::HetQuota,
                http_status: Some(status),
                code: "QUOTA_EXHAUSTED".into(),
                message: format!("Hết quota (HTTP {status}): {msg}"),
            };
        }

        // Workspace bị muse.ai cắt (muse2api trả 503 kèm code `upstream_vm_disabled`):
        // retry/fail-over vô nghĩa — phải mở ticket trên muse.ai, xếp NghiepVu.
        if err_code_json == "upstream_vm_disabled"
            || combined_low.contains("vm_state=disabled")
            || combined_low.contains("disconnected this account's workspace")
        {
            return Self {
                kind: ApiFailKind::NghiepVu,
                http_status: Some(status),
                code: "UPSTREAM_VM_DISABLED".into(),
                message: format!("Workspace tài khoản bị muse.ai ngắt kết nối (HTTP {status}): {msg}"),
            };
        }

        // Upstream từ chối do vi phạm chính sách / nội dung (muse2api trả 502 code `upstream_refused`):
        // retry vô nghĩa — xếp NghiepVu để dừng ngay, không retry 3 lần làm kẹt 7.5 phút ([P0-7]).
        if err_code_json == "upstream_refused"
            || combined_low.contains("upstream_refused")
            || combined_low.contains("content policy")
            || combined_low.contains("policy violation")
            || combined_low.contains("vi phạm chính sách")
        {
            return Self {
                kind: ApiFailKind::NghiepVu,
                http_status: Some(status),
                code: "UPSTREAM_REFUSED".into(),
                message: format!("Yêu cầu bị từ chối do vi phạm chính sách/nội dung (HTTP {status}): {msg}"),
            };
        }

        // Muse khóa tính năng tạo video (muse2api trả code `upstream_video_locked` hoặc `video_locked`):
        // retry vô nghĩa — xếp NghiepVu để dừng ngay ([P1-4]).
        if err_code_json == "upstream_video_locked"
            || err_code_json == "video_locked"
            || combined_low.contains("upstream_video_locked")
            || combined_low.contains("video_locked")
            || combined_low.contains("tính năng tạo video không khả dụng")
            || combined_low.contains("video không khả dụng")
            || combined_low.contains("không thể tạo video")
            || combined_low.contains("video generation is not available")
            || combined_low.contains("unable to generate video")
        {
            return Self {
                kind: ApiFailKind::NghiepVu,
                http_status: Some(status),
                code: "VIDEO_LOCKED".into(),
                message: format!("Tính năng tạo video bị khóa trên tài khoản (HTTP {status}): {msg}"),
            };
        }

        let retry_after_s = headers
            .and_then(|h| h.get("retry-after"))
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok());

        let kind = match status {
            408 | 429 | 500..=599 => ApiFailKind::TamThoi { retry_after_s },
            400 | 401 | 403 | 404 | 405 | 413 | 422 => ApiFailKind::NghiepVu,
            400..=499 => ApiFailKind::NghiepVu,
            _ => ApiFailKind::TamThoi { retry_after_s },
        };

        let code = match status {
            400 | 422 => "PROMPT_REJECTED".to_string(),
            401 | 403 => "AUTH_FAIL".to_string(),
            404 => "NOT_FOUND".to_string(),
            405 => "METHOD_NOT_ALLOWED".to_string(),
            413 => "FILE_TOO_LARGE".to_string(),
            _ => format!("HTTP_{status}"),
        };

        let message = match status {
            401 | 403 => format!(
                "Lỗi xác thực HTTP {status} - kiểm tra API key ở Cài đặt. Chi tiết: {msg}"
            ),
            400 | 422 => format!("Yêu cầu/Prompt bị từ chối (HTTP {status}): {msg}"),
            404 => format!("Không tìm thấy đường dẫn API/model (HTTP 404): {msg}"),
            405 => format!("Phương thức HTTP không được hỗ trợ (HTTP 405): {msg}"),
            413 => format!("Dữ liệu gửi lên quá lớn (HTTP 413): {msg}"),
            _ => format!("HTTP {status}: {msg}"),
        };

        Self {
            kind,
            http_status: Some(status),
            code,
            message,
        }
    }

    fn network(e: &reqwest::Error) -> Self {
        Self {
            kind: ApiFailKind::TamThoi { retry_after_s: None },
            http_status: None,
            code: "NETWORK".into(),
            message: crate::settings::redact_secrets(&format!("Lỗi mạng: {e}")),
        }
    }

    fn timeout() -> Self {
        Self {
            kind: ApiFailKind::TamThoi { retry_after_s: None },
            http_status: None,
            code: "TIMEOUT".into(),
            message: "Hết thời gian chờ kết nối/phản hồi".into(),
        }
    }
}

pub fn contains_quota_keyword(low: &str) -> bool {
    [
        "insufficient_quota",
        "upstream_quota_exhausted",
        "quota_exhausted",
        "quota exhausted",
        "quota_exceeded",
        "quota exceeded",
        "exceeded your current quota",
        "billing_hard_limit",
        "billing hard limit",
        "insufficient_balance",
        "insufficient balance",
        "insufficient credits",
        "out_of_credits",
        "out of credits",
    ]
    .iter()
    .any(|kw| low.contains(kw))
}

/// Kiểm tra đoạn định danh (`task_id` / `media_name`) an toàn trước khi ghép vào URL có đính Bearer token.
pub fn sanitize_url_segment(seg: &str, allow_slash: bool) -> Result<String, ApiFail> {
    let trimmed = seg.trim();
    if trimmed.is_empty()
        || trimmed.contains("..")
        || trimmed.contains('?')
        || trimmed.contains('#')
        || trimmed.contains('\\')
        || trimmed.chars().any(|c| c.is_control() || c.is_whitespace())
        || (!allow_slash && trimmed.contains('/'))
    {
        return Err(ApiFail {
            kind: ApiFailKind::NghiepVu,
            http_status: None,
            code: "UNSAFE_URL_SEGMENT".into(),
            message: crate::settings::redact_secrets(&format!(
                "Định danh từ server chứa ký tự không an toàn: '{trimmed}'"
            )),
        });
    }
    Ok(trimmed.to_string())
}

/// Nhận diện MIME type của file ảnh theo magic bytes trước, sau đó theo đuôi file.
pub fn detect_image_mime(bytes: &[u8], path: &Path) -> &'static str {
    if let Ok(fmt) = image::guess_format(bytes) {
        match fmt {
            image::ImageFormat::Png => return "image/png",
            image::ImageFormat::Jpeg => return "image/jpeg",
            image::ImageFormat::WebP => return "image/webp",
            _ => {}
        }
    }
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        _ => "image/png",
    }
}

/// Kiểm tra ảnh giải mã được và luôn chuẩn hóa ra bytes PNG thật cho `{stem}_gen.png`.
pub fn ensure_png_bytes(raw_bytes: Vec<u8>, status: u16) -> Result<Vec<u8>, ApiFail> {
    let guessed = image::guess_format(&raw_bytes).ok();
    let decoded = image::load_from_memory(&raw_bytes).map_err(|_| ApiFail {
        kind: ApiFailKind::NghiepVu,
        http_status: Some(status),
        code: "BAD_IMAGE".into(),
        message: "File ảnh trả về không đọc được (định dạng sai hoặc dữ liệu hỏng)".to_string(),
    })?;

    if guessed == Some(image::ImageFormat::Png) {
        return Ok(raw_bytes);
    }

    let mut png_out = Vec::new();
    decoded
        .write_to(&mut Cursor::new(&mut png_out), image::ImageFormat::Png)
        .map_err(|e| ApiFail {
            kind: ApiFailKind::NghiepVu,
            http_status: Some(status),
            code: "IMAGE_ENCODE_FAIL".into(),
            message: format!("Không chuyển đổi được ảnh sang PNG: {e}"),
        })?;
    Ok(png_out)
}

// ━━━━━━━━━━━━━━━━━━━━━━ B2 - Tạo ảnh (5.1, 5.5) ━━━━━━━━━━━━━━━━━━━━━━

#[derive(Debug)]
pub struct ImageResult {
    pub png_bytes: Vec<u8>,
    pub duration_ms: u128,
}

pub fn create_image(cfg: &Settings, image_path: &Path, prompt: &str) -> Result<ImageResult, ApiFail> {
    let start = Instant::now();
    let file_bytes = std::fs::read(image_path).map_err(|e| ApiFail {
        kind: ApiFailKind::NghiepVu,
        http_status: None,
        code: "REF_IMAGE_READ_FAIL".into(),
        message: format!("Không đọc được ảnh tham chiếu: {e}"),
    })?;

    let mime = detect_image_mime(&file_bytes, image_path);
    let file_name = image_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "reference.png".into());

    let part = reqwest::blocking::multipart::Part::bytes(file_bytes)
        .file_name(file_name)
        .mime_str(mime)
        .map_err(|e| ApiFail {
            kind: ApiFailKind::NghiepVu,
            http_status: None,
            code: "BAD_MIME".into(),
            message: format!("MIME ảnh không hợp lệ: {e}"),
        })?;

    let (base_clean, key, model) = resolve_image_base_and_key(cfg)?;

    let form = reqwest::blocking::multipart::Form::new()
        .text("prompt", prompt.to_string())
        .text("model", model)
        .text("n", "1")
        .text("response_format", "b64_json")
        .part("image", part);

    let url = format!("{base_clean}/images/edits");

    let resp = client_for_url(cfg.image_timeout_s, &url)
        .post(&url)
        .bearer_auth(&key)
        .multipart(form)
        .send()
        .map_err(|e| {
            if e.is_timeout() {
                ApiFail::timeout()
            } else {
                ApiFail::network(&e)
            }
        })?;

    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let body = resp.text().map_err(|e| ApiFail::network(&e))?;

    if !(200..=201).contains(&status) {
        return Err(ApiFail::classify_http(status, Some(&headers), &body));
    }

    let json: Value = serde_json::from_str(&body).map_err(|e| ApiFail {
        kind: ApiFailKind::NghiepVu,
        http_status: Some(status),
        code: "BAD_RESPONSE".into(),
        message: format!("Response tạo ảnh không phải JSON: {e}"),
    })?;

    let b64_raw = json
        .pointer("/data/0/b64_json")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ApiFail {
            kind: ApiFailKind::NghiepVu,
            http_status: Some(status),
            code: "BAD_RESPONSE".into(),
            message: crate::settings::redact_secrets(&format!(
                "Thiếu trường data[0].b64_json trong response: {}",
                body.chars().take(200).collect::<String>()
            )),
        })?;

    let b64_clean = b64_raw
        .split_once(',')
        .map(|(_, data)| data)
        .unwrap_or(b64_raw)
        .trim();

    let raw_bytes = base64::engine::general_purpose::STANDARD
        .decode(b64_clean)
        .map_err(|e| ApiFail {
            kind: ApiFailKind::NghiepVu,
            http_status: Some(status),
            code: "BAD_RESPONSE".into(),
            message: format!("b64_json giải mã lỗi: {e}"),
        })?;

    let png_bytes = ensure_png_bytes(raw_bytes, status)?;

    Ok(ImageResult {
        png_bytes,
        duration_ms: start.elapsed().as_millis(),
    })
}

// ━━━━━━━━━━━━━━━━━━━━━━ B3 - Tạo video (5.1, 5.5, 5.6, 13.3, 13.6) ━━━━━━━━━━━━━━━━━━━━━━

#[derive(Debug)]
pub struct VideoTask {
    pub task_id: String,
    pub reencoded_jpeg: bool,
    pub original_bytes: usize,
}

/// Tạo data URL cho ảnh đầu vào bước tạo video (mục 5.6):
/// Nếu kích thước ảnh `> threshold_bytes` (mặc định 10 MB), nén sang JPEG chất lượng 85 trên bộ nhớ.
/// Trả về `(data_url, reencoded_jpeg)`.
pub fn build_video_image_data_url_info(
    file_bytes: &[u8],
    image_path: &Path,
    threshold_bytes: usize,
) -> Result<(String, bool), ApiFail> {
    if file_bytes.len() > threshold_bytes {
        let decoded = image::load_from_memory(file_bytes).map_err(|e| ApiFail {
            kind: ApiFailKind::NghiepVu,
            http_status: None,
            code: "BAD_IMAGE".into(),
            message: format!("Không đọc được ảnh sản phẩm để nén JPEG: {e}"),
        })?;
        let rgb = decoded.into_rgb8();
        let mut jpeg_buf = Vec::new();
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg_buf, 85);
        encoder
            .write_image(
                rgb.as_raw(),
                rgb.width(),
                rgb.height(),
                image::ExtendedColorType::Rgb8,
            )
            .map_err(|e| ApiFail {
                kind: ApiFailKind::NghiepVu,
                http_status: None,
                code: "IMAGE_ENCODE_FAIL".into(),
                message: format!("Không nén được ảnh sang JPEG chất lượng 85: {e}"),
            })?;
        return Ok((
            format!(
                "data:image/jpeg;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(&jpeg_buf)
            ),
            true,
        ));
    }

    let mime = detect_image_mime(file_bytes, image_path);
    Ok((
        format!(
            "data:{mime};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(file_bytes)
        ),
        false,
    ))
}

pub fn build_video_image_data_url(
    file_bytes: &[u8],
    image_path: &Path,
    threshold_bytes: usize,
) -> Result<String, ApiFail> {
    build_video_image_data_url_info(file_bytes, image_path, threshold_bytes).map(|(u, _)| u)
}

/// Bước 1 của B3: gửi yêu cầu tạo task kèm `Idempotency-Key` (5.6).
pub fn create_video_task(
    cfg: &Settings,
    image_path: &Path,
    prompt: &str,
    idempotency_key: Option<&str>,
) -> Result<VideoTask, ApiFail> {
    create_video_task_inner(
        cfg,
        image_path,
        prompt,
        idempotency_key,
        crate::muse_sidecar::ensure_running,
    )
}

fn create_video_task_inner<F>(
    cfg: &Settings,
    image_path: &Path,
    prompt: &str,
    idempotency_key: Option<&str>,
    ensure_sidecar: F,
) -> Result<VideoTask, ApiFail>
where
    F: FnOnce(&Settings) -> AppResult<crate::muse_sidecar::SidecarSession>,
{
    let file_bytes = std::fs::read(image_path).map_err(|e| ApiFail {
        kind: ApiFailKind::NghiepVu,
        http_status: None,
        code: "IMAGE_READ_FAIL".into(),
        message: format!("Không đọc được ảnh sản phẩm: {e}"),
    })?;
    let original_bytes = file_bytes.len();

    let (data_url, reencoded_jpeg) =
        build_video_image_data_url_info(&file_bytes, image_path, MAX_VIDEO_INPUT_IMAGE_BYTES)?;

    let (base_clean, key, model) = resolve_video_base_and_key_with(cfg, ensure_sidecar)?;
    let url = format!("{base_clean}/videos");

    let payload = serde_json::json!({
        "model": model,
        "prompt": prompt,
        "image": data_url,
        "input_reference": data_url,
        "size": cfg.video_size.trim(),
        "seconds": cfg.video_seconds
    });

    let mut req = client_for_url(cfg.video_create_timeout_s, &url)
        .post(&url)
        .bearer_auth(&key)
        .json(&payload);
    if let Some(idem) = idempotency_key.filter(|s| !s.trim().is_empty()) {
        req = req.header("Idempotency-Key", idem.trim());
    }

    let resp = req.send().map_err(|e| {
        if e.is_timeout() {
            ApiFail::timeout()
        } else {
            ApiFail::network(&e)
        }
    })?;

    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let body = resp.text().map_err(|e| ApiFail::network(&e))?;

    if !(200..=202).contains(&status) {
        return Err(ApiFail::classify_http(status, Some(&headers), &body));
    }

    let json: Value = serde_json::from_str(&body).map_err(|e| ApiFail {
        kind: ApiFailKind::NghiepVu,
        http_status: Some(status),
        code: "BAD_RESPONSE".into(),
        message: format!("Response tạo task không phải JSON: {e}"),
    })?;

    let raw_task_id = ["id", "task_id", "taskId"]
        .iter()
        .find_map(|k| json.get(*k).and_then(|v| v.as_str()))
        .or_else(|| {
            ["/data/id", "/data/task_id", "/data/taskId"]
                .iter()
                .find_map(|p| json.pointer(p).and_then(|v| v.as_str()))
        })
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| ApiFail {
            kind: ApiFailKind::NghiepVu,
            http_status: Some(status),
            code: "BAD_RESPONSE".into(),
            message: crate::settings::redact_secrets(&format!(
                "Không tìm thấy task_id trong response: {}",
                body.chars().take(200).collect::<String>()
            )),
        })?;

    let task_id = sanitize_url_segment(raw_task_id, false)?;

    Ok(VideoTask {
        task_id,
        reencoded_jpeg,
        original_bytes,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollOutcome {
    /// task_id đang sống nhưng chưa xong (running hoặc trạng thái lạ - 13.3)
    StillRunning { progress: Option<u32> },
    Xong {
        video_url: Option<String>,
        media_name: Option<String>,
    },
    Loi {
        message: String,
        kind: ApiFailKind,
        code: String,
    },
    /// GET task trả 404 / task not found (13.6): nhiệm vụ không còn tồn tại
    TaskGone,
}

/// Phân loại lỗi bất đồng bộ khi JSON poll trả về `status: "failed" | "error"`
pub fn classify_poll_task_failure(json: &Value) -> PollOutcome {
    let err_obj = json.get("error");
    let msg = err_obj
        .map(|e| {
            if let Some(s) = e.as_str() {
                s.to_string()
            } else if let Some(m) = e.get("message").and_then(|m| m.as_str()) {
                m.to_string()
            } else {
                e.to_string()
            }
        })
        .unwrap_or_else(|| "Task báo lỗi trên máy chủ".into());
    let msg = crate::settings::redact_secrets(&msg);

    let raw_code = err_obj
        .and_then(|e| {
            e.get("code")
                .or_else(|| e.get("type"))
                .and_then(|c| c.as_str())
        })
        .or_else(|| json.get("error_code").and_then(|c| c.as_str()))
        .unwrap_or("");

    let combined_low = format!("{raw_code} {msg}").to_lowercase();
    if contains_quota_keyword(&combined_low) {
        return PollOutcome::Loi {
            message: format!("Hết quota: {msg}"),
            kind: ApiFailKind::HetQuota,
            code: "QUOTA_EXHAUSTED".into(),
        };
    }

    // Workspace bị muse.ai cắt (vm_state=DISABLED): fail-over/retry vô nghĩa, phải
    // mở ticket trên muse.ai — giữ nguyên message tiếng Anh gốc để copy được hướng dẫn.
    if raw_code == "upstream_vm_disabled"
        || combined_low.contains("vm_state=disabled")
        || combined_low.contains("disconnected this account's workspace")
    {
        return PollOutcome::Loi {
            message: format!("Workspace tài khoản bị muse.ai ngắt kết nối: {msg}"),
            kind: ApiFailKind::NghiepVu,
            code: "UPSTREAM_VM_DISABLED".into(),
        };
    }

    // Pool hết account khả dụng (tất cả đang cooldown/hết quota): lỗi tạm thời —
    // retry theo backoff là hợp lý vì account sẽ hồi phục khi cooldown hết.
    if raw_code == "no_account_available" || combined_low.contains("no account available") {
        return PollOutcome::Loi {
            message: format!("Pool muse2api chưa có account khả dụng: {msg}"),
            kind: ApiFailKind::TamThoi { retry_after_s: None },
            code: "TASK_TRANSIENT_ERROR".into(),
        };
    }

    // Upstream từ chối do vi phạm chính sách / nội dung (muse2api trả code `upstream_refused`):
    // retry vô nghĩa — xếp NghiepVu để dừng ngay, không retry 3 lần làm kẹt 7.5 phút ([P0-7]).
    if raw_code == "upstream_refused"
        || combined_low.contains("upstream_refused")
        || combined_low.contains("content policy")
        || combined_low.contains("policy violation")
        || combined_low.contains("vi phạm chính sách")
    {
        return PollOutcome::Loi {
            message: format!("Yêu cầu bị từ chối do vi phạm chính sách/nội dung: {msg}"),
            kind: ApiFailKind::NghiepVu,
            code: "UPSTREAM_REFUSED".into(),
        };
    }

    // Muse khóa tính năng tạo video (muse2api trả code `upstream_video_locked` hoặc `video_locked`):
    // retry vô nghĩa — xếp NghiepVu để dừng ngay ([P1-4]).
    if raw_code == "upstream_video_locked"
        || raw_code == "video_locked"
        || combined_low.contains("upstream_video_locked")
        || combined_low.contains("video_locked")
        || combined_low.contains("tính năng tạo video không khả dụng")
        || combined_low.contains("video không khả dụng")
        || combined_low.contains("không thể tạo video")
        || combined_low.contains("video generation is not available")
        || combined_low.contains("unable to generate video")
    {
        return PollOutcome::Loi {
            message: format!("Tính năng tạo video bị khóa trên tài khoản: {msg}"),
            kind: ApiFailKind::NghiepVu,
            code: "VIDEO_LOCKED".into(),
        };
    }

    let is_transient = [
        "internal_error",
        "server_error",
        "service_unavailable",
        "overloaded",
        "rate_limit",
        "timeout",
        "timed out",
        "502",
        "503",
        "504",
    ]
    .iter()
    .any(|kw| combined_low.contains(kw));

    if is_transient {
        return PollOutcome::Loi {
            message: msg,
            kind: ApiFailKind::TamThoi { retry_after_s: None },
            code: "TASK_TRANSIENT_ERROR".into(),
        };
    }

    PollOutcome::Loi {
        message: msg,
        kind: ApiFailKind::NghiepVu,
        code: "TASK_FAILED".into(),
    }
}

/// Bước 2 của B3: poll một lần GET /v1/videos/{task_id} theo bảng giá trị 13.3.
pub fn poll_once(cfg: &Settings, task_id: &str) -> Result<PollOutcome, ApiFail> {
    let safe_tid = sanitize_url_segment(task_id, false)?;
    let (base_clean, key, _model) = resolve_video_base_and_key(cfg)?;
    let url = format!("{base_clean}/videos/{safe_tid}");

    let resp = client_for_url(60, &url)
        .get(&url)
        .bearer_auth(&key)
        .send()
        .map_err(|e| {
            if e.is_timeout() {
                ApiFail::timeout()
            } else {
                ApiFail::network(&e)
            }
        })?;

    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let body = resp.text().map_err(|e| ApiFail::network(&e))?;

    if status == 404 {
        return Ok(PollOutcome::TaskGone);
    }
    if status != 200 {
        return Err(ApiFail::classify_http(status, Some(&headers), &body));
    }

    let json: Value = serde_json::from_str(&body).map_err(|e| ApiFail {
        kind: ApiFailKind::NghiepVu,
        http_status: Some(status),
        code: "BAD_RESPONSE".into(),
        message: format!("Poll response không phải JSON: {e}"),
    })?;

    let st = json
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let progress = parse_progress(json.get("progress"));

    match st.as_str() {
        "succeeded" | "completed" | "success" | "done" => Ok(PollOutcome::Xong {
            video_url: extract_video_url(&json),
            media_name: extract_media_name(&json),
        }),
        "failed" | "error" | "cancelled" | "canceled" => Ok(classify_poll_task_failure(&json)),
        _ => Ok(PollOutcome::StillRunning { progress }),
    }
}

fn parse_progress(v: Option<&Value>) -> Option<u32> {
    let v = v?;
    if let Some(n) = v.as_u64() {
        return Some(n.min(100) as u32);
    }
    if let Some(f) = v.as_f64() {
        if f.is_finite() && f >= 0.0 {
            let pct = if f > 0.0 && f <= 1.0 { f * 100.0 } else { f };
            return Some((pct.round() as u32).min(100));
        }
    }
    if let Some(s) = v.as_str() {
        let trimmed = s.trim().trim_end_matches('%').trim();
        if let Ok(n) = trimmed.parse::<f64>() {
            if n.is_finite() && n >= 0.0 {
                return Some((n.round() as u32).min(100));
            }
        }
    }
    None
}

/// Thứ tự thử trường URL theo 13.3
pub fn extract_video_url(json: &Value) -> Option<String> {
    let url_keys = [
        "video_url",
        "videoUrl",
        "url",
        "output",
        "result_url",
        "download_url",
        "file_url",
        "data",
        "result",
        "video",
    ];
    for key in url_keys {
        if let Some(v) = json.get(key) {
            if let Some(found) = extract_url_from_value(v) {
                return Some(found);
            }
        }
    }
    None
}

fn extract_url_from_value(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if s.starts_with("http://") || s.starts_with("https://") => {
            Some(s.clone())
        }
        Value::Array(arr) => {
            for item in arr {
                if let Some(found) = extract_url_from_value(item) {
                    return Some(found);
                }
            }
            None
        }
        Value::Object(o) => {
            for k in ["video_url", "videoUrl", "url", "download_url", "file_url"] {
                if let Some(s) = o.get(k).and_then(|u| u.as_str()) {
                    if s.starts_with("http://") || s.starts_with("https://") {
                        return Some(s.to_string());
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// Trích xuất tên media dự phòng khi response không kèm URL trực tiếp (13.3)
pub fn extract_media_name(json: &Value) -> Option<String> {
    let name_keys = [
        "media_name",
        "mediaName",
        "filename",
        "file_name",
        "fileName",
        "output_name",
        "outputName",
        "media_id",
        "mediaId",
    ];
    for k in name_keys {
        if let Some(s) = json.get(k).and_then(|v| v.as_str()) {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    for container_key in ["output", "data", "result", "video"] {
        if let Some(container) = json.get(container_key) {
            if let Some(s) = container.as_str() {
                let trimmed = s.trim();
                if !trimmed.is_empty() && !trimmed.starts_with("http") {
                    return Some(trimmed.trim_start_matches('/').to_string());
                }
            }
            let obj_opt = container
                .as_object()
                .or_else(|| container.as_array().and_then(|a| a.first()).and_then(|x| x.as_object()));
            if let Some(obj) = obj_opt {
                for k in name_keys {
                    if let Some(s) = obj.get(k).and_then(|v| v.as_str()) {
                        let trimmed = s.trim();
                        if !trimmed.is_empty() {
                            return Some(trimmed.to_string());
                        }
                    }
                }
            }
        }
    }
    None
}

/// Lấy URL khi task không kèm URL: GET /v1/media/{name} (13.3)
pub fn media_url(cfg: &Settings, name: &str) -> Result<String, ApiFail> {
    let clean = name.trim().trim_start_matches('/');
    let clean = clean.strip_prefix("v1/media/").unwrap_or(clean);
    let clean = clean.strip_prefix("media/").unwrap_or(clean);
    let safe = sanitize_url_segment(clean, true)?;
    let (base, _key, _model) = resolve_video_base_and_key(cfg)?;
    Ok(format!("{base}/media/{safe}"))
}

/// Chọn URL tải video cuối cùng: ưu tiên URL trực tiếp -> `media_name` -> fallback cuối cùng là `task_id`.
pub fn resolve_download_url(
    cfg: &Settings,
    video_url: Option<String>,
    media_name: Option<String>,
    task_id: &str,
) -> Result<String, ApiFail> {
    if let Some(u) = video_url.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        return Ok(u);
    }
    if let Some(name) = media_name.filter(|s| !s.trim().is_empty()) {
        if let Ok(u) = media_url(cfg, &name) {
            return Ok(u);
        }
    }
    media_url(cfg, task_id)
}

/// So sánh cùng origin (`scheme + host + port`) giữa hai URL ([P0-6]).
/// CHỈ đính kèm `Authorization: Bearer` khi URL tải cùng origin với `endpoint_video_base_url`!
pub fn is_same_origin(url_a: &str, url_b: &str) -> bool {
    let (Ok(a), Ok(b)) = (
        reqwest::Url::parse(url_a.trim()),
        reqwest::Url::parse(url_b.trim()),
    ) else {
        return false;
    };
    let (Some(host_a), Some(host_b)) = (a.host_str(), b.host_str()) else {
        return false;
    };
    a.scheme().eq_ignore_ascii_case(b.scheme())
        && host_a.eq_ignore_ascii_case(host_b)
        && a.port_or_known_default() == b.port_or_known_default()
}

/// Kiểm tra header ISO BMFF / MP4 hợp lệ (`bytes[4..8]` là box type chuẩn như `ftyp`).
pub fn is_valid_mp4_header(header: &[u8]) -> bool {
    if header.len() < 8 {
        return false;
    }
    matches!(
        &header[4..8],
        b"ftyp" | b"moov" | b"mdat" | b"styp" | b"free" | b"skip" | b"wide" | b"pdin"
    )
}

/// Xác thực file MP4 đã tải về trên đĩa: kích thước > 0, khớp `Content-Length` (nếu có), và có MP4 box header.
pub fn verify_mp4_file(path: &Path, expected_len: Option<u64>) -> Result<u64, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("Không đọc được metadata file tạm: {e}"))?;
    let actual_len = meta.len();
    if actual_len == 0 {
        return Err("File tải về rỗng (0 byte)".into());
    }
    if let Some(exp) = expected_len {
        if exp > 0 && actual_len != exp {
            return Err(format!(
                "Kích thước file tải về không đủ ({actual_len}/{exp} bytes)"
            ));
        }
    }
    let mut f = std::fs::File::open(path).map_err(|e| format!("Không mở được file tạm: {e}"))?;
    let mut header = [0u8; 12];
    let n = f
        .read(&mut header)
        .map_err(|e| format!("Không đọc được header MP4: {e}"))?;
    if !is_valid_mp4_header(&header[..n]) {
        return Err("File tải về không có header MP4 hợp lệ (thiếu ftyp box)".into());
    }
    Ok(actual_len)
}

#[derive(Debug)]
pub struct DownloadResult {
    pub bytes: u64,
    pub final_url: String,
}

/// Bước 3 của B3: tải file theo 13.6 - stream ra file `.part`, kiểm tra đủ & hợp lệ mới đổi tên sang `.mp4`.
/// Không bao giờ gửi Bearer token sang S3/R2/CDN khác origin ([P0-6]).
pub fn download_video(
    cfg: &Settings,
    url: &str,
    dest: &Path,
    task_id: &str,
    stop_flag: &AtomicBool,
) -> Result<DownloadResult, ApiFail> {
    let mut last_err: String;
    let mut current_url = url.to_string();
    let mut refreshed_url_once = false;
    let mut retry_attempt: u32 = 0;
    static PART_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let seq = PART_SEQ.fetch_add(1, Ordering::Relaxed);
    let part_path = dest.with_extension(format!("mp4.{}.{seq}.part", std::process::id()));

    loop {
        if stop_flag.load(Ordering::SeqCst) {
            let _ = std::fs::remove_file(&part_path);
            return Err(ApiFail::stopped(
                "Đã dừng an toàn trong lúc tải file video",
            ));
        }

        let (base_clean, key, _model) = resolve_video_base_and_key(cfg)?;
        let attach_auth = !key.is_empty() && is_same_origin(&current_url, &base_clean);

        let mut req = client_for_url(600, current_url.trim()).get(current_url.trim());
        if attach_auth {
            req = req.bearer_auth(&key);
        }

        match req.send() {
            Ok(mut resp) => {
                let st = resp.status().as_u16();
                if (st == 403 || st == 404) && !refreshed_url_once {
                    refreshed_url_once = true;
                    // URL hết hạn: poll lại task 1 lần để lấy URL mới, KHÔNG tính là 1 trong 3 lần retry download
                    match poll_once(cfg, task_id) {
                        Ok(PollOutcome::Xong {
                            video_url,
                            media_name,
                        }) => match resolve_download_url(cfg, video_url, media_name, task_id) {
                            Ok(new_u) => {
                                current_url = new_u;
                                continue;
                            }
                            Err(e) => {
                                last_err = crate::settings::redact_secrets(&format!(
                                    "HTTP_{st} (URL mới không hợp lệ: {})",
                                    e.message
                                ));
                            }
                        },
                        Ok(PollOutcome::TaskGone) => {
                            let _ = std::fs::remove_file(&part_path);
                            return Err(ApiFail {
                                kind: ApiFailKind::TamThoi { retry_after_s: None },
                                http_status: Some(404),
                                code: "DOWNLOAD_TASK_GONE".into(),
                                message: crate::settings::redact_secrets(&format!(
                                    "URL tải hết hạn ({st}) và task {task_id} không còn tồn tại trên server (404)"
                                )),
                            });
                        }
                        Ok(_) => {
                            last_err = format!("HTTP_{st} (không lấy được URL mới từ task)");
                        }
                        Err(e) => {
                            last_err = crate::settings::redact_secrets(&format!(
                                "HTTP_{st} (làm mới URL lỗi: {})",
                                e.message
                            ));
                        }
                    }
                } else if st == 200 || st == 206 {
                    let expected_len = if resp.headers().contains_key(reqwest::header::CONTENT_ENCODING) {
                        None
                    } else {
                        resp.content_length()
                    };

                    let copy_res = (|| -> Result<u64, String> {
                        let mut out_file = std::fs::File::create(&part_path)
                            .map_err(|e| format!("WRITE_OPEN:{e}"))?;
                        let written = std::io::copy(&mut resp, &mut out_file)
                            .map_err(|e| format!("Lỗi đọc luồng tải video: {e}"))?;
                        out_file
                            .flush()
                            .map_err(|e| format!("WRITE_FLUSH:{e}"))?;
                        let _ = out_file.sync_all();
                        Ok(written)
                    })();

                    match copy_res {
                        Ok(_written) => match verify_mp4_file(&part_path, expected_len) {
                            Ok(valid_len) => {
                                std::fs::rename(&part_path, dest).map_err(|e| {
                                    let _ = std::fs::remove_file(&part_path);
                                    ApiFail {
                                        kind: ApiFailKind::NghiepVu,
                                        http_status: None,
                                        code: "WRITE_FAIL".into(),
                                        message: crate::settings::redact_secrets(&format!(
                                            "Không đổi tên được file tạm sang MP4: {e}"
                                        )),
                                    }
                                })?;
                                return Ok(DownloadResult {
                                    bytes: valid_len,
                                    final_url: current_url,
                                });
                            }
                            Err(verify_err) => {
                                let _ = std::fs::remove_file(&part_path);
                                last_err = verify_err;
                            }
                        },
                        Err(e) if e.starts_with("WRITE_OPEN:") || e.starts_with("WRITE_FLUSH:") => {
                            let _ = std::fs::remove_file(&part_path);
                            return Err(ApiFail {
                                kind: ApiFailKind::NghiepVu,
                                http_status: None,
                                code: "WRITE_FAIL".into(),
                                message: crate::settings::redact_secrets(&format!(
                                    "Không ghi được file tạm vào video_out/: {e}"
                                )),
                            });
                        }
                        Err(stream_err) => {
                            let _ = std::fs::remove_file(&part_path);
                            last_err = stream_err;
                        }
                    }
                } else {
                    last_err = format!("HTTP_{st}");
                }
            }
            Err(e) => {
                last_err = crate::settings::redact_secrets(&format!("Lỗi mạng khi tải video: {e}"));
            }
        }

        retry_attempt += 1;
        if retry_attempt >= 3 {
            break;
        }
        let sleep_s = DOWNLOAD_BACKOFF_S[(retry_attempt - 1) as usize];
        if sleep_interruptible(Duration::from_secs(sleep_s), stop_flag) {
            let _ = std::fs::remove_file(&part_path);
            return Err(ApiFail::stopped(
                "Đã dừng an toàn giữa các lần thử tải video",
            ));
        }
    }

    let _ = std::fs::remove_file(&part_path);
    Err(ApiFail {
        kind: ApiFailKind::TamThoi { retry_after_s: None },
        http_status: None,
        code: "DOWNLOAD_FAIL".into(),
        message: crate::settings::redact_secrets(&format!(
            "Tải video thất bại sau 3 lần thử: {last_err}"
        )),
    })
}

/// Xác định `base_url` và `api_key` thực tế:
/// - Nếu `base_url` là `Some(draft)`, giữ nguyên `draft.trim()` (kể cả khi người dùng xóa trắng `""` để không test nhầm URL cũ!).
/// - Nếu `base_url` là `None`, mới lấy từ cấu hình đã lưu; riêng nguồn video `"internal"`
///   thì dùng sidecar muse2api localhost + key phiên (tự spawn nếu chưa chạy).
pub fn resolve_endpoint_and_key(
    cfg: &Settings,
    which: &str,
    base_url: Option<&str>,
    api_key: Option<&str>,
) -> (String, String) {
    // Nguồn nội bộ (ảnh hoặc video) chỉ áp dụng khi không truyền draft URL/key.
    if base_url.is_none() && api_key.is_none() {
        let internal = if which == "anh" {
            cfg.image_source == "internal"
        } else {
            is_internal_video_source(cfg)
        };
        if internal {
            let resolved = if which == "anh" {
                resolve_image_base_and_key(cfg).map(|(b, k, _m)| (b, k))
            } else {
                resolve_video_base_and_key(cfg).map(|(b, k, _m)| (b, k))
            };
            // Lỗi engine: về rỗng để caller báo "Chưa nhập Base URL" dễ hiểu hơn là
            // nổ lỗi nội bộ trong lúc user chỉ đang bấm Test kết nối.
            return resolved.unwrap_or_default();
        }
    }
    let resolved_base = match base_url {
        Some(draft) => draft.trim().to_string(),
        None => match which {
            "anh" => cfg.endpoint_image_base_url.trim().to_string(),
            _ => cfg.endpoint_video_base_url.trim().to_string(),
        },
    };

    let resolved_key = api_key
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| crate::settings::get_api_key(which).unwrap_or_default());

    (resolved_base, resolved_key)
}

/// Kiểm tra kết nối endpoint (6.6b): gọi danh sách model, đo thời gian đáp
pub fn test_connection(
    cfg: &Settings,
    which: &str,
    base_url: Option<&str>,
    api_key: Option<&str>,
) -> AppResult<(bool, u64, String)> {
    let start = Instant::now();
    let (base, key) = resolve_endpoint_and_key(cfg, which, base_url, api_key);
    if base.is_empty() {
        return Ok((false, 0, "Chưa nhập Base URL endpoint".into()));
    }
    let url = format!("{}/models", base.trim_end_matches('/'));
    let mut req = client_for_url(30, &url).get(&url);
    if !key.is_empty() {
        req = req.bearer_auth(&key);
    }
    let resp = req.send();
    let elapsed = start.elapsed().as_millis() as u64;
    match resp {
        Ok(r) => {
            let st = r.status().as_u16();
            if st == 200 {
                Ok((true, elapsed, "HTTP 200 OK".into()))
            } else {
                let body = r.text().unwrap_or_default();
                let fail = ApiFail::classify_http(st, None, &body);
                Ok((false, elapsed, fail.message))
            }
        }
        Err(e) => Ok((
            false,
            elapsed,
            crate::settings::redact_secrets(&format!("Không kết nối được: {e}")),
        )),
    }
}

/// Tải danh sách model từ GET /models (6.6: nút Tải danh sách model)
pub fn list_models(
    cfg: &Settings,
    which: &str,
    base_url: Option<&str>,
    api_key: Option<&str>,
) -> AppResult<Vec<String>> {
    let (base, key) = resolve_endpoint_and_key(cfg, which, base_url, api_key);
    if base.is_empty() {
        return Err(AppError::new(
            "EMPTY_BASE_URL",
            "Chưa nhập Base URL endpoint.",
        ));
    }
    let url = format!("{}/models", base.trim_end_matches('/'));
    let mut req = client_for_url(30, &url).get(&url);
    if !key.is_empty() {
        req = req.bearer_auth(&key);
    }
    let resp = req.send().map_err(|e| {
        AppError::new(
            "NETWORK",
            crate::settings::redact_secrets(&format!("Không gọi được endpoint: {e}")),
        )
    })?;
    let status = resp.status().as_u16();
    let body = resp.text().map_err(|e| {
        AppError::new(
            "NETWORK",
            crate::settings::redact_secrets(&format!("Đọc response lỗi: {e}")),
        )
    })?;
    if status != 200 {
        let fail = ApiFail::classify_http(status, None, &body);
        return Err(AppError::new(&fail.code, fail.message));
    }
    let json: Value = serde_json::from_str(&body).map_err(|e| {
        AppError::new(
            "BAD_RESPONSE",
            crate::settings::redact_secrets(&format!("Response không phải JSON: {e}")),
        )
    })?;
    let ids = json
        .get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_same_origin_security_boundary() {
        let base = "https://muse.photowrapped.com/v1";
        assert!(is_same_origin(
            "https://muse.photowrapped.com/v1/media/vid_001.mp4",
            base
        ));
        assert!(is_same_origin(
            "https://MUSE.photowrapped.com:443/media/vid_001.mp4",
            base
        ));
        // Presigned S3 / R2 / CDN URLs MUST NOT match origin
        assert!(!is_same_origin(
            "https://r2.cloudflarestorage.com/bucket/vid_001.mp4?X-Amz-Signature=abc",
            base
        ));
        assert!(!is_same_origin(
            "https://s3.amazonaws.com/bucket/vid_001.mp4",
            base
        ));
        // Scheme or port differences MUST NOT match origin
        assert!(!is_same_origin(
            "http://muse.photowrapped.com/v1/media/vid_001.mp4",
            base
        ));
        assert!(!is_same_origin(
            "http://localhost:8000/v1/media/a.mp4",
            "http://localhost:9000/v1"
        ));
    }

    #[test]
    fn test_http_status_classification_and_retry_after() {
        // NghiepVu codes
        for st in [400, 401, 403, 404, 405, 413, 422] {
            let f = ApiFail::classify_http(st, None, r#"{"error":{"message":"bad request"}}"#);
            assert_eq!(f.kind, ApiFailKind::NghiepVu, "status {st} must be NghiepVu");
        }

        // Quota keywords override status into HetQuota
        let q = ApiFail::classify_http(
            429,
            None,
            r#"{"error":{"code":"insufficient_quota","message":"You exceeded your current quota"}}"#,
        );
        assert_eq!(q.kind, ApiFailKind::HetQuota);
        assert_eq!(q.code, "QUOTA_EXHAUSTED");

        let q2 = ApiFail::classify_http(403, None, "billing_hard_limit reached");
        assert_eq!(q2.kind, ApiFailKind::HetQuota);

        // 429 with Retry-After header -> TamThoi with retry_after_s
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "45".parse().unwrap());
        let rl = ApiFail::classify_http(429, Some(&headers), r#"{"error":"rate limit"}"#);
        assert_eq!(rl.kind, ApiFailKind::TamThoi { retry_after_s: Some(45) });

        // 408 and 5xx -> TamThoi
        for st in [408, 500, 502, 503, 504] {
            let t = ApiFail::classify_http(st, None, "gateway timeout");
            assert_eq!(t.kind, ApiFailKind::TamThoi { retry_after_s: None });
        }
    }

    #[test]
    fn test_vm_disabled_classification_khong_retry() {
        // muse2api trả 503 + code upstream_vm_disabled → NghiepVu (không auto-retry)
        let f = ApiFail::classify_http(
            503,
            None,
            r#"{"error":{"code":"upstream_vm_disabled","message":"muse.ai disconnected this account's workspace (vm_state=DISABLED); it needs a support ticket on muse.ai to reconnect."}}"#,
        );
        assert_eq!(f.kind, ApiFailKind::NghiepVu);
        assert_eq!(f.code, "UPSTREAM_VM_DISABLED");

        // Poll task failed cùng code → NghiepVu
        let p = classify_poll_task_failure(&serde_json::json!({
            "status": "failed",
            "error": {"code": "upstream_vm_disabled",
                      "message": "muse.ai disconnected this account's workspace"}
        }));
        match p {
            PollOutcome::Loi { kind, code, .. } => {
                assert_eq!(kind, ApiFailKind::NghiepVu);
                assert_eq!(code, "UPSTREAM_VM_DISABLED");
            }
            other => panic!("expected Loi, got {other:?}"),
        }

        // 503 no_account_available (pool trống) → TamThoi (retry hợp lệ)
        let na = ApiFail::classify_http(
            503,
            None,
            r#"{"error":{"code":"no_account_available","message":"no muse.ai account is available"}}"#,
        );
        assert_eq!(na.kind, ApiFailKind::TamThoi { retry_after_s: None });

        // vm_disabled trong message dù code lạ → vẫn nhận diện được
        let f2 = ApiFail::classify_http(
            502,
            None,
            r#"{"error":{"message":"muse.ai disconnected this account's workspace (vm_state=DISABLED)"}}"#,
        );
        assert_eq!(f2.kind, ApiFailKind::NghiepVu);
    }

    #[test]
    fn test_upstream_refused_classification_khong_retry() {
        // [P0-7]: UpstreamRefused (502) với error code "upstream_refused" phải là NghiepVu
        let f1 = ApiFail::classify_http(
            502,
            None,
            r#"{"error":{"code":"upstream_refused","message":"Upstream answered but did not produce what was asked"}}"#,
        );
        assert_eq!(f1.kind, ApiFailKind::NghiepVu);
        assert_eq!(f1.code, "UPSTREAM_REFUSED");

        // [P0-7]: 502 với text content policy / policy violation / vi phạm chính sách trong body
        let f2 = ApiFail::classify_http(
            502,
            None,
            r#"{"detail":"Generation blocked due to content policy violation"}"#,
        );
        assert_eq!(f2.kind, ApiFailKind::NghiepVu);
        assert_eq!(f2.code, "UPSTREAM_REFUSED");

        // [P0-7]: Poll outcome failure với code "upstream_refused"
        let p1 = classify_poll_task_failure(&serde_json::json!({
            "status": "failed",
            "error": {
                "code": "upstream_refused",
                "message": "Prompt vi phạm chính sách cộng đồng"
            }
        }));
        match p1 {
            PollOutcome::Loi { kind, code, message } => {
                assert_eq!(kind, ApiFailKind::NghiepVu);
                assert_eq!(code, "UPSTREAM_REFUSED");
                assert!(message.contains("vi phạm chính sách"));
            }
            other => panic!("expected Loi, got {other:?}"),
        }
    }

    #[test]
    fn test_video_locked_classification_khong_retry() {
        // [P1-4]: HTTP error với code upstream_video_locked hoặc message
        let f1 = ApiFail::classify_http(
            403,
            None,
            r#"{"error":{"code":"upstream_video_locked","message":"muse.ai reports video generation is locked"}}"#,
        );
        assert_eq!(f1.kind, ApiFailKind::NghiepVu);
        assert_eq!(f1.code, "VIDEO_LOCKED");

        let f2 = ApiFail::classify_http(
            502,
            None,
            r#"{"error":{"code":"upstream_error","message":"Tính năng tạo video không khả dụng trên tài khoản"}}"#,
        );
        assert_eq!(f2.kind, ApiFailKind::NghiepVu);
        assert_eq!(f2.code, "VIDEO_LOCKED");

        let f3 = ApiFail::classify_http(
            502,
            None,
            r#"{"error":{"message":"video generation is not available for this account"}}"#,
        );
        assert_eq!(f3.kind, ApiFailKind::NghiepVu);
        assert_eq!(f3.code, "VIDEO_LOCKED");

        // [P1-4]: Async poll task failure với code upstream_video_locked
        let p1 = classify_poll_task_failure(&serde_json::json!({
            "status": "failed",
            "error": {
                "code": "upstream_video_locked",
                "message": "muse.ai reports video generation is locked"
            }
        }));
        match p1 {
            PollOutcome::Loi { kind, code, message } => {
                assert_eq!(kind, ApiFailKind::NghiepVu);
                assert_eq!(code, "VIDEO_LOCKED");
                assert!(message.contains("khóa trên tài khoản"));
            }
            other => panic!("expected Loi, got {other:?}"),
        }

        let p2 = classify_poll_task_failure(&serde_json::json!({
            "status": "failed",
            "error": {
                "message": "Không thể tạo video vào lúc này"
            }
        }));
        match p2 {
            PollOutcome::Loi { kind, code, message } => {
                assert_eq!(kind, ApiFailKind::NghiepVu);
                assert_eq!(code, "VIDEO_LOCKED");
                assert!(message.contains("Không thể tạo video"));
            }
            other => panic!("expected Loi, got {other:?}"),
        }
    }

    #[test]
    fn test_jpeg_to_png_normalization_and_large_image_jpeg85_reencode() {
        // Tạo 1 ảnh RGBA nhỏ trên RAM, encode sang JPEG rồi kiểm tra ensure_png_bytes xuất ra PNG thật
        let img = image::RgbaImage::from_pixel(8, 8, image::Rgba([20, 140, 150, 255]));
        let dyn_img = image::DynamicImage::ImageRgba8(img);

        let mut jpg_bytes = Vec::new();
        dyn_img
            .to_rgb8()
            .write_to(&mut Cursor::new(&mut jpg_bytes), image::ImageFormat::Jpeg)
            .unwrap();
        assert_eq!(
            detect_image_mime(&jpg_bytes, Path::new("x.png")),
            "image/jpeg"
        );

        let normalized_png = ensure_png_bytes(jpg_bytes, 200).expect("convert jpg to png");
        assert_eq!(
            image::guess_format(&normalized_png).unwrap(),
            image::ImageFormat::Png
        );

        // Kiểm tra nén sang JPEG 85 khi vượt ngưỡng (test với ngưỡng nhỏ 10 bytes trên ảnh RGBA có kênh alpha!)
        let data_url =
            build_video_image_data_url(&normalized_png, Path::new("out_gen.png"), 10).unwrap();
        assert!(
            data_url.starts_with("data:image/jpeg;base64,"),
            "exceeding threshold must re-encode RGBA PNG to JPEG 85 data URL"
        );

        // Khi không vượt ngưỡng -> giữ nguyên data:image/png;base64,
        let png_url = build_video_image_data_url(
            &normalized_png,
            Path::new("out_gen.png"),
            MAX_VIDEO_INPUT_IMAGE_BYTES,
        )
        .unwrap();
        assert!(png_url.starts_with("data:image/png;base64,"));
    }

    #[test]
    fn test_media_name_extraction_and_mp4_verification() {
        let cfg = Settings::default();
        let json: Value = serde_json::json!({
            "status": "succeeded",
            "output": { "media_name": "clip_9988.mp4" }
        });
        assert_eq!(extract_video_url(&json), None);
        let media = extract_media_name(&json);
        assert_eq!(media.as_deref(), Some("clip_9988.mp4"));
        assert_eq!(
            resolve_download_url(&cfg, None, media, "task_abc").unwrap(),
            "https://muse.photowrapped.com/v1/media/clip_9988.mp4"
        );

        // Chặn path traversal / query injection trong task_id & media_name ([P2-7])
        assert!(sanitize_url_segment("../admin", false).is_err());
        assert!(sanitize_url_segment("task?foo=1", false).is_err());
        assert!(sanitize_url_segment("task#frag", false).is_err());
        assert!(sanitize_url_segment("task_ok-123", false).is_ok());

        // Phân loại lỗi bất đồng bộ trong PollOutcome::Loi ([P2-3])
        let poll_q = classify_poll_task_failure(&serde_json::json!({
            "status": "failed",
            "error": { "message": "You are out of credits for this billing cycle" }
        }));
        assert!(matches!(
            poll_q,
            PollOutcome::Loi {
                kind: ApiFailKind::HetQuota,
                ..
            }
        ));
        let poll_503 = classify_poll_task_failure(&serde_json::json!({
            "status": "failed",
            "error": { "code": "internal_error", "message": "503 backend overloaded" }
        }));
        assert!(matches!(
            poll_503,
            PollOutcome::Loi {
                kind: ApiFailKind::TamThoi { .. },
                ..
            }
        ));

        // Khi người dùng xóa trắng Base URL trên form (`Some("")`), không được lấy URL cũ đã lưu
        let (empty_base, _) = resolve_endpoint_and_key(&cfg, "anh", Some("   "), None);
        assert!(empty_base.is_empty());
        let (ok_conn, _, msg_conn) = test_connection(&cfg, "anh", Some(""), None).unwrap();
        assert!(!ok_conn);
        assert!(msg_conn.contains("Base URL"));

        // Kiểm tra verify_mp4_file
        let tmp_dir = std::env::temp_dir().join(format!("vf_mp4_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp_dir);
        let good_mp4 = tmp_dir.join("good.part");
        let mut fake_mp4 = vec![0x00, 0x00, 0x00, 0x18];
        fake_mp4.extend_from_slice(b"ftypmp42");
        fake_mp4.extend_from_slice(&[0u8; 12]);
        std::fs::write(&good_mp4, &fake_mp4).unwrap();
        assert_eq!(verify_mp4_file(&good_mp4, Some(24)).unwrap(), 24);
        assert!(verify_mp4_file(&good_mp4, Some(999)).is_err());

        let bad_mp4 = tmp_dir.join("bad.part");
        std::fs::write(&bad_mp4, b"<?xml version=\"1.0\"?><Error>AccessDenied</Error>").unwrap();
        assert!(verify_mp4_file(&bad_mp4, None).is_err());

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn loopback_url_va_upstream_quota_exhausted() {
        assert!(is_loopback_url("http://127.0.0.1:18610/v1/videos"));
        assert!(is_loopback_url("http://localhost:18610/v1/images/edits"));
        assert!(is_loopback_url("http://[::1]:18610/v1/models"));
        assert!(!is_loopback_url("https://muse.photowrapped.com/v1"));
        assert!(!is_loopback_url("not-a-url"));

        assert!(contains_quota_keyword("upstream_quota_exhausted"));
        assert!(contains_quota_keyword("quota_exhausted"));
        assert!(contains_quota_keyword("quota exhausted"));
        let fail = ApiFail::classify_http(
            429,
            None,
            r#"{"error":{"code":"upstream_quota_exhausted","message":"Account out of quota"}}"#,
        );
        assert_eq!(fail.kind, ApiFailKind::HetQuota);
        assert_eq!(fail.code, "QUOTA_EXHAUSTED");
    }

    #[test]
    fn internal_video_mode_uses_muse_video_model() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::{Arc, Mutex};

        let tmp_dir = std::env::temp_dir().join(format!(
            "vf_m3_video_model_{}_{}",
            std::process::id(),
            chrono::Local::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let img_path = tmp_dir.join("prod.png");
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([10, 20, 30, 255]));
        image::DynamicImage::ImageRgba8(img)
            .save_with_format(&img_path, image::ImageFormat::Png)
            .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock video server");
        let port = listener.local_addr().unwrap().port();
        let captured_models: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let models_clone = Arc::clone(&captured_models);

        let server = std::thread::spawn(move || {
            for i in 0..2 {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    let n = stream.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    let s = String::from_utf8_lossy(&buf);
                    if let Some(idx) = s.find("\r\n\r\n") {
                        let headers_part = &s[..idx];
                        let content_len = headers_part
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                if k.trim().eq_ignore_ascii_case("content-length") {
                                    v.trim().parse::<usize>().ok()
                                } else {
                                    None
                                }
                            })
                            .unwrap_or(0);
                        if buf.len() >= idx + 4 + content_len {
                            let body_bytes = &buf[idx + 4..idx + 4 + content_len];
                            let v: Value = serde_json::from_slice(body_bytes).unwrap_or(Value::Null);
                            if let Some(m) = v.get("model").and_then(|m| m.as_str()) {
                                models_clone.lock().unwrap().push(m.to_string());
                            }
                            break;
                        }
                    }
                }
                let resp_body = format!(r#"{{"id":"task_m3_{i}"}}"#);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    resp_body.len(),
                    resp_body
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });

        // 1. Internal video mode MUST send "muse-video" even when endpoint_video_model is "sora-2".
        let internal_cfg = Settings {
            video_source: "internal".to_string(),
            endpoint_video_model: "sora-2".to_string(),
            ..Settings::default()
        };
        let (base, key, model) = resolve_video_base_and_key_with(&internal_cfg, |_| {
            Ok(crate::muse_sidecar::SidecarSession {
                port,
                api_key: "vf_mock_key".to_string(),
            })
        })
        .expect("resolve internal");
        assert_eq!(base, format!("http://127.0.0.1:{port}/v1"));
        assert_eq!(key, "vf_mock_key");
        assert_eq!(model, "muse-video");

        let task_int = create_video_task_inner(
            &internal_cfg,
            &img_path,
            "animate product",
            None,
            |_| {
                Ok(crate::muse_sidecar::SidecarSession {
                    port,
                    api_key: "vf_mock_key".to_string(),
                })
            },
        )
        .expect("create internal video task");
        assert_eq!(task_int.task_id, "task_m3_0");

        // 2. Gateway video mode MUST send cfg.endpoint_video_model.
        let gateway_base = format!("http://127.0.0.1:{port}/v1");
        register_test_api_key(&gateway_base, "gw_test_key");
        let gateway_cfg = Settings {
            video_source: "gateway".to_string(),
            endpoint_video_base_url: gateway_base,
            endpoint_video_model: "sora-2-pro".to_string(),
            ..Settings::default()
        };
        let (_, _, gw_model) = resolve_video_base_and_key(&gateway_cfg).expect("resolve gateway");
        assert_eq!(gw_model, "sora-2-pro");

        let task_gw = create_video_task(&gateway_cfg, &img_path, "animate product", None)
            .expect("create gateway video task");
        assert_eq!(task_gw.task_id, "task_m3_1");

        server.join().expect("mock video server");
        let models = captured_models.lock().unwrap();
        assert_eq!(models.as_slice(), &["muse-video", "sora-2-pro"]);

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn adapters_error_paths_redact_secrets() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let known_secret = "secret_tok_adapters_m7_xyz";
        crate::settings::register_secret_for_redaction(known_secret);

        let tmp_dir = std::env::temp_dir().join(format!(
            "vf_m7_redact_{}_{}",
            std::process::id(),
            chrono::Local::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let img_path = tmp_dir.join("ref.png");
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]));
        image::DynamicImage::ImageRgba8(img)
            .save_with_format(&img_path, image::ImageFormat::Png)
            .unwrap();

        // 1. classify_poll_task_failure redacts Bearer, sk-, and KNOWN_SECRETS
        let poll_err = classify_poll_task_failure(&serde_json::json!({
            "status": "failed",
            "error": {
                "message": format!("failed with Bearer secret_bearer_999 sk-secret_sk_999 {known_secret}")
            }
        }));
        match poll_err {
            PollOutcome::Loi { message, .. } => {
                assert!(!message.contains("secret_bearer_999"));
                assert!(!message.contains("secret_sk_999"));
                assert!(!message.contains(known_secret));
                assert!(message.contains("[REDACTED]"));
            }
            other => panic!("expected PollOutcome::Loi, got {other:?}"),
        }

        // 2. sanitize_url_segment redacts secrets
        let seg_err = sanitize_url_segment(
            &format!("bad?Bearer secret_bearer_999 {known_secret}"),
            false,
        )
        .unwrap_err();
        assert!(!seg_err.message.contains("secret_bearer_999"));
        assert!(!seg_err.message.contains(known_secret));
        assert!(seg_err.message.contains("[REDACTED]"));

        // 3. Mock server returning missing-field JSON (200 OK) for create_image & create_video_task, plus 200 OK for download_video write failure
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let port = listener.local_addr().unwrap().port();
        let secret_body = format!(
            r#"{{"unexpected":"Bearer secret_bearer_999 sk-secret_sk_999 {known_secret}"}}"#
        );
        let secret_body_clone = secret_body.clone();
        let server = std::thread::spawn(move || {
            for i in 0..3 {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                if i < 2 {
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        secret_body_clone.len(),
                        secret_body_clone
                    );
                    let _ = stream.write_all(resp.as_bytes());
                } else {
                    let payload = b"0000ftypmp420000";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        payload.len()
                    );
                    let _ = stream.write_all(resp.as_bytes());
                    let _ = stream.write_all(payload);
                }
            }
        });

        let base_url = format!("http://127.0.0.1:{port}/v1");
        register_test_api_key(&base_url, "gw_test_key");
        let cfg = Settings {
            image_source: "gateway".to_string(),
            video_source: "gateway".to_string(),
            endpoint_image_base_url: base_url.clone(),
            endpoint_video_base_url: base_url.clone(),
            ..Settings::default()
        };

        let img_err = create_image(&cfg, &img_path, "prompt").expect_err("missing b64_json");
        assert!(!img_err.message.contains("secret_bearer_999"));
        assert!(!img_err.message.contains("secret_sk_999"));
        assert!(!img_err.message.contains(known_secret));
        assert!(img_err.message.contains("[REDACTED]"));

        let vid_err = create_video_task(&cfg, &img_path, "prompt", None)
            .expect_err("missing task_id");
        assert!(!vid_err.message.contains("secret_bearer_999"));
        assert!(!vid_err.message.contains("secret_sk_999"));
        assert!(!vid_err.message.contains(known_secret));
        assert!(vid_err.message.contains("[REDACTED]"));

        // download_video WRITE_FAIL with secret in non-existent destination path
        let missing_dest = tmp_dir.join(known_secret).join("out.mp4");
        let stop = AtomicBool::new(false);
        let dl_write_err = download_video(
            &cfg,
            &format!("{base_url}/media/vid.mp4"),
            &missing_dest,
            "task_1",
            &stop,
        )
        .expect_err("missing dest parent must fail write");
        assert_eq!(dl_write_err.code, "WRITE_FAIL");
        assert!(!dl_write_err.message.contains(known_secret));

        server.join().expect("mock server");

        // 4. Closed port for test_connection, list_models, and download_video reqwest::Error redaction
        let closed_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let closed_port = closed_listener.local_addr().unwrap().port();
        drop(closed_listener);
        let secret_url = format!("http://127.0.0.1:{closed_port}/v1?token=sk-secret_sk_999&k={known_secret}");

        let (ok_tc, _, tc_msg) =
            test_connection(&cfg, "video", Some(&secret_url), None).expect("test_connection Ok");
        assert!(!ok_tc);
        assert!(!tc_msg.contains("secret_sk_999"));
        assert!(!tc_msg.contains(known_secret));

        let lm_err = list_models(&cfg, "video", Some(&secret_url), None)
            .expect_err("list_models closed port");
        assert!(!lm_err.message.contains("secret_sk_999"));
        assert!(!lm_err.message.contains(known_secret));

        let dl_net_err = download_video(
            &cfg,
            &format!("http://127.0.0.1:{closed_port}/v.mp4?token=sk-secret_sk_999&k={known_secret}"),
            &tmp_dir.join("never.mp4"),
            "task_net",
            &stop,
        )
        .expect_err("closed port download must fail");
        assert!(!dl_net_err.message.contains("secret_sk_999"));
        assert!(!dl_net_err.message.contains(known_secret));
        assert!(dl_net_err.message.contains("[REDACTED]"));

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}
