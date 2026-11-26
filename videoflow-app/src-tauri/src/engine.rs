//! Run Engine (Lớp 2 - mục 3): pipeline 2 tầng song song độc lập (tạo ảnh B2 / tạo video B3)
//! với đa luồng `1..=MAX_CONCURRENCY` (giới hạn `<= 1` video / tài khoản Muse nội bộ), máy trạng thái 5.3,
//! retry theo bảng 13.5 + Phụ lục v1.3.1, dừng an toàn (5.4), chạy tiếp sau
//! gián đoạn (5.3), versioning _vN (6.10), single-writer ghi CSV (7.2).

use crate::adapters::{self, ApiFail, ApiFailKind, PollOutcome};
use crate::error::{AppError, AppResult};
use crate::prompts_csv::{self, RowStatus};
use crate::settings::Settings;
use base64::Engine as _;
use rusqlite::Connection;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

const MAX_RETRY_AFTER_S: u64 = 600;

mod auto_merge;
mod pipeline;
mod planner;
pub use auto_merge::*;
pub use pipeline::*;
pub use planner::*;

// ━━ Sự kiện theo hợp đồng 13.2 ━━

#[derive(Serialize, Clone)]
pub struct RunStartedEvent {
    pub run_id: i64,
    pub kind: String,
    pub total: usize,
}

#[derive(Serialize, Clone, Debug)]
pub struct ActiveRowProgress {
    pub row_key: String,
    pub step: String,
    pub detail: String,
}

#[derive(Serialize, Clone)]
pub struct ProgressEvent {
    pub run_id: i64,
    pub done: usize,
    pub total: usize,
    pub percent: f64,
    pub current_row_key: String,
    pub current_step: String,
    pub step_detail: String,
    pub active_count: usize,
    pub active_rows: Vec<ActiveRowProgress>,
}

#[derive(Serialize, Clone)]
pub struct RowChangedEvent {
    pub row_key: String,
    pub status: String,
    pub image_status: String,
    pub video_status: String,
    pub attempts_count: i64,
    pub last_error: Option<String>,
}

#[derive(Serialize, Clone)]
pub struct RunFinishedEvent {
    pub run_id: i64,
    pub status: String,
    pub done_count: usize,
    pub error_count: usize,
    pub duration_ms: i64,
}

#[derive(Serialize, Clone)]
pub struct RunErrorEvent {
    pub run_id: i64,
    pub row_key: Option<String>,
    pub step: Option<String>,
    pub code: String,
    pub message: String,
}

pub type EventEmitter<'a> = &'a (dyn Fn(&str, serde_json::Value) + Send + Sync);

fn emit<T: serde::Serialize>(emitter: EventEmitter<'_>, name: &str, payload: &T) {
    if let Ok(val) = serde_json::to_value(payload) {
        emitter(name, val);
    }
}

// ━━ Trạng thái máy (5.3) & Truy vấn jobs ━━

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    B2,
    B3,
}

/// Tra bản ghi job cho một dòng CSV, có tương thích dữ liệu CŨ.
///
/// `state.db` trước v1.4.1 lưu `row_key` = tên ảnh. Nếu CSV hiện tại chỉ có **một** dòng
/// dùng ảnh đó thì dữ liệu cũ chắc chắn thuộc về dòng này, nên dùng lại được — nhờ vậy
/// trạng thái `video_task_id`, số lần thử và cảnh báo "prompt đã sửa" không bị mất khi
/// nâng cấp. Khi ảnh được dùng cho nhiều dòng (>=2), dữ liệu cũ là mơ hồ → bỏ qua và
/// dòng được tính lại từ đầu (đúng như tài liệu migration đã nêu).
pub fn job_for_row<'a>(
    map: &'a HashMap<String, LatestJobInfo>,
    rows: &[prompts_csv::Row],
    row: &prompts_csv::Row,
) -> Option<&'a LatestJobInfo> {
    let key = row_key_low(row);
    if let Some(j) = map.get(&key) {
        return Some(j);
    }
    let legacy = row.anh_tham_chieu.trim().to_lowercase();
    if legacy.is_empty() {
        return None;
    }
    let same_image = rows
        .iter()
        .filter(|r| r.anh_tham_chieu.trim().to_lowercase() == legacy)
        .count();
    // Ảnh dùng cho ĐÚNG một dòng ⇒ mọi bản ghi lịch sử của ảnh đó thuộc dòng này:
    // ưu tiên `row_key` = tên ảnh (dữ liệu trước v1.4.1), nếu không có thì lấy bản ghi
    // cùng tên ảnh với hash khác (khoá đổi khi người dùng sửa prompt).
    if same_image != 1 {
        return None;
    }
    if let Some(j) = map.get(&legacy) {
        return Some(j);
    }
    let prefix = format!("{legacy}#");
    map.iter()
        .filter(|(k, _)| k.starts_with(&prefix))
        .max_by_key(|(_, v)| v.id)
        .map(|(_, v)| v)
}

/// Đếm số dòng CSV dùng chung mỗi `anh_tham_chieu` (lowercase) để phân biệt ảnh duy nhất
/// với ảnh dùng chung cho nhiều dòng.
pub fn image_counts(rows: &[prompts_csv::Row]) -> HashMap<String, usize> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for r in rows {
        if r.is_example() || r.is_empty_content() {
            continue;
        }
        let img = r.anh_tham_chieu.trim().to_lowercase();
        if !img.is_empty() {
            *counts.entry(img).or_insert(0) += 1;
        }
    }
    counts
}

/// Nạp `LatestJobInfo` theo `jobs.id` cụ thể.
pub fn load_job_info_by_id(conn: &Connection, job_id: i64) -> Option<LatestJobInfo> {
    conn.query_row(
        "SELECT id, row_key, status, image_status, video_status,
                image_prompt_hash, video_prompt_hash, attempts_count,
                last_error, video_task_id, video_download_url
         FROM jobs
         WHERE id = ?1",
        rusqlite::params![job_id],
        |r| {
            Ok(LatestJobInfo {
                id: r.get(0)?,
                row_key: r.get(1)?,
                status: r.get(2)?,
                image_status: r.get(3)?,
                video_status: r.get(4)?,
                image_prompt_hash: r.get(5)?,
                video_prompt_hash: r.get(6)?,
                attempts_count: r.get(7)?,
                last_error: r.get(8)?,
                video_task_id: r.get(9)?,
                video_download_url: r.get(10)?,
            })
        },
    )
    .ok()
}

/// Tra `LatestJobInfo` nhất quán cho một dòng CSV:
/// 1. Khớp chính xác `row_key` (`tên_ảnh#hash`).
/// 2. Nếu chưa khớp và dòng đã có `anh_da_tao` / `video_da_tao`, tra `file_refs` để tìm đúng `job_id`
///    (định danh chính xác kể cả khi nhiều dòng dùng chung 1 ảnh gốc và prompt đã bị sửa).
/// 3. Nếu ảnh chỉ dùng cho đúng 1 dòng (`same_image == 1`), fallback sang tên ảnh trần (v1.4.0)
///    hoặc bản ghi mới nhất theo `job.id` có cùng prefix `{image}#`.
pub fn latest_job_for_csv_row(
    conn: &Connection,
    row: &prompts_csv::Row,
    latest_map: &HashMap<String, LatestJobInfo>,
    img_counts: &HashMap<String, usize>,
) -> Option<LatestJobInfo> {
    let key = row_key_low(row);
    if let Some(j) = latest_map.get(&key) {
        return Some(j.clone());
    }

    for fname in [row.anh_da_tao.trim(), row.video_da_tao.trim()] {
        if fname.is_empty() {
            continue;
        }
        let job_id: Option<i64> = conn
            .query_row(
                "SELECT job_id FROM file_refs
                 WHERE LOWER(file_name) = LOWER(?1) AND job_id IS NOT NULL
                 ORDER BY id DESC LIMIT 1",
                rusqlite::params![fname],
                |r| r.get(0),
            )
            .ok();
        if let Some(jid) = job_id {
            if let Some(info) = load_job_info_by_id(conn, jid) {
                return Some(info);
            }
        }
        let ref_rk: Option<String> = conn
            .query_row(
                "SELECT row_key FROM file_refs
                 WHERE LOWER(file_name) = LOWER(?1)
                 ORDER BY id DESC LIMIT 1",
                rusqlite::params![fname],
                |r| r.get(0),
            )
            .ok();
        if let Some(rk) = ref_rk {
            if let Some(info) = latest_map.get(&rk.to_lowercase()) {
                return Some(info.clone());
            }
        }
    }

    let legacy = row.anh_tham_chieu.trim().to_lowercase();
    if legacy.is_empty() {
        return None;
    }
    if img_counts.get(&legacy).copied().unwrap_or(0) != 1 {
        return None;
    }
    if let Some(j) = latest_map.get(&legacy) {
        return Some(j.clone());
    }
    let prefix = format!("{legacy}#");
    latest_map
        .iter()
        .filter(|(k, _)| k.starts_with(&prefix))
        .max_by_key(|(_, v)| v.id)
        .map(|(_, v)| v.clone())
}

/// Như `job_for_row` nhưng cho map bất kỳ (media refs, …) — cùng quy tắc legacy:
/// chỉ fallback về tên ảnh khi CSV có đúng một dòng dùng ảnh đó.
pub fn map_lookup_legacy<'a, V>(
    map: &'a HashMap<String, V>,
    rows: &[prompts_csv::Row],
    row: &prompts_csv::Row,
) -> Option<&'a V> {
    if let Some(v) = map.get(&row_key_low(row)) {
        return Some(v);
    }
    let legacy = row.anh_tham_chieu.trim().to_lowercase();
    if legacy.is_empty() {
        return None;
    }
    let same_image = rows
        .iter()
        .filter(|r| r.anh_tham_chieu.trim().to_lowercase() == legacy)
        .count();
    // Khi ảnh chỉ dùng cho MỘT dòng, mọi bản ghi lịch sử của ảnh đó chắc chắn thuộc dòng
    // này: ưu tiên bản ghi legacy (`row_key` = tên ảnh, dữ liệu trước v1.4.1), nếu không
    // có thì lấy bản ghi có cùng tên ảnh với hash prompt CŨ (khoá đổi khi sửa prompt).
    if same_image != 1 {
        return None;
    }
    if let Some(j) = map.get(&legacy) {
        return Some(j);
    }
    let prefix = format!("{legacy}#");
    map.iter()
        .filter(|(k, _)| k.starts_with(&prefix))
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, v)| v)
}

/// Khoá dòng (lowercase) dùng để tra `state.db`. Xem `prompts_csv::row_key_of`.
pub fn row_key_low(row: &prompts_csv::Row) -> String {
    prompts_csv::row_key_of(
        &row.anh_tham_chieu,
        &row.promt_tao_image,
        &row.promt_tao_video,
    )
    .to_lowercase()
}

/// Bản ghi jobs mới nhất của một `row_key` trong `state.db`
#[derive(Debug, Clone)]
pub struct LatestJobInfo {
    pub id: i64,
    pub row_key: String,
    pub status: String,
    pub image_status: String,
    pub video_status: String,
    pub image_prompt_hash: Option<String>,
    pub video_prompt_hash: Option<String>,
    pub attempts_count: i64,
    pub last_error: Option<String>,
    pub video_task_id: Option<String>,
    pub video_download_url: Option<String>,
}

/// Nạp toàn bộ bản ghi `jobs` mới nhất của mọi `row_key` bằng MỘT câu SQL duy nhất (khử N+1).
pub fn load_latest_jobs_map(conn: &Connection) -> HashMap<String, LatestJobInfo> {
    let mut map = HashMap::new();
    let sql = "SELECT id, row_key, status, image_status, video_status,
                      image_prompt_hash, video_prompt_hash, attempts_count,
                      last_error, video_task_id, video_download_url
               FROM jobs
               WHERE id IN (SELECT MAX(id) FROM jobs GROUP BY LOWER(row_key))";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return map;
    };
    let Ok(rows) = stmt.query_map([], |r| {
        let rk: String = r.get(1)?;
        Ok((
            rk.clone(),
            LatestJobInfo {
                id: r.get(0)?,
                row_key: rk,
                status: r.get(2)?,
                image_status: r.get(3)?,
                video_status: r.get(4)?,
                image_prompt_hash: r.get(5)?,
                video_prompt_hash: r.get(6)?,
                attempts_count: r.get(7)?,
                last_error: r.get(8)?,
                video_task_id: r.get(9)?,
                video_download_url: r.get(10)?,
            },
        ))
    }) else {
        return map;
    };

    for (key, info) in rows.flatten() {
        map.insert(key.to_lowercase(), info);
    }
    map
}

/// Truy vấn bản ghi `jobs` mới nhất cho một `row_key`
pub fn latest_job_info(conn: &Connection, row_key: &str) -> Option<LatestJobInfo> {
    conn.query_row(
        "SELECT id, row_key, status, image_status, video_status,
                image_prompt_hash, video_prompt_hash, attempts_count,
                last_error, video_task_id, video_download_url
         FROM jobs
         WHERE LOWER(row_key) = LOWER(?1)
         ORDER BY id DESC LIMIT 1",
        rusqlite::params![row_key],
        |r| {
            Ok(LatestJobInfo {
                id: r.get(0)?,
                row_key: r.get(1)?,
                status: r.get(2)?,
                image_status: r.get(3)?,
                video_status: r.get(4)?,
                image_prompt_hash: r.get(5)?,
                video_prompt_hash: r.get(6)?,
                attempts_count: r.get(7)?,
                last_error: r.get(8)?,
                video_task_id: r.get(9)?,
                video_download_url: r.get(10)?,
            })
        },
    )
    .ok()
}

pub fn start_steps(status: RowStatus) -> Vec<Step> {
    match status {
        RowStatus::Cho | RowStatus::LoiImage | RowStatus::LoiTamThoi => vec![Step::B2, Step::B3],
        RowStatus::DaTaoImage | RowStatus::LoiVideo => vec![Step::B3],
        RowStatus::DaTaoVideo => vec![],
    }
}

pub fn status_to_db(s: RowStatus) -> &'static str {
    match s {
        RowStatus::Cho => "dang_cho",
        RowStatus::DaTaoImage => "da_tao_image",
        RowStatus::DaTaoVideo => "da_tao_video",
        RowStatus::LoiImage => "loi_tao_image",
        RowStatus::LoiVideo => "loi_tao_video",
        RowStatus::LoiTamThoi => "loi_tam_thoi",
    }
}

pub fn row_status_to_label(s: RowStatus) -> &'static str {
    match s {
        RowStatus::Cho => "Đang chờ",
        RowStatus::DaTaoImage => "Đã tạo image",
        RowStatus::DaTaoVideo => "Đã tạo video",
        RowStatus::LoiImage => "Lỗi tạo image",
        RowStatus::LoiVideo => "Lỗi tạo video",
        RowStatus::LoiTamThoi => "Lỗi tạm thời",
    }
}

pub fn db_status_to_label(db_status: &str) -> &'static str {
    match db_status {
        "da_tao_image" => "Đã tạo image",
        "da_tao_video" => "Đã tạo video",
        "loi_tao_image" => "Lỗi tạo image",
        "loi_tao_video" => "Lỗi tạo video",
        "loi_tam_thoi" => "Lỗi tạm thời",
        "bo_qua" => "Bỏ qua",
        _ => "Đang chờ",
    }
}

pub fn effective_status_label(
    row: &prompts_csv::Row,
    prev: Option<&LatestJobInfo>,
) -> &'static str {
    if let Some(j) = prev {
        db_status_to_label(&j.status)
    } else {
        row_status_to_label(row.status())
    }
}

pub fn effective_row_status(
    row: &prompts_csv::Row,
    prev: Option<&LatestJobInfo>,
) -> Option<RowStatus> {
    if let Some(j) = prev {
        match j.status.as_str() {
            "bo_qua" => None,
            "da_tao_image" => Some(RowStatus::DaTaoImage),
            "da_tao_video" => Some(RowStatus::DaTaoVideo),
            "loi_tao_image" => Some(RowStatus::LoiImage),
            "loi_tao_video" => Some(RowStatus::LoiVideo),
            "loi_tam_thoi" => Some(RowStatus::LoiTamThoi),
            _ => Some(RowStatus::Cho),
        }
    } else {
        Some(row.status())
    }
}

/// Phát hiện prompt cột B / cột C bị sửa so với lần thực thi thành công gần nhất trong `state.db` (mục 13.4).
pub fn detect_prompt_change(
    row: &prompts_csv::Row,
    prev: Option<&LatestJobInfo>,
) -> (bool, bool) {
    let Some(p) = prev else {
        return (false, false);
    };
    let cur_img = sha2hex(row.promt_tao_image.trim());
    let cur_vid = sha2hex(row.promt_tao_video.trim());
    let img_changed = p
        .image_prompt_hash
        .as_deref()
        .is_some_and(|h| !h.is_empty() && h != cur_img);
    let vid_changed = p
        .video_prompt_hash
        .as_deref()
        .is_some_and(|h| !h.is_empty() && h != cur_vid);
    (img_changed, vid_changed)
}

pub fn is_prompt_modified(row: &prompts_csv::Row, prev: Option<&LatestJobInfo>) -> bool {
    let (ic, vc) = detect_prompt_change(row, prev);
    ic || vc
}

/// Kiểm tra thực tế xem `prompts.csv` có đang bị khóa ghi bởi ứng dụng ngoài (Excel) hay không (TC-44).
/// Đồng bộ với `GLOBAL_QUEUE` để không báo khóa giả tạo khi chính luồng của app đang `rename` CSV.
pub fn is_csv_write_locked(csv_path: &Path) -> bool {
    let _guard = crate::csv_writer::GLOBAL_QUEUE.lock();
    if !csv_path.is_file() {
        return false;
    }
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(csv_path)
        .is_err()
}

fn classify(fail: &ApiFail) -> &'static str {
    match fail.kind {
        ApiFailKind::TamThoi { .. } => "loi_tam_thoi",
        ApiFailKind::NghiepVu | ApiFailKind::HetQuota => "loi_nghiep_vu",
    }
}

struct AttemptRecord<'a> {
    step: &'a str,
    fallback_attempt_no: u32,
    result: &'a str,
    http_status: Option<i64>,
    error_code: &'a str,
    error_message: &'a str,
    count_toward_limit: bool,
}

/// Ghi vào `attempts` và (nếu `count_toward_limit == true`) cộng `attempts_count` trên `jobs` trong 1 transaction.
/// Trả về giá trị `jobs.attempts_count` mới nhất của dòng.
fn record_attempt(conn: &Connection, job_id: i64, rec: AttemptRecord<'_>) -> i64 {
    let ts = chrono::Local::now().to_rfc3339();
    let cur = current_job_attempts(conn, job_id);
    let new_total = if rec.count_toward_limit {
        cur + 1
    } else {
        cur
    };
    let attempt_no = if rec.count_toward_limit {
        new_total.max(1) as u32
    } else {
        rec.fallback_attempt_no.max(1)
    };

    if let Ok(tx) = conn.unchecked_transaction() {
        let _ = tx.execute(
            "INSERT INTO attempts(job_id, step, attempt_no, started_at, finished_at, result, http_status, error_code, error_message)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            rusqlite::params![
                job_id,
                rec.step,
                attempt_no,
                ts,
                ts,
                rec.result,
                rec.http_status,
                rec.error_code,
                rec.error_message
            ],
        );
        if rec.count_toward_limit {
            let _ = tx.execute(
                "UPDATE jobs SET attempts_count = ?1 WHERE id = ?2",
                rusqlite::params![new_total, job_id],
            );
        }
        let _ = tx.commit();
    }
    new_total
}

fn backoff_for(cfg: &Settings, attempt_no: u32) -> u64 {
    if cfg.backoff_s.is_empty() {
        return 30;
    }
    let idx = (attempt_no.saturating_sub(1) as usize).min(cfg.backoff_s.len() - 1);
    cfg.backoff_s[idx].max(1)
}

fn compute_retry_wait_s(cfg: &Settings, attempt_no: u32, retry_after_s: Option<u64>) -> u64 {
    retry_after_s
        .map(|s| s.clamp(1, MAX_RETRY_AFTER_S))
        .unwrap_or_else(|| backoff_for(cfg, attempt_no))
}

pub fn versioned_file_name(stem: &str, ver: i64, ext: &str) -> String {
    if ver <= 1 {
        format!("{stem}_gen.{ext}")
    } else {
        format!("{stem}_gen_v{ver}.{ext}")
    }
}

#[derive(Default)]
struct ReservedVersions {
    reserved_paths: std::collections::HashSet<String>,
    reserved_db_versions: std::collections::HashSet<(String, String, i64)>,
}

static VERSION_LOCK: std::sync::OnceLock<parking_lot::Mutex<ReservedVersions>> =
    std::sync::OnceLock::new();

fn version_registry() -> &'static parking_lot::Mutex<ReservedVersions> {
    VERSION_LOCK.get_or_init(|| parking_lot::Mutex::new(ReservedVersions::default()))
}

fn escape_sqlite_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

fn normalize_reserved_path(path: &Path) -> String {
    let s = path.to_string_lossy().into_owned();
    #[cfg(windows)]
    {
        s.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        s
    }
}

pub struct VersionReservation {
    pub version: i64,
    pub file_name: String,
    pub dest: PathBuf,
    reserved_path_key: String,
    reserved_db_key: (String, String, i64),
}

impl Drop for VersionReservation {
    fn drop(&mut self) {
        let mut reg = version_registry().lock();
        reg.reserved_paths.remove(&self.reserved_path_key);
        reg.reserved_db_versions.remove(&self.reserved_db_key);
    }
}

/// Chọn và đặt giữ phiên bản kế tiếp (chỉ giữ `VERSION_LOCK` trong lúc chọn số phiên bản,
/// nhả mutex trước I/O/mạng; giữ `VersionReservation` tới sau `tx.commit()` để chặn hai worker
/// cùng stem/row_key đè file hoặc va chạm `uq_refs_row_kind_ver`).
pub fn reserve_next_version(
    conn: &Connection,
    out_dir: &Path,
    row_key: &str,
    kind: &str,
) -> VersionReservation {
    reserve_next_version_with_keys(conn, out_dir, row_key, &[], kind)
}

fn reserve_next_version_with_keys(
    conn: &Connection,
    out_dir: &Path,
    row_key: &str,
    extra_row_keys: &[String],
    kind: &str,
) -> VersionReservation {
    let ext = if kind == "anh" { "png" } else { "mp4" };
    // row_key là "<tên ảnh>#<hash prompt>" → stem phải lấy từ tên ảnh, nếu không tên
    // file đầu ra sẽ dính hash và khác hoàn toàn quy ước `stem_gen.png`.
    let image_name = crate::prompts_csv::row_key_image_name(row_key);
    let stem = Path::new(image_name)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "anh".into());
    let stem_low = stem.to_lowercase();
    let row_key_low = row_key.to_lowercase();
    let stem_like = format!("{}\\_gen%", escape_sqlite_like(&stem_low));

    let mut reg = version_registry().lock();

    let mut db_max: i64 = conn
        .query_row(
            r#"SELECT COALESCE(MAX(version), 0) FROM file_refs
               WHERE (LOWER(row_key) = LOWER(?1) OR LOWER(file_name) LIKE LOWER(?2) ESCAPE '\')
                 AND kind = ?3"#,
            rusqlite::params![row_key, stem_like, kind],
            |r| r.get(0),
        )
        .unwrap_or(0);

    // Tính cả các `old_keys` sắp được migrate sang `row_key` trong transaction commit
    // (kể cả trường hợp va chạm version với `row_key` làm `migrate_file_refs_row_key` tăng version).
    let mut extra_lows: Vec<String> = Vec::new();
    if !extra_row_keys.is_empty() {
        let mut target_versions: HashMap<i64, String> = HashMap::new();
        if let Ok(mut stmt) = conn.prepare(
            "SELECT version, file_name FROM file_refs WHERE row_key = ?1 AND kind = ?2",
        ) {
            if let Ok(iter) = stmt.query_map(rusqlite::params![row_key, kind], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            }) {
                for (ver, fname) in iter.flatten() {
                    db_max = db_max.max(ver);
                    target_versions.insert(ver, fname);
                }
            }
        }
        let mut seen_extra: Vec<String> = Vec::new();
        for ek in extra_row_keys {
            let trimmed = ek.trim();
            if trimmed.is_empty()
                || trimmed == row_key
                || seen_extra.iter().any(|s| s.eq_ignore_ascii_case(trimmed))
            {
                continue;
            }
            seen_extra.push(trimmed.to_string());
            let low = trimmed.to_lowercase();
            if low != row_key_low && !extra_lows.contains(&low) {
                extra_lows.push(low);
            }
            if let Ok(mut stmt) = conn.prepare(
                "SELECT row_key, version, file_name
                 FROM file_refs
                 WHERE LOWER(row_key) = LOWER(?1) AND kind = ?2
                 ORDER BY version ASC, id ASC",
            ) {
                if let Ok(iter) = stmt.query_map(rusqlite::params![trimmed, kind], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                }) {
                    for (cur_rk, ver, fname) in iter.flatten() {
                        if cur_rk == row_key {
                            continue;
                        }
                        db_max = db_max.max(ver);
                        if let Some(existing_fname) = target_versions.get(&ver) {
                            if !existing_fname.eq_ignore_ascii_case(&fname) {
                                db_max += 1;
                                target_versions.insert(db_max, fname);
                            }
                        } else {
                            target_versions.insert(ver, fname);
                        }
                    }
                }
            }
        }
    }

    let v1_name = format!("{stem_low}_gen.{ext}");
    let vn_prefix = format!("{stem_low}_gen_v");
    let vn_suffix = format!(".{ext}");

    let mut disk_max: i64 = 0;
    if let Ok(entries) = std::fs::read_dir(out_dir) {
        for entry in entries.flatten() {
            let fname_low = entry.file_name().to_string_lossy().to_lowercase();
            // Bỏ hậu tố tạm `.part`, `.mp4.<pid>.<seq>.part`, `.vf_tmp_<ns>` để không ghi đè file đang ghi dở.
            let clean_low = if fname_low.contains(".vf_tmp_") {
                fname_low.split(".vf_tmp_").next().unwrap_or(&fname_low)
            } else if fname_low.ends_with(".part") {
                fname_low.split(".mp4.").next().unwrap_or(&fname_low)
            } else {
                &fname_low
            };
            if clean_low == v1_name || clean_low == format!("{stem_low}_gen") {
                disk_max = disk_max.max(1);
            } else if let Some(mid) = clean_low
                .strip_prefix(&vn_prefix)
                .map(|s| s.strip_suffix(&vn_suffix).unwrap_or(s))
            {
                if let Ok(v) = mid.parse::<i64>() {
                    disk_max = disk_max.max(v);
                }
            }
        }
    }

    let mut candidate = db_max.max(disk_max) + 1;
    let (file_name, dest, path_key, db_key) = loop {
        let fname = versioned_file_name(&stem, candidate, ext);
        let d = out_dir.join(&fname);
        let pk = normalize_reserved_path(&d);
        let dk = (row_key_low.clone(), kind.to_string(), candidate);
        let extra_reserved = extra_lows.iter().any(|ek_low| {
            reg.reserved_db_versions
                .contains(&(ek_low.clone(), kind.to_string(), candidate))
        });
        if !d.exists()
            && !reg.reserved_paths.contains(&pk)
            && !reg.reserved_db_versions.contains(&dk)
            && !extra_reserved
        {
            break (fname, d, pk, dk);
        }
        candidate += 1;
    };

    reg.reserved_paths.insert(path_key.clone());
    reg.reserved_db_versions.insert(db_key.clone());

    VersionReservation {
        version: candidate,
        file_name,
        dest,
        reserved_path_key: path_key,
        reserved_db_key: db_key,
    }
}

pub fn next_version(conn: &Connection, out_dir: &Path, row_key: &str, kind: &str) -> i64 {
    reserve_next_version(conn, out_dir, row_key, kind).version
}

fn write_atomic(dest: &Path, bytes: &[u8]) -> AppResult<()> {
    let part_ext = format!(
        "vf_tmp_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let tmp = dest.with_extension(part_ext);
    std::fs::write(&tmp, bytes).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        AppError::new("WRITE_FAIL", format!("Không ghi được file tạm: {e}"))
    })?;
    std::fs::rename(&tmp, dest).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        AppError::new("WRITE_FAIL", format!("Không đổi tên được file tạm: {e}"))
    })
}

pub fn should_reset_poll_timer(max_progress: &mut u32, progress: Option<u32>) -> bool {
    if let Some(p) = progress {
        if p > *max_progress {
            *max_progress = p;
            return true;
        }
    }
    false
}

fn format_rfc3339_to_csv_time(rfc_ts: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(rfc_ts)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%d/%m/%Y %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| rfc_ts.to_string())
}

/// Nhân thực thi cốt lõi của Run Engine (hỗ trợ hook nhận `run_id` tức thì và pipeline 2 tầng song song).
pub fn run_with_emitter_and_hook(
    emitter: EventEmitter<'_>,
    on_run_created: Option<&(dyn Fn(i64) + Send + Sync)>,
    cfg: &Settings,
    project_path: &Path,
    engine_cfg: EngineConfig,
    stop_flag: &Arc<AtomicBool>,
) -> AppResult<EngineOutcome> {
    let csv_path = project_path.join("prompts.csv");
    let mut rows = prompts_csv::read_all(&csv_path)?;

    let conn = crate::state_db::open(project_path)
        .map_err(|e| AppError::new("DB_FAIL", format!("Không mở được state.db: {e}")))?;

    // A confirmed STOP is an operation decision, not just a worker flag. Retry
    // any durable DELETE intents before planning new work after restart.
    if let Ok(pending) = crate::state_db::pending_plugin_cancellations(&conn) {
        for op in pending {
            if let Ok(result) = crate::plugins::client::cancel(&op.owner, &op.operation_id) {
                let status = result["status"].as_str().unwrap_or("reconcile_needed");
                let _ =
                    crate::state_db::confirm_plugin_operation_stop(&conn, &op.operation_id, status);
                let _ = crate::state_db::update_plugin_operation(
                    &conn,
                    &op.operation_id,
                    status,
                    None,
                    None,
                    None,
                );
            }
        }
    }

    let image_out = project_path.join("image_out");
    let video_out = project_path.join("video_out");
    let _ = std::fs::create_dir_all(&image_out);
    let _ = std::fs::create_dir_all(&video_out);

    // 13.4 + TC-44: đồng bộ trạng thái, thời gian & tên file sản phẩm từ state.db trước khi chạy
    let _ = sync_rows_from_state_db(project_path, &conn, &mut rows);
    let latest_map = load_latest_jobs_map(&conn);
    let media_map = load_current_media_map(&conn);
    let img_counts = image_counts(&rows);

    // B1: chọn dòng & lập kế hoạch bước chạy TRƯỚC khi tạo bản ghi mới
    let mut planned_rows: VecDeque<(prompts_csv::Row, RowPlan)> = VecDeque::new();
    let mut seen_keys = std::collections::HashSet::new();
    for r in &rows {
        // Khoá dòng (tên ảnh + hash prompt) — dùng tên ảnh thì hai dòng cùng ảnh sẽ
        // chia sẻ một bản ghi job và "dòng này xong thì dòng kia cũng xong".
        let key_low = row_key_low(r);
        if key_low.is_empty() || !seen_keys.insert(key_low.clone()) {
            continue;
        }
        let prev_owned = latest_job_for_csv_row(&conn, r, &latest_map, &img_counts);
        let prev = prev_owned.as_ref();
        // media_map dùng chung khoá với jobs; fallback theo job.row_key hoặc tên ảnh khi ảnh duy nhất.
        let db_img = media_map
            .get(&key_low)
            .or_else(|| prev.and_then(|p| media_map.get(&p.row_key.to_lowercase())))
            .or_else(|| map_lookup_legacy(&media_map, &rows, r))
            .and_then(|m| m.image_file.as_deref());
        let legacy_img = r.anh_tham_chieu.trim().to_lowercase();
        let same_image = img_counts.get(&legacy_img).copied().unwrap_or(0);
        if let Some(plan) = plan_row(r, prev, cfg, &engine_cfg, &image_out, db_img, same_image) {
            planned_rows.push_back((r.clone(), plan));
        }
    }

    let total = planned_rows.len();
    if total == 0 {
        return Err(AppError::new(
            "NO_ELIGIBLE_ROWS",
            "Không có dòng nào đủ điều kiện chạy (các dòng đã hoàn tất, bị bỏ qua, hoặc đã khóa 3/3 lần thử lỗi — hãy sửa prompt hoặc Đặt lại trạng thái nếu muốn chạy lại).",
        ));
    }

    let has_noi_canh = planned_rows.iter().any(|(r, _)| r.noi_canh);
    let ffmpeg_check = if cfg.auto_merge || has_noi_canh {
        Some(crate::ffmpeg::resolve_ffmpeg_binary(&cfg.ffmpeg_path))
    } else {
        None
    };
    if cfg.auto_merge {
        if let Some(Err(msg)) = &ffmpeg_check {
            return Err(AppError::new(
                "FFMPEG_MISSING",
                format!(
                    "Đang bật 'Tự động ghép video sau batch' nhưng không tìm thấy FFmpeg: {msg}"
                ),
            ));
        }
    }

    let ts = chrono::Local::now().to_rfc3339();
    let run_kind = if engine_cfg.kind.is_empty() {
        "chung"
    } else {
        engine_cfg.kind
    };
    conn.execute(
        "INSERT INTO runs(kind, status, started_at, total_rows) VALUES (?1,'dang_chay',?2,?3)",
        rusqlite::params![run_kind, ts, total as i64],
    )
    .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    let run_id = conn.last_insert_rowid();
    if let Some(cb) = on_run_created {
        cb(run_id);
    }

    emit(
        emitter,
        "run_started",
        &RunStartedEvent {
            run_id,
            kind: run_kind.into(),
            total,
        },
    );

    // Tách pipeline 2 tầng độc lập:
    // - Tầng tạo ảnh (B2) chạy song song liên tục, không chờ tầng tạo video (B3).
    // - Tầng tạo video (B3) đợi từng dòng có ảnh xong (hoặc chạy ngay với dòng đã có ảnh / không cần B2).
    // - Nếu batch có ít nhất 1 dòng `noi_canh = true`, riêng tầng video ép 1 luồng theo đúng thứ tự CSV
    //   để đảm bảo video cảnh trước luôn xong trước khi trích frame cuối cho cảnh sau, trong khi luồng ảnh
    //   vẫn tiếp tục chạy song song phía trước.
    if has_noi_canh {
        if let Some(Err(msg)) = &ffmpeg_check {
            crate::settings::log(
                &conn,
                "warn",
                Some(run_id),
                None,
                Some("B1"),
                &format!(
                    "Batch có dòng nối cảnh (noi_canh=1) nhưng chưa tìm thấy FFmpeg ({msg}) — các cảnh nối sẽ fallback dùng ảnh gốc của dòng."
                ),
            );
        }
    }

    let mut image_items: VecDeque<ImageWorkItem> = VecDeque::new();
    let mut initial_video_items: Vec<VideoWorkItem> = Vec::new();
    let mut b3_count: usize = 0;

    for (seq_idx, (row, plan)) in planned_rows.into_iter().enumerate() {
        let has_b2 = plan.steps.first() == Some(&Step::B2);
        let has_b3 = plan.steps.contains(&Step::B3);
        if has_b3 {
            b3_count += 1;
        }
        if has_b2 {
            image_items.push_back(ImageWorkItem { seq_idx, row, plan });
        } else if has_b3 {
            initial_video_items.push(VideoWorkItem {
                seq_idx,
                row,
                plan,
                job_id: None,
                b2_duration_ms: 0,
                b2_completed: false,
            });
        }
    }

    let muse_counts = if cfg.video_source == "internal" || cfg.image_source == "internal" {
        Some(crate::muse_sidecar::inspect_account_counts(cfg))
    } else {
        None
    };
    let muse_avail = muse_counts.map(|c| c.available_now);

    let (image_worker_count, video_worker_count) = compute_pipeline_workers(
        cfg,
        image_items.len(),
        b3_count,
        has_noi_canh,
        muse_avail,
    );

    if cfg.video_source == "internal" && !has_noi_canh && b3_count > 0 {
        let requested = (cfg.concurrency.clamp(1, crate::settings::MAX_CONCURRENCY) as usize)
            .min(b3_count);
        if video_worker_count < requested {
            let avail_n = muse_avail.unwrap_or(0);
            crate::settings::log(
                &conn,
                "info",
                Some(run_id),
                None,
                Some("B1"),
                &format!(
                    "Giới hạn số luồng video về {video_worker_count} (cấu hình: {requested}) theo số tài khoản Muse khả dụng ({avail_n}) để đảm bảo mỗi tài khoản chỉ chạy tối đa 1 video."
                ),
            );
        }
    }

    let image_queue = parking_lot::Mutex::new(image_items);
    let video_queue = VideoPipelineQueue::new(has_noi_canh, total, image_worker_count);
    for item in initial_video_items {
        video_queue.push_ready(item);
    }

    let active_rows: parking_lot::Mutex<Vec<ActiveRowProgress>> =
        parking_lot::Mutex::new(Vec::new());
    let done_count = AtomicUsize::new(0);
    let error_count = AtomicUsize::new(0);
    let stopped = AtomicBool::new(false);
    let fatal_err: parking_lot::Mutex<Option<AppError>> = parking_lot::Mutex::new(None);
    let run_start = Instant::now();

    let shared = PipelineShared {
        emitter,
        cfg,
        project_path,
        csv_path: &csv_path,
        image_out: &image_out,
        video_out: &video_out,
        stop_flag: stop_flag.as_ref(),
        run_id,
        total,
        done_count: &done_count,
        error_count: &error_count,
        stopped: &stopped,
        active_rows: &active_rows,
        fatal_err: &fatal_err,
        video_queue: &video_queue,
    };

    std::thread::scope(|scope| {
        // ── Tầng 1: Luồng tạo ảnh (B2) chạy song song, không đợi video ──
        for _img_worker_idx in 0..image_worker_count {
            let shared_ref = &shared;
            let img_queue_ref = &image_queue;

            scope.spawn(move || {
                let _drop_guard = ImageWorkerDropGuard(shared_ref.video_queue);
                let w_conn = match crate::state_db::open(shared_ref.project_path) {
                    Ok(c) => c,
                    Err(e) => {
                        *shared_ref.fatal_err.lock() = Some(AppError::new(
                            "DB_FAIL",
                            format!("Worker tạo ảnh không mở được state.db: {e}"),
                        ));
                        shared_ref.stopped.store(true, Ordering::SeqCst);
                        shared_ref.video_queue.request_stop();
                        return;
                    }
                };

                loop {
                    if shared_ref.stop_flag.load(Ordering::SeqCst)
                        || shared_ref.stopped.load(Ordering::SeqCst)
                    {
                        shared_ref.stopped.store(true, Ordering::SeqCst);
                        shared_ref.video_queue.request_stop();
                        break;
                    }

                    let Some(ImageWorkItem {
                        seq_idx,
                        mut row,
                        plan,
                    }) = img_queue_ref.lock().pop_front()
                    else {
                        break;
                    };

                    if shared_ref.stop_flag.load(Ordering::SeqCst)
                        || shared_ref.stopped.load(Ordering::SeqCst)
                    {
                        shared_ref.video_queue.mark_skipped(seq_idx);
                        shared_ref.stopped.store(true, Ordering::SeqCst);
                        shared_ref.video_queue.request_stop();
                        break;
                    }

                    let row_key = crate::prompts_csv::row_key_of(
                        &row.anh_tham_chieu,
                        &row.promt_tao_image,
                        &row.promt_tao_video,
                    );
                    let step_timer = Instant::now();

                    let job_id = match insert_job_for_row(
                        &w_conn,
                        shared_ref.run_id,
                        &row,
                        &row_key,
                        &plan,
                        shared_ref.image_out,
                    ) {
                        Ok(id) => id,
                        Err(e) => {
                            crate::settings::log(
                                &w_conn,
                                "error",
                                Some(shared_ref.run_id),
                                Some(&row_key),
                                Some("B1"),
                                &format!("Không chèn được bản ghi job: {e}"),
                            );
                            shared_ref.error_count.fetch_add(1, Ordering::SeqCst);
                            update_runs_progress(
                                &w_conn,
                                shared_ref.run_id,
                                shared_ref.done_count,
                                shared_ref.error_count,
                            );
                            shared_ref.video_queue.mark_skipped(seq_idx);
                            continue;
                        }
                    };

                    let ctx = WorkerRowCtx {
                        emitter: shared_ref.emitter,
                        cfg: shared_ref.cfg,
                        conn: &w_conn,
                        csv_path: shared_ref.csv_path,
                        image_out: shared_ref.image_out,
                        video_out: shared_ref.video_out,
                        stop_flag: shared_ref.stop_flag,
                        run_id: shared_ref.run_id,
                        job_id,
                        total: shared_ref.total,
                        done_count: shared_ref.done_count,
                        active_rows: shared_ref.active_rows,
                    };

                    ctx.emit_step_progress(&row_key, "B2", "Đang tạo image".to_string());

                    match run_b2(&ctx, &mut row, plan.force_new_image) {
                        Ok(next_status) => {
                            let b2_dur_ms = step_timer.elapsed().as_millis() as i64;
                            let ts_done = chrono::Local::now().to_rfc3339();
                            let _ = w_conn.execute(
                                "UPDATE jobs SET finished_at = ?1, duration_ms = ?2 WHERE id = ?3",
                                rusqlite::params![ts_done, b2_dur_ms, job_id],
                            );

                            let has_b3 = plan.steps.contains(&Step::B3);
                            let (img_st, vid_st) = match next_status {
                                RowStatus::DaTaoImage => ("xong", "chua_chay"),
                                RowStatus::DaTaoVideo => ("xong", "xong"),
                                _ => ("chua_chay", "chua_chay"),
                            };

                            // Xóa khỏi active_rows TRƯỚC khi đẩy sang hàng đợi video để tránh
                            // xóa nhầm trạng thái B3 nếu worker video nhận dòng ngay lập tức.
                            ctx.clear_active_row(&row_key);

                            emit(
                                shared_ref.emitter,
                                "row_changed",
                                &RowChangedEvent {
                                    row_key: row_key.clone(),
                                    status: row_status_to_label(next_status).into(),
                                    image_status: img_st.into(),
                                    video_status: vid_st.into(),
                                    attempts_count: current_job_attempts(&w_conn, job_id),
                                    last_error: None,
                                },
                            );

                            if has_b3 {
                                ctx.emit_active_snapshot(&row_key);
                                if shared_ref.stop_flag.load(Ordering::SeqCst)
                                    || shared_ref.stopped.load(Ordering::SeqCst)
                                {
                                    shared_ref.video_queue.mark_skipped(seq_idx);
                                    shared_ref.stopped.store(true, Ordering::SeqCst);
                                    shared_ref.video_queue.request_stop();
                                    break;
                                }
                                shared_ref.video_queue.push_ready(VideoWorkItem {
                                    seq_idx,
                                    row,
                                    plan,
                                    job_id: Some(job_id),
                                    b2_duration_ms: b2_dur_ms,
                                    b2_completed: true,
                                });
                            } else {
                                shared_ref.done_count.fetch_add(1, Ordering::SeqCst);
                                update_runs_progress(
                                    &w_conn,
                                    shared_ref.run_id,
                                    shared_ref.done_count,
                                    shared_ref.error_count,
                                );
                                ctx.emit_active_snapshot(&row_key);
                                shared_ref.video_queue.mark_skipped(seq_idx);
                            }
                        }
                        Err(fail) => {
                            let dur_ms = step_timer.elapsed().as_millis() as i64;
                            let should_stop = handle_step_failure(
                                shared_ref,
                                &ctx,
                                &mut row,
                                StepFailInput {
                                    step: Step::B2,
                                    fail: &fail,
                                    row_key: &row_key,
                                    plan: &plan,
                                    b2_completed_in_this_job: false,
                                    duration_ms: dur_ms,
                                },
                            );
                            shared_ref.video_queue.mark_skipped(seq_idx);
                            if should_stop || shared_ref.stopped.load(Ordering::SeqCst) {
                                break;
                            }
                        }
                    }
                }
            });
        }

        // ── Tầng 2: Luồng tạo video (B3) nhận từng dòng ngay khi có ảnh ──
        for _vid_worker_idx in 0..video_worker_count {
            let shared_ref = &shared;

            scope.spawn(move || {
                let w_conn = match crate::state_db::open(shared_ref.project_path) {
                    Ok(c) => c,
                    Err(e) => {
                        *shared_ref.fatal_err.lock() = Some(AppError::new(
                            "DB_FAIL",
                            format!("Worker tạo video không mở được state.db: {e}"),
                        ));
                        shared_ref.stopped.store(true, Ordering::SeqCst);
                        shared_ref.video_queue.request_stop();
                        return;
                    }
                };

                while let Some(VideoWorkItem {
                    seq_idx: _,
                    mut row,
                    plan,
                    job_id,
                    b2_duration_ms,
                    b2_completed,
                }) = shared_ref
                    .video_queue
                    .pop_next(shared_ref.stop_flag, shared_ref.stopped)
                {

                    if shared_ref.stop_flag.load(Ordering::SeqCst)
                        || shared_ref.stopped.load(Ordering::SeqCst)
                    {
                        shared_ref.stopped.store(true, Ordering::SeqCst);
                        shared_ref.video_queue.request_stop();
                        break;
                    }

                    let row_key = crate::prompts_csv::row_key_of(
                        &row.anh_tham_chieu,
                        &row.promt_tao_image,
                        &row.promt_tao_video,
                    );
                    let b3_timer = Instant::now();

                    let job_id = match job_id {
                        Some(id) => id,
                        None => match insert_job_for_row(
                            &w_conn,
                            shared_ref.run_id,
                            &row,
                            &row_key,
                            &plan,
                            shared_ref.image_out,
                        ) {
                            Ok(id) => id,
                            Err(e) => {
                                crate::settings::log(
                                    &w_conn,
                                    "error",
                                    Some(shared_ref.run_id),
                                    Some(&row_key),
                                    Some("B1"),
                                    &format!("Không chèn được bản ghi job: {e}"),
                                );
                                shared_ref.error_count.fetch_add(1, Ordering::SeqCst);
                                update_runs_progress(
                                    &w_conn,
                                    shared_ref.run_id,
                                    shared_ref.done_count,
                                    shared_ref.error_count,
                                );
                                continue;
                            }
                        },
                    };

                    let ctx = WorkerRowCtx {
                        emitter: shared_ref.emitter,
                        cfg: shared_ref.cfg,
                        conn: &w_conn,
                        csv_path: shared_ref.csv_path,
                        image_out: shared_ref.image_out,
                        video_out: shared_ref.video_out,
                        stop_flag: shared_ref.stop_flag,
                        run_id: shared_ref.run_id,
                        job_id,
                        total: shared_ref.total,
                        done_count: shared_ref.done_count,
                        active_rows: shared_ref.active_rows,
                    };

                    ctx.emit_step_progress(&row_key, "B3", "Đang tạo video".to_string());

                    match run_b3(&ctx, &mut row, plan.force_new_video) {
                        Ok(next_status) => {
                            let dur_ms =
                                b2_duration_ms + b3_timer.elapsed().as_millis() as i64;
                            let ts_done = chrono::Local::now().to_rfc3339();
                            let _ = w_conn.execute(
                                "UPDATE jobs SET finished_at = ?1, duration_ms = ?2 WHERE id = ?3",
                                rusqlite::params![ts_done, dur_ms, job_id],
                            );
                            let (img_st, vid_st) = match next_status {
                                RowStatus::DaTaoImage => ("xong", "chua_chay"),
                                RowStatus::DaTaoVideo => ("xong", "xong"),
                                _ => ("chua_chay", "chua_chay"),
                            };
                            ctx.clear_active_row(&row_key);
                            shared_ref.done_count.fetch_add(1, Ordering::SeqCst);
                            update_runs_progress(
                                &w_conn,
                                shared_ref.run_id,
                                shared_ref.done_count,
                                shared_ref.error_count,
                            );
                            emit(
                                shared_ref.emitter,
                                "row_changed",
                                &RowChangedEvent {
                                    row_key: row_key.clone(),
                                    status: row_status_to_label(next_status).into(),
                                    image_status: img_st.into(),
                                    video_status: vid_st.into(),
                                    attempts_count: current_job_attempts(&w_conn, job_id),
                                    last_error: None,
                                },
                            );
                            ctx.emit_active_snapshot(&row_key);
                        }
                        Err(fail) => {
                            let dur_ms =
                                b2_duration_ms + b3_timer.elapsed().as_millis() as i64;
                            let should_stop = handle_step_failure(
                                shared_ref,
                                &ctx,
                                &mut row,
                                StepFailInput {
                                    step: Step::B3,
                                    fail: &fail,
                                    row_key: &row_key,
                                    plan: &plan,
                                    b2_completed_in_this_job: b2_completed,
                                    duration_ms: dur_ms,
                                },
                            );
                            if should_stop || shared_ref.stopped.load(Ordering::SeqCst) {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });

    if let Some(err) = fatal_err.into_inner() {
        let ts_err = chrono::Local::now().to_rfc3339();
        let _ = conn.execute(
            "UPDATE runs SET status = 'gian_doan', finished_at = ?1 WHERE id = ?2",
            rusqlite::params![ts_err, run_id],
        );
        return Err(err);
    }

    let final_done = done_count.load(Ordering::SeqCst);
    let final_errors = error_count.load(Ordering::SeqCst);
    let was_stopped = stopped.load(Ordering::SeqCst) || stop_flag.load(Ordering::SeqCst);
    let final_status = if was_stopped { "da_dung" } else { "hoan_tat" };
    let ts = chrono::Local::now().to_rfc3339();
    let _ = conn.execute(
        "UPDATE runs SET status = ?1, finished_at = ?2, done_count = ?3, error_count = ?4 WHERE id = ?5",
        rusqlite::params![final_status, ts, final_done as i64, final_errors as i64, run_id],
    );

    if cfg.auto_cleanup_days > 0 {
        let _ = crate::state_db::cleanup_old_history(&conn, cfg.auto_cleanup_days);
    }

    if cfg.auto_merge && !was_stopped {
        match auto_merge_batch_with_stop(
            project_path,
            Some(run_id),
            Some(&conn),
            cfg,
            Some(stop_flag.as_ref()),
        ) {
            Ok(_) => {}
            Err(e) if e.starts_with("STOPPED:") => {
                crate::settings::log(
                    &conn,
                    "info",
                    Some(run_id),
                    None,
                    Some("auto_merge"),
                    "Đã dừng bước tự động ghép video theo yêu cầu người dùng. Các clip lẻ thành phẩm vẫn được giữ nguyên.",
                );
            }
            Err(e) => {
                crate::settings::log(
                    &conn,
                    "warn",
                    Some(run_id),
                    None,
                    Some("auto_merge"),
                    &format!("Tự động ghép nối video thất bại: {e}. Toàn bộ các clip lẻ thành phẩm vẫn được giữ nguyên."),
                );
            }
        }
    }

    let duration_ms = run_start.elapsed().as_millis() as i64;
    Ok(EngineOutcome {
        run_id,
        status: final_status.into(),
        done: final_done,
        errors: final_errors,
        stopped: was_stopped,
        duration_ms,
    })
}

fn current_job_attempts(conn: &Connection, job_id: i64) -> i64 {
    conn.query_row(
        "SELECT attempts_count FROM jobs WHERE id = ?1",
        rusqlite::params![job_id],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

pub fn sha2hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

// ━━ B2: tạo ảnh (5.1, 5.5) + cập nhật row trong RAM + SQLite transaction ━━

fn plugin_create_image(
    ctx: &WorkerRowCtx<'_>,
    lease: &crate::plugins::routing::Lease,
    op: &crate::state_db::PluginOperation,
    image: &Path,
    prompt: &str,
) -> Result<Vec<u8>, ApiFail> {
    let model = serde_json::from_str::<serde_json::Value>(&op.request_json)
        .ok()
        .and_then(|v| v["model"].as_str().map(str::to_owned))
        .unwrap_or_else(|| lease.model.clone());
    // A saved operation is resolved by GET first. A transport error from POST
    // is ambiguous, so this invocation never follows it with another POST.
    let mut value = if plugin_operation_can_retry(op) {
        crate::plugins::client::submit_image(
            ctx.cfg,
            &lease.owner,
            &op.operation_id,
            &model,
            image,
            prompt,
        )
        .map_err(|e| persist_plugin_operation_error(ctx, op, &e, "reconcile_needed"))?
    } else if op.status != "prepared" {
        match crate::plugins::client::lookup(&lease.owner, &op.operation_id) {
            Ok(v) => v,
            Err(err) if err.http == Some(404) => {
                return Err(persist_plugin_operation_error(
                    ctx,
                    op,
                    &err,
                    "reconcile_needed",
                ))
            }
            Err(err) => {
                crate::plugins::routing::block(&lease.owner, "image", &err);
                return Err(err.as_api_fail());
            }
        }
    } else {
        crate::state_db::update_plugin_operation(
            ctx.conn,
            &op.operation_id,
            "submitting",
            None,
            None,
            None,
        )
        .map_err(|e| ApiFail {
            kind: ApiFailKind::NghiepVu,
            http_status: None,
            code: "DB_TX_FAIL".into(),
            message: e.to_string(),
        })?;
        match crate::plugins::client::submit_image(
            ctx.cfg,
            &lease.owner,
            &op.operation_id,
            &model,
            image,
            prompt,
        ) {
            Ok(v) => v,
            Err(err) => {
                crate::plugins::routing::block(&lease.owner, "image", &err);
                match crate::plugins::client::lookup(&lease.owner, &op.operation_id) {
                    Ok(v) => v,
                    Err(lookup_err) => {
                        crate::plugins::routing::block(&lease.owner, "image", &lookup_err);
                        return Err(persist_plugin_operation_error(
                            ctx,
                            op,
                            &err,
                            "reconcile_needed",
                        ));
                    }
                }
            }
        }
    };
    if value.get("status").is_none() && value["data"][0]["b64_json"].as_str().is_some() {
        value["status"] = serde_json::Value::String("succeeded".into());
    }
    let poll_started = Instant::now();
    loop {
        if ctx.stop_flag.load(Ordering::SeqCst) {
            stop_plugin_operation(ctx, op)?;
            return Err(ApiFail::stopped(
                "Đang chờ plugin xác nhận dừng thao tác ảnh",
            ));
        }
        match value["status"].as_str().unwrap_or("") {
            "succeeded" => {
                let data = value["data"][0]["b64_json"]
                    .as_str()
                    .ok_or_else(|| ApiFail {
                        kind: ApiFailKind::NghiepVu,
                        http_status: None,
                        code: "PLUGIN_BAD_RESPONSE".into(),
                        message: "Plugin thiếu ảnh kết quả".into(),
                    })?;
                let raw_bytes = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map_err(|e| ApiFail {
                        kind: ApiFailKind::NghiepVu,
                        http_status: None,
                        code: "PLUGIN_BAD_RESPONSE".into(),
                        message: e.to_string(),
                    })?;
                let bytes = adapters::ensure_png_bytes(raw_bytes, 200)?;
                crate::state_db::update_plugin_operation(
                    ctx.conn,
                    &op.operation_id,
                    "succeeded",
                    None,
                    None,
                    None,
                )
                .map_err(|e| ApiFail {
                    kind: ApiFailKind::NghiepVu,
                    http_status: None,
                    code: "DB_TX_FAIL".into(),
                    message: e.to_string(),
                })?;
                return Ok(bytes);
            }
            "failed" | "reconcile_needed" | "cancelled" | "abandoned" => {
                let error = value.get("error").cloned().unwrap_or_default();
                let status = value["status"].as_str().unwrap_or("failed");
                crate::state_db::update_plugin_operation(
                    ctx.conn,
                    &op.operation_id,
                    status,
                    None,
                    None,
                    Some(&error.to_string()),
                )
                .map_err(|e| ApiFail {
                    kind: ApiFailKind::NghiepVu,
                    http_status: None,
                    code: "DB_TX_FAIL".into(),
                    message: e.to_string(),
                })?;
                let failure = crate::plugins::client::PluginError::from_error(&error, None);
                crate::plugins::routing::block(&op.owner, &op.media_kind, &failure);
                return Err(failure.as_api_fail());
            }
            "processing" => {
                crate::state_db::update_plugin_operation(
                    ctx.conn,
                    &op.operation_id,
                    "processing",
                    None,
                    None,
                    None,
                )
                .map_err(|e| ApiFail {
                    kind: ApiFailKind::NghiepVu,
                    http_status: None,
                    code: "DB_TX_FAIL".into(),
                    message: e.to_string(),
                })?;
                if poll_started.elapsed().as_secs() >= 1800 {
                    return Err(ApiFail {
                        kind: ApiFailKind::TamThoi {
                            retry_after_s: None,
                        },
                        http_status: None,
                        code: "POLL_TIMEOUT".into(),
                        message: "Ảnh vẫn đang xử lý; operation được giữ để tiếp tục poll".into(),
                    });
                }
                if adapters::sleep_interruptible(Duration::from_secs(2), ctx.stop_flag) {
                    continue;
                }
                value = match crate::plugins::client::lookup(&lease.owner, &op.operation_id) {
                    Ok(v) => v,
                    Err(err) => {
                        crate::plugins::routing::block(&lease.owner, "image", &err);
                        return Err(persist_plugin_operation_error(
                            ctx,
                            op,
                            &err,
                            "reconcile_needed",
                        ));
                    }
                };
            }
            _ => {
                return Err(ApiFail {
                    kind: ApiFailKind::NghiepVu,
                    http_status: None,
                    code: "PLUGIN_BAD_RESPONSE".into(),
                    message: "Plugin trả trạng thái operation không hợp lệ".into(),
                })
            }
        }
    }
}

fn stop_plugin_operation(
    ctx: &WorkerRowCtx<'_>,
    op: &crate::state_db::PluginOperation,
) -> Result<(), ApiFail> {
    crate::state_db::request_plugin_operation_stop(ctx.conn, &op.operation_id).map_err(|e| {
        ApiFail {
            kind: ApiFailKind::NghiepVu,
            http_status: None,
            code: "DB_TX_FAIL".into(),
            message: e.to_string(),
        }
    })?;
    match crate::plugins::client::cancel(&op.owner, &op.operation_id) {
        Ok(v) => {
            let status = v["status"].as_str().unwrap_or("reconcile_needed");
            crate::state_db::confirm_plugin_operation_stop(ctx.conn, &op.operation_id, status)
                .map_err(|e| ApiFail {
                    kind: ApiFailKind::NghiepVu,
                    http_status: None,
                    code: "DB_TX_FAIL".into(),
                    message: e.to_string(),
                })?;
            crate::state_db::update_plugin_operation(
                ctx.conn,
                &op.operation_id,
                status,
                None,
                None,
                None,
            )
            .map_err(|e| ApiFail {
                kind: ApiFailKind::NghiepVu,
                http_status: None,
                code: "DB_TX_FAIL".into(),
                message: e.to_string(),
            })?;
        }
        Err(e) => crate::plugins::routing::block(&op.owner, &op.media_kind, &e),
    }
    Ok(())
}

fn persist_plugin_operation_error(
    ctx: &WorkerRowCtx<'_>,
    op: &crate::state_db::PluginOperation,
    error: &crate::plugins::client::PluginError,
    status: &str,
) -> ApiFail {
    crate::plugins::routing::block(&op.owner, &op.media_kind, error);
    let snapshot = serde_json::json!({
        "kind":error.kind,"scope":error.scope,"code":error.code,"message":error.message,
        "retryable":error.retryable,"retry_after_s":error.retry_after_s,
        "recoverable_media_id":error.recoverable_media_id,
    })
    .to_string();
    match crate::state_db::update_plugin_operation(
        ctx.conn,
        &op.operation_id,
        status,
        None,
        None,
        Some(&snapshot),
    ) {
        Ok(()) => error.as_api_fail(),
        Err(e) => ApiFail {
            kind: ApiFailKind::NghiepVu,
            http_status: None,
            code: "DB_TX_FAIL".into(),
            message: format!("Không lưu được trạng thái operation: {e}"),
        },
    }
}

fn plugin_operation_can_retry(op: &crate::state_db::PluginOperation) -> bool {
    op.status == "failed"
        && op
            .error_json
            .as_deref()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .is_some_and(|e| e["kind"] == "transient" && e["retryable"] == true)
}

fn run_b2(
    ctx: &WorkerRowCtx<'_>,
    row: &mut prompts_csv::Row,
    force_new: bool,
) -> Result<RowStatus, ApiFail> {
    // Tên ảnh (file trong input/) — KHÔNG phải khóa dòng.
    let image_name = row.anh_tham_chieu.clone();
    let row_key = crate::prompts_csv::row_key_of(
        &row.anh_tham_chieu,
        &row.promt_tao_image,
        &row.promt_tao_video,
    );
    let ref_path = ctx
        .csv_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("input")
        .join(&image_name);
    if !ref_path.is_file() {
        return Err(ApiFail {
            kind: ApiFailKind::NghiepVu,
            http_status: None,
            code: "REF_IMAGE_MISSING".into(),
            message: format!(
                "Thiếu ảnh tham chiếu '{row_key}' trong input/ - bổ sung file vào input/ rồi chạy lại (không tính lần thử)"
            ),
        });
    }

    let cur_img_hash = sha2hex(row.promt_tao_image.trim());

    let input_bytes = std::fs::read(&ref_path).map_err(|e| ApiFail {
        kind: ApiFailKind::NghiepVu,
        http_status: None,
        code: "IMAGE_READ_FAIL".into(),
        message: e.to_string(),
    })?;
    let image_hash = sha2hex(&base64::engine::general_purpose::STANDARD.encode(&input_bytes));
    let prior_plugin_op = if force_new {
        None
    } else {
        crate::state_db::load_plugin_operation(ctx.conn, &row_key, "B2").map_err(|e| ApiFail {
            kind: ApiFailKind::NghiepVu,
            http_status: None,
            code: "DB_TX_FAIL".into(),
            message: format!("Không đọc được operation ảnh: {e}"),
        })?
    };
    let reusable_plugin_op = prior_plugin_op.filter(|op| {
        op.media_kind == "image"
            && serde_json::from_str::<serde_json::Value>(&op.request_json)
                .ok()
                .is_some_and(|v| {
                    v["prompt"] == row.promt_tao_image && v["image_sha256"] == image_hash
                })
    });
    let use_plugin =
        crate::plugins::routing::is_plugin(&ctx.cfg.image_source) || reusable_plugin_op.is_some();
    let plugin_lease = if use_plugin {
        Some(
            if let Some(op) = reusable_plugin_op.as_ref() {
                crate::plugins::routing::hold_owner(&op.owner, "image", ctx.stop_flag)
            } else {
                crate::plugins::routing::acquire(&ctx.cfg.image_source, "image", ctx.stop_flag)
            }
            .map_err(|e| e.as_api_fail())?,
        )
    } else {
        None
    };
    let mut plugin_op = if let Some(lease) = plugin_lease.as_ref() {
        let snap_model = reusable_plugin_op
            .as_ref()
            .and_then(|op| serde_json::from_str::<serde_json::Value>(&op.request_json).ok())
            .and_then(|v| v["model"].as_str().map(str::to_owned))
            .unwrap_or_else(|| lease.model.clone());
        let request = serde_json::json!({"model":snap_model,"prompt":row.promt_tao_image,"image_sha256":image_hash});
        let fingerprint = sha2hex(&request.to_string());
        Some(if let Some(op) = reusable_plugin_op {
            let _ = ctx.conn.execute(
                "UPDATE plugin_operations SET job_id=?1,updated_at=?2 WHERE operation_id=?3",
                rusqlite::params![
                    ctx.job_id,
                    chrono::Local::now().to_rfc3339(),
                    op.operation_id
                ],
            );
            op
        } else {
            crate::state_db::prepare_plugin_operation(
                ctx.conn,
                ctx.job_id,
                &row_key,
                "B2",
                &lease.owner,
                "image",
                &request.to_string(),
                &fingerprint,
                force_new,
            )
            .map_err(|e| ApiFail {
                kind: ApiFailKind::NghiepVu,
                http_status: None,
                code: "DB_TX_FAIL".into(),
                message: format!("Không lưu được operation trước khi gửi yêu cầu: {e}"),
            })?
        })
    } else {
        None
    };

    if !force_new
        && matches!(row.status(), RowStatus::DaTaoImage | RowStatus::DaTaoVideo)
        && !row.anh_da_tao.is_empty()
        && ctx.image_out.join(&row.anh_da_tao).is_file()
    {
        let _ = ctx.conn.execute(
            "UPDATE jobs SET status = 'da_tao_image', image_status = 'xong', image_prompt_hash = ?1, last_error = NULL WHERE id = ?2",
            rusqlite::params![cur_img_hash, ctx.job_id],
        );
        return Ok(RowStatus::DaTaoImage);
    }

    let _ = ctx.conn.execute(
        "UPDATE jobs SET image_status = 'dang_chay' WHERE id = ?1",
        rusqlite::params![ctx.job_id],
    );

    let max_attempts = ctx.cfg.max_attempts.max(1) as i64;
    let mut local_try: u32 = 0;
    let bytes = loop {
        if ctx.stop_flag.load(Ordering::SeqCst) {
            if let Some(op) = plugin_op.as_ref() {
                stop_plugin_operation(ctx, op)?;
            }
            return Err(ApiFail::stopped("Đã dừng an toàn trước khi gọi tạo ảnh"));
        }
        local_try += 1;
        let create_result = if let (Some(lease), Some(op)) =
            (plugin_lease.as_ref(), plugin_op.as_ref())
        {
            plugin_create_image(ctx, lease, op, &ref_path, &row.promt_tao_image)
        } else {
            adapters::create_image(ctx.cfg, &ref_path, &row.promt_tao_image).map(|r| r.png_bytes)
        };
        if let Some(op) = plugin_op.as_mut() {
            op.status = "submitting".into();
        }
        match create_result {
            Ok(bytes) => break bytes,
            Err(fail) => {
                let transient = is_transient_error(&fail);
                let _ = record_attempt(
                    ctx.conn,
                    ctx.job_id,
                    AttemptRecord {
                        step: "B2",
                        fallback_attempt_no: local_try,
                        result: classify(&fail),
                        http_status: fail.http_status.map(|s| s as i64),
                        error_code: &fail.code,
                        error_message: &fail.message,
                        count_toward_limit: !transient,
                    },
                );
                match fail.kind {
                    ApiFailKind::NghiepVu | ApiFailKind::HetQuota => return Err(fail),
                    ApiFailKind::TamThoi { retry_after_s } => {
                        if local_try >= max_attempts as u32 {
                            return Err(fail);
                        }
                        let wait = compute_retry_wait_s(ctx.cfg, local_try, retry_after_s);
                        ctx.emit_step_progress(
                            &row_key,
                            "B2",
                            format!("Lỗi tạm thời ({}) - thử lại sau {wait} giây", fail.code),
                        );
                        if adapters::sleep_interruptible(Duration::from_secs(wait), ctx.stop_flag) {
                            return Err(ApiFail::stopped("Đã dừng an toàn giữa các lần thử (5.4)"));
                        }
                    }
                }
            }
        }
    };

    let prev_img = row.anh_da_tao.trim().to_string();
    let old_keys = if !prev_img.is_empty() {
        collect_file_ref_row_keys_by_name(ctx.conn, &prev_img).map_err(|e| ApiFail {
            kind: ApiFailKind::NghiepVu,
            http_status: None,
            code: "DB_TX_FAIL".into(),
            message: format!("{e}"),
        })?
    } else {
        Vec::new()
    };

    let reservation =
        reserve_next_version_with_keys(ctx.conn, ctx.image_out, &row_key, &old_keys, "anh");
    let ver = reservation.version;
    let file_name = reservation.file_name.clone();
    let dest = reservation.dest.clone();
    write_atomic(&dest, &bytes).map_err(|e| ApiFail {
        kind: ApiFailKind::NghiepVu,
        http_status: None,
        code: e.code,
        message: e.message,
    })?;

    // [P2-1] + [P0-2]: Bọc cập nhật `file_refs` + `jobs` (bao gồm `image_prompt_hash` mới!) trong 1 transaction nguyên tử
    let rel = format!("image_out/{file_name}");
    let ts = chrono::Local::now().to_rfc3339();
    let tx = rusqlite::Transaction::new_unchecked(
        ctx.conn,
        rusqlite::TransactionBehavior::Immediate,
    )
    .map_err(|e| ApiFail {
        kind: ApiFailKind::NghiepVu,
        http_status: None,
        code: "DB_TX_FAIL".into(),
        message: format!("Lỗi mở transaction lưu ảnh: {e}"),
    })?;
    for old_rk in &old_keys {
        if old_rk == &row_key {
            continue;
        }
        crate::state_db::migrate_file_refs_row_key(&tx, old_rk, &row_key, None).map_err(|e| {
            ApiFail {
                kind: ApiFailKind::NghiepVu,
                http_status: None,
                code: "DB_TX_FAIL".into(),
                message: format!("{e}"),
            }
        })?;
    }
    tx.execute(
        "UPDATE file_refs SET is_current = 0 WHERE (LOWER(row_key) = LOWER(?1) OR job_id = ?2) AND kind = 'anh'",
        rusqlite::params![row_key, ctx.job_id],
    )
    .map_err(|e| ApiFail {
        kind: ApiFailKind::NghiepVu,
        http_status: None,
        code: "DB_TX_FAIL".into(),
        message: format!("{e}"),
    })?;
    tx.execute(
        "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, size_bytes, created_at)
         VALUES (?1,?2,'anh',?3,?4,?5,1,?6,?7)",
        rusqlite::params![ctx.job_id, row_key, file_name, rel, ver, bytes.len() as i64, ts],
    )
    .map_err(|e| ApiFail {
        kind: ApiFailKind::NghiepVu,
        http_status: None,
        code: "DB_TX_FAIL".into(),
        message: format!("{e}"),
    })?;
    let new_ref_id = tx.last_insert_rowid();
    tx.execute(
        "UPDATE file_refs SET is_current = 0 WHERE (LOWER(row_key) = LOWER(?1) OR job_id = ?2) AND kind = 'anh' AND id != ?3",
        rusqlite::params![row_key, ctx.job_id, new_ref_id],
    )
    .map_err(|e| ApiFail {
        kind: ApiFailKind::NghiepVu,
        http_status: None,
        code: "DB_TX_FAIL".into(),
        message: format!("{e}"),
    })?;
    tx.execute(
        "UPDATE jobs
         SET status = 'da_tao_image', image_status = 'xong',
             image_prompt_hash = ?1, video_task_id = NULL, video_download_url = NULL, last_error = NULL
         WHERE id = ?2",
        rusqlite::params![cur_img_hash, ctx.job_id],
    )
    .map_err(|e| ApiFail {
        kind: ApiFailKind::NghiepVu,
        http_status: None,
        code: "DB_TX_FAIL".into(),
        message: format!("{e}"),
    })?;
    tx.commit().map_err(|e| ApiFail {
        kind: ApiFailKind::NghiepVu,
        http_status: None,
        code: "DB_TX_FAIL".into(),
        message: format!("{e}"),
    })?;
    drop(reservation);

    let now_str = chrono::Local::now().format("%d/%m/%Y %H:%M").to_string();
    row.anh_da_tao = file_name.clone();
    row.trang_thai = "Đã tạo image".into();
    row.ngay_tao_anh = now_str.clone();
    row.ghi_chu_loi.clear();

    if let Err(e) = prompts_csv::update_row(ctx.csv_path, &row_key, |r| {
        r.anh_da_tao = file_name;
        r.trang_thai = "Đã tạo image".into();
        r.ngay_tao_anh = now_str;
        r.ghi_chu_loi.clear();
    }) {
        crate::settings::log(
            ctx.conn,
            "warn",
            Some(ctx.run_id),
            Some(&row_key),
            Some("ghi_sheet"),
            &e.message,
        );
    }

    Ok(RowStatus::DaTaoImage)
}

fn collect_file_ref_row_keys_by_name(
    tx: &Connection,
    file_name: &str,
) -> Result<Vec<String>, rusqlite::Error> {
    let mut stmt =
        tx.prepare("SELECT DISTINCT row_key FROM file_refs WHERE LOWER(file_name) = LOWER(?1)")?;
    let mut rows = stmt.query(rusqlite::params![file_name])?;
    let mut keys = Vec::new();
    while let Some(r) = rows.next()? {
        keys.push(r.get::<_, String>(0)?);
    }
    Ok(keys)
}

// ━━ B3: tạo video (5.1, 13.3, 13.6) + Idempotency-Key + First-Poll Delay ━━

fn create_video_task_with_retry(
    ctx: &WorkerRowCtx<'_>,
    row_key: &str,
    img_path: &Path,
    prompt_video: &str,
    salt: &str,
) -> Result<String, ApiFail> {
    let max_attempts = ctx.cfg.max_attempts.max(1) as i64;
    let idem_key = format!(
        "vf-r{}-j{}-{}-{}",
        ctx.run_id,
        ctx.job_id,
        salt,
        &sha2hex(prompt_video.trim())[..12]
    );
    let mut local_try: u32 = 0;

    loop {
        if ctx.stop_flag.load(Ordering::SeqCst) {
            return Err(ApiFail::stopped(
                "Đã dừng an toàn trước khi gửi yêu cầu tạo task video",
            ));
        }
        local_try += 1;
        match adapters::create_video_task(ctx.cfg, img_path, prompt_video, Some(&idem_key)) {
            Ok(t) => {
                if t.reencoded_jpeg {
                    let mb = t.original_bytes as f64 / (1024.0 * 1024.0);
                    crate::settings::log_detail(
                        ctx.conn,
                        "warn",
                        Some(ctx.run_id),
                        Some(row_key),
                        Some("B3"),
                        &format!(
                            "Ảnh sản phẩm lớn ({mb:.2} MB > 10 MB) - đã tự nén sang JPEG chất lượng 85 trên bộ nhớ trước khi gửi tạo video (5.6)"
                        ),
                        Some(&format!("original_bytes={}", t.original_bytes)),
                    );
                }
                let _ = ctx.conn.execute(
                    "UPDATE jobs SET video_task_id = ?1, video_download_url = NULL, video_prompt_hash = ?2 WHERE id = ?3",
                    rusqlite::params![t.task_id, sha2hex(prompt_video.trim()), ctx.job_id],
                );
                crate::settings::log(
                    ctx.conn,
                    "info",
                    Some(ctx.run_id),
                    Some(row_key),
                    Some("B3"),
                    &format!("Đã tạo task video: {}", t.task_id),
                );
                return Ok(t.task_id);
            }
            Err(fail) => {
                let transient = is_transient_error(&fail);
                let _ = record_attempt(
                    ctx.conn,
                    ctx.job_id,
                    AttemptRecord {
                        step: "B3",
                        fallback_attempt_no: local_try,
                        result: classify(&fail),
                        http_status: fail.http_status.map(|s| s as i64),
                        error_code: &fail.code,
                        error_message: &fail.message,
                        count_toward_limit: !transient,
                    },
                );
                match fail.kind {
                    ApiFailKind::NghiepVu | ApiFailKind::HetQuota => return Err(fail),
                    ApiFailKind::TamThoi { retry_after_s } => {
                        if local_try >= max_attempts as u32 {
                            return Err(fail);
                        }
                        let wait = compute_retry_wait_s(ctx.cfg, local_try, retry_after_s);
                        ctx.emit_step_progress(
                            row_key,
                            "B3",
                            format!(
                                "Lỗi tạm thời khi tạo task video ({}) - thử lại sau {wait} giây",
                                fail.code
                            ),
                        );
                        if adapters::sleep_interruptible(Duration::from_secs(wait), ctx.stop_flag) {
                            return Err(ApiFail::stopped(
                                "Đã dừng an toàn giữa các lần thử tạo task video",
                            ));
                        }
                    }
                }
            }
        }
    }
}

fn download_and_commit_video(
    ctx: &WorkerRowCtx<'_>,
    row: &mut prompts_csv::Row,
    url: &str,
    task_id: &str,
) -> Result<(), ApiFail> {
    let row_key = crate::prompts_csv::row_key_of(
        &row.anh_tham_chieu,
        &row.promt_tao_image,
        &row.promt_tao_video,
    );
    ctx.emit_step_progress(
        &row_key,
        "download",
        "Đang tải video về video_out/".to_string(),
    );

    let mut old_keys: Vec<String> = Vec::new();
    for fname in [row.video_da_tao.trim(), row.anh_da_tao.trim()] {
        if !fname.is_empty() {
            let keys = collect_file_ref_row_keys_by_name(ctx.conn, fname).map_err(|e| ApiFail {
                kind: ApiFailKind::NghiepVu,
                http_status: None,
                code: "DB_TX_FAIL".into(),
                message: format!("{e}"),
            })?;
            for k in keys {
                if !old_keys.iter().any(|existing| existing.eq_ignore_ascii_case(&k)) {
                    old_keys.push(k);
                }
            }
        }
    }

    let reservation =
        reserve_next_version_with_keys(ctx.conn, ctx.video_out, &row_key, &old_keys, "video");
    let ver = reservation.version;
    let file_name = reservation.file_name.clone();
    let dest = reservation.dest.clone();

    let dl = adapters::download_video(ctx.cfg, url, &dest, task_id, ctx.stop_flag)?;

    // [P2-1] + [P1-1] + [P0-2]: Transaction nguyên tử lưu `file_refs` + `jobs`, cập nhật `video_prompt_hash` mới & xóa `attempts_count = 0`!
    let rel = format!("video_out/{file_name}");
    let ts = chrono::Local::now().to_rfc3339();
    let cur_vid_hash = sha2hex(row.promt_tao_video.trim());

    let tx = rusqlite::Transaction::new_unchecked(
        ctx.conn,
        rusqlite::TransactionBehavior::Immediate,
    )
    .map_err(|e| ApiFail {
        kind: ApiFailKind::NghiepVu,
        http_status: None,
        code: "DB_TX_FAIL".into(),
        message: format!("Lỗi mở transaction lưu video: {e}"),
    })?;
    for old_rk in &old_keys {
        if old_rk == &row_key {
            continue;
        }
        crate::state_db::migrate_file_refs_row_key(&tx, old_rk, &row_key, None).map_err(|e| {
            ApiFail {
                kind: ApiFailKind::NghiepVu,
                http_status: None,
                code: "DB_TX_FAIL".into(),
                message: format!("{e}"),
            }
        })?;
    }
    tx.execute(
        "UPDATE file_refs SET is_current = 0 WHERE (LOWER(row_key) = LOWER(?1) OR job_id = ?2) AND kind = 'video'",
        rusqlite::params![row_key, ctx.job_id],
    )
    .map_err(|e| ApiFail {
        kind: ApiFailKind::NghiepVu,
        http_status: None,
        code: "DB_TX_FAIL".into(),
        message: format!("{e}"),
    })?;
    tx.execute(
        "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, size_bytes, created_at)
         VALUES (?1,?2,'video',?3,?4,?5,1,?6,?7)",
        rusqlite::params![ctx.job_id, row_key, file_name, rel, ver, dl.bytes as i64, ts],
    )
    .map_err(|e| ApiFail {
        kind: ApiFailKind::NghiepVu,
        http_status: None,
        code: "DB_TX_FAIL".into(),
        message: format!("{e}"),
    })?;
    let new_ref_id = tx.last_insert_rowid();
    tx.execute(
        "UPDATE file_refs SET is_current = 0 WHERE (LOWER(row_key) = LOWER(?1) OR job_id = ?2) AND kind = 'video' AND id != ?3",
        rusqlite::params![row_key, ctx.job_id, new_ref_id],
    )
    .map_err(|e| ApiFail {
        kind: ApiFailKind::NghiepVu,
        http_status: None,
        code: "DB_TX_FAIL".into(),
        message: format!("{e}"),
    })?;
    tx.execute(
        "UPDATE jobs
         SET status = 'da_tao_video', image_status = 'xong', video_status = 'xong',
             video_download_url = ?1, video_prompt_hash = ?2, attempts_count = 0, last_error = NULL
         WHERE id = ?3",
        rusqlite::params![dl.final_url, cur_vid_hash, ctx.job_id],
    )
    .map_err(|e| ApiFail {
        kind: ApiFailKind::NghiepVu,
        http_status: None,
        code: "DB_TX_FAIL".into(),
        message: format!("{e}"),
    })?;
    tx.commit().map_err(|e| ApiFail {
        kind: ApiFailKind::NghiepVu,
        http_status: None,
        code: "DB_TX_FAIL".into(),
        message: format!("{e}"),
    })?;
    drop(reservation);

    let now_str = chrono::Local::now().format("%d/%m/%Y %H:%M").to_string();
    row.video_da_tao = file_name.clone();
    row.trang_thai = "Đã tạo video".into();
    row.ngay_tao_video = now_str.clone();
    row.ghi_chu_loi.clear();

    if let Err(e) = prompts_csv::update_row(ctx.csv_path, &row_key, |r| {
        r.video_da_tao = file_name;
        r.trang_thai = "Đã tạo video".into();
        r.ngay_tao_video = now_str;
        r.ghi_chu_loi.clear();
    }) {
        crate::settings::log(
            ctx.conn,
            "warn",
            Some(ctx.run_id),
            Some(&row_key),
            Some("ghi_sheet"),
            &e.message,
        );
    }

    Ok(())
}

fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

pub fn find_previous_scene_video_impl(
    csv_path: &Path,
    video_out: &Path,
    conn: &rusqlite::Connection,
    run_id: i64,
    job_id: i64,
    row: &prompts_csv::Row,
) -> Option<PathBuf> {
    // 1. Tìm dòng dữ liệu liền trước trong prompts.csv
    if let Ok(rows) = prompts_csv::read_all(csv_path) {
        let cur_key = prompts_csv::row_key_of(&row.anh_tham_chieu, &row.promt_tao_image, &row.promt_tao_video);
        let pos = rows
            .iter()
            .position(|r| r.row_no == row.row_no)
            .or_else(|| prompts_csv::find_row_position(&rows, &cur_key));

        if let Some(p) = pos {
            for prev_r in rows[..p].iter().rev() {
                if prev_r.is_empty_content() || prev_r.is_example() {
                    continue;
                }
                // Kiểm tra nếu video_da_tao đã có tên file và file tồn tại trên đĩa
                let candidate = prev_r.video_da_tao.trim();
                if !candidate.is_empty() {
                    let vid_path = video_out.join(candidate);
                    if vid_path.is_file() {
                        return Some(vid_path);
                    }
                }
                // Tra cứu file_refs trong SQLite cho dòng liền trước này
                let prev_key = prompts_csv::row_key_of(
                    &prev_r.anh_tham_chieu,
                    &prev_r.promt_tao_image,
                    &prev_r.promt_tao_video,
                );
                if let Ok(file_name) = conn.query_row(
                    "SELECT file_name FROM file_refs WHERE LOWER(row_key) = LOWER(?1) AND kind = 'video' ORDER BY is_current DESC, version DESC, id DESC LIMIT 1",
                    rusqlite::params![prev_key],
                    |r| r.get::<_, String>(0),
                ) {
                    let vid_path = video_out.join(file_name.trim());
                    if vid_path.is_file() {
                        return Some(vid_path);
                    }
                }
                // Chỉ duyệt đúng dòng cảnh liền trước
                break;
            }
        }
    }

    // Không fallback sang `WHERE run_id = ?1 AND id < ?2` vì có thể lấy nhầm video của dòng
    // không liên quan khi chạy lẻ hoặc dòng trước bị bỏ qua.
    let _ = (run_id, job_id);
    None
}

fn find_previous_scene_video(ctx: &WorkerRowCtx<'_>, row: &prompts_csv::Row) -> Option<PathBuf> {
    find_previous_scene_video_impl(ctx.csv_path, ctx.video_out, ctx.conn, ctx.run_id, ctx.job_id, row)
}

pub fn extract_last_frame_ffmpeg(video_path: &Path, output_frame_path: &Path) -> Result<(), String> {
    extract_last_frame_ffmpeg_with_options(video_path, output_frame_path, "", None)
}

pub fn extract_last_frame_ffmpeg_with_options(
    video_path: &Path,
    output_frame_path: &Path,
    ffmpeg_override: &str,
    stop_flag: Option<&AtomicBool>,
) -> Result<(), String> {
    if !video_path.is_file() {
        return Err(format!("File video nguồn không tồn tại: {}", video_path.display()));
    }
    if let Some(parent) = output_frame_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let bin = crate::ffmpeg::resolve_ffmpeg_binary(ffmpeg_override)?;
    let args = vec![
        "-y".to_string(),
        "-sseof".to_string(),
        "-0.5".to_string(),
        "-i".to_string(),
        video_path.to_string_lossy().to_string(),
        "-frames:v".to_string(),
        "1".to_string(),
        output_frame_path.to_string_lossy().to_string(),
    ];

    let output = crate::ffmpeg::run_command_with_timeout(
        &bin,
        &args,
        crate::ffmpeg::FFMPEG_FRAME_TIMEOUT,
        stop_flag,
    )?;
    if !output.status.success() {
        let err_msg = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Lệnh FFmpeg thất bại (code {:?}): {}", output.status.code(), err_msg.trim()));
    }
    if !output_frame_path.is_file() {
        return Err("FFmpeg không tạo được file ảnh đầu ra".into());
    }
    let len = std::fs::metadata(output_frame_path)
        .map(|m| m.len())
        .unwrap_or(0);
    if len == 0 {
        return Err("File frame trích xuất có kích thước 0 byte".into());
    }
    Ok(())
}

fn run_b3(
    ctx: &WorkerRowCtx<'_>,
    row: &mut prompts_csv::Row,
    force_new: bool,
) -> Result<RowStatus, ApiFail> {
    let row_key = crate::prompts_csv::row_key_of(
        &row.anh_tham_chieu,
        &row.promt_tao_image,
        &row.promt_tao_video,
    );

    if !force_new
        && matches!(row.status(), RowStatus::DaTaoVideo)
        && !row.video_da_tao.is_empty()
        && ctx.video_out.join(&row.video_da_tao).is_file()
    {
        let _ = ctx.conn.execute(
            "UPDATE jobs SET status = 'da_tao_video', image_status = 'xong', video_status = 'xong',
                             video_prompt_hash = ?1, attempts_count = 0, last_error = NULL
             WHERE id = ?2",
            rusqlite::params![sha2hex(row.promt_tao_video.trim()), ctx.job_id],
        );
        return Ok(RowStatus::DaTaoVideo);
    }

    // Xác định ảnh đầu vào cho B3: nếu có `anh_da_tao` thì bắt buộc file trong `image_out/`
    // phải tồn tại (không âm thầm rơi về ảnh thô `input/` khi ảnh đã bị xóa). Chỉ dùng ảnh `input/`
    // khi dòng không có prompt ảnh (`promt_tao_image` rỗng).
    let orig_img_path = if !row.anh_da_tao.is_empty() {
        let p = ctx.image_out.join(&row.anh_da_tao);
        if p.is_file() {
            Some(p)
        } else {
            return Err(ApiFail {
                kind: ApiFailKind::NghiepVu,
                http_status: None,
                code: "NO_IMAGE".into(),
                message: format!(
                    "Không tìm thấy ảnh sản phẩm '{}' trong image_out/ để tạo video.",
                    row.anh_da_tao
                ),
            });
        }
    } else if row.promt_tao_image.trim().is_empty() {
        let in_ref = ctx
            .csv_path
            .parent()
            .unwrap_or(Path::new("."))
            .join("input")
            .join(&row.anh_tham_chieu);
        if in_ref.is_file() {
            Some(in_ref)
        } else {
            None
        }
    } else {
        None
    };

    let mut chosen_img_path = orig_img_path.clone();

    // [P2-1]: Nếu noi_canh bật, tìm video cảnh trước để trích xuất frame cuối
    if row.noi_canh {
        ctx.emit_step_progress(
            &row_key,
            "B3",
            "Nối cảnh (noi_canh=1): đang tìm video cảnh trước để trích xuất frame cuối...".to_string(),
        );

        let prev_video_opt = find_previous_scene_video(ctx, row);
        match prev_video_opt {
            Some(prev_video_path) => {
                let project_dir = ctx.csv_path.parent().unwrap_or(Path::new("."));
                let cache_dir = project_dir.join(".cache");
                let safe_key = sanitize_filename(&row_key);
                let extracted_frame_path = cache_dir.join(format!("last_frame_{}.png", safe_key));

                match extract_last_frame_ffmpeg_with_options(
                    &prev_video_path,
                    &extracted_frame_path,
                    &ctx.cfg.ffmpeg_path,
                    Some(ctx.stop_flag),
                ) {
                    Ok(()) => {
                        crate::settings::log(
                            ctx.conn,
                            "info",
                            Some(ctx.run_id),
                            Some(&row_key),
                            Some("B3"),
                            &format!(
                                "Đã trích xuất frame cuối từ video trước '{}' làm ảnh tham chiếu nối cảnh",
                                prev_video_path.display()
                            ),
                        );
                        chosen_img_path = Some(extracted_frame_path);
                    }
                    Err(err) if err.starts_with("STOPPED:") => {
                        return Err(ApiFail::stopped(
                            "Đã dừng an toàn trong lúc trích xuất frame nối cảnh FFmpeg",
                        ));
                    }
                    Err(err) => {
                        crate::settings::log(
                            ctx.conn,
                            "warn",
                            Some(ctx.run_id),
                            Some(&row_key),
                            Some("B3"),
                            &format!(
                                "Lỗi trích xuất frame cuối video trước ({err}) - tự động fallback về ảnh tham chiếu gốc"
                            ),
                        );
                        // chosen_img_path giữ nguyên orig_img_path (Fail-safe)
                    }
                }
            }
            None => {
                crate::settings::log(
                    ctx.conn,
                    "warn",
                    Some(ctx.run_id),
                    Some(&row_key),
                    Some("B3"),
                    "Không tìm thấy video thành phẩm của cảnh liền trước - tự động fallback về ảnh tham chiếu gốc",
                );
                // chosen_img_path giữ nguyên orig_img_path (Fail-safe)
            }
        }
    }

    let img_path = match chosen_img_path {
        Some(p) if p.is_file() => p,
        _ => {
            return Err(ApiFail {
                kind: ApiFailKind::NghiepVu,
                http_status: None,
                code: "NO_IMAGE".into(),
                message: if row.anh_da_tao.is_empty() {
                    "Chưa có ảnh sản phẩm - không thể tạo video (6.12)".into()
                } else {
                    format!("Ảnh sản phẩm '{}' không còn trong image_out/", row.anh_da_tao)
                },
            });
        }
    };

    let has_resumable_plugin_op = if force_new {
        false
    } else {
        crate::state_db::load_plugin_operation(ctx.conn, &row_key, "B3")
            .map_err(|e| ApiFail {
                kind: ApiFailKind::NghiepVu,
                http_status: None,
                code: "DB_TX_FAIL".into(),
                message: format!("Không đọc được operation video: {e}"),
            })?
            .is_some_and(|op| {
                std::fs::read(&img_path).ok().is_some_and(|bytes| {
                    let hash = sha2hex(&base64::engine::general_purpose::STANDARD.encode(bytes));
                    serde_json::from_str::<serde_json::Value>(&op.request_json)
                        .ok()
                        .is_some_and(|v| {
                            op.media_kind == "video"
                                && v["image_sha256"] == hash
                                && v["prompt"] == row.promt_tao_video
                        })
                })
            })
    };
    if crate::plugins::routing::is_plugin(&ctx.cfg.video_source) || has_resumable_plugin_op {
        return run_plugin_b3(ctx, row, &row_key, &img_path, force_new);
    }

    let _ = ctx.conn.execute(
        "UPDATE jobs SET video_status = 'dang_chay' WHERE id = ?1",
        rusqlite::params![ctx.job_id],
    );

    let (saved_task_id, saved_dl_url): (Option<String>, Option<String>) = if force_new {
        (None, None)
    } else {
        ctx.conn
            .query_row(
                "SELECT video_task_id, video_download_url FROM jobs WHERE id = ?1",
                rusqlite::params![ctx.job_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap_or((None, None))
    };

    // Nhánh 1: Đã có sẵn URL tải video từ lần trước -> tải trực tiếp, KHÔNG tạo task mới!
    if let Some(existing_url) = saved_dl_url.filter(|s| !s.trim().is_empty()) {
        let tid_ref = saved_task_id.as_deref().unwrap_or_default();
        match download_and_commit_video(ctx, row, &existing_url, tid_ref) {
            Ok(()) => return Ok(RowStatus::DaTaoVideo),
            Err(fail) if fail.code == "DOWNLOAD_TASK_GONE" => {
                let _ = ctx.conn.execute(
                    "UPDATE jobs SET video_task_id = NULL, video_download_url = NULL WHERE id = ?1",
                    rusqlite::params![ctx.job_id],
                );
            }
            Err(fail) => {
                if fail.code != "STOPPED" {
                    let _ = record_attempt(
                        ctx.conn,
                        ctx.job_id,
                        AttemptRecord {
                            step: "download",
                            fallback_attempt_no: 3,
                            result: classify(&fail),
                            http_status: fail.http_status.map(|s| s as i64),
                            error_code: &fail.code,
                            error_message: &fail.message,
                            count_toward_limit: false,
                        },
                    );
                }
                return Err(fail);
            }
        }
    }

    // Nhánh 2: Resume `saved_task_id` (poll ngay) hoặc tạo task mới (chờ `poll_interval_s` trước nhịp poll đầu!)
    let (mut task_id, mut is_fresh_task) = match saved_task_id.filter(|s| !s.trim().is_empty()) {
        Some(existing_tid) => {
            let still_saved: Option<String> = ctx
                .conn
                .query_row(
                    "SELECT video_task_id FROM jobs WHERE id = ?1",
                    rusqlite::params![ctx.job_id],
                    |r| r.get(0),
                )
                .ok()
                .flatten();
            if still_saved.is_some() {
                crate::settings::log(
                    ctx.conn,
                    "info",
                    Some(ctx.run_id),
                    Some(&row_key),
                    Some("B3"),
                    &format!("Tiếp tục poll task video cũ: {existing_tid} (không tạo task mới)"),
                );
                (existing_tid, false)
            } else {
                (
                    create_video_task_with_retry(ctx, &row_key, &img_path, &row.promt_tao_video, "init")?,
                    true,
                )
            }
        }
        None => (
            create_video_task_with_retry(ctx, &row_key, &img_path, &row.promt_tao_video, "init")?,
            true,
        ),
    };

    let interval = Duration::from_secs(ctx.cfg.poll_interval_s.max(1));
    let max_attempts = ctx.cfg.max_attempts.max(1) as i64;
    let mut recreated_once = false;
    let mut max_progress: u32 = 0;
    let mut poll_started = Instant::now();
    let mut poll_no: u32 = 0;
    let mut local_transient_try: u32 = 0;

    let resolved_url = loop {
        // [P1-3]: Với task vừa mới tạo xong, chờ đủ `poll_interval_s` trước nhịp poll đầu tiên
        // để tránh hiểu nhầm 404 do độ trễ đồng bộ phía server thành `TaskGone`.
        if is_fresh_task {
            is_fresh_task = false;
            ctx.emit_step_progress(
                &row_key,
                "B3",
                format!("Đã tạo task {task_id} - chờ nhịp poll đầu tiên..."),
            );
            if adapters::sleep_interruptible(interval, ctx.stop_flag) {
                return Err(ApiFail::stopped(format!(
                    "Dừng an toàn - task {task_id} vẫn được lưu để poll tiếp lần sau"
                )));
            }
        } else if ctx.stop_flag.load(Ordering::SeqCst) {
            return Err(ApiFail::stopped(format!(
                "Dừng an toàn - task {task_id} vẫn được lưu trong state.db để poll tiếp lần sau"
            )));
        }

        poll_no += 1;
        match adapters::poll_once(ctx.cfg, &task_id) {
            Ok(PollOutcome::Xong {
                video_url,
                media_name,
            }) => {
                let url = adapters::resolve_download_url(ctx.cfg, video_url, media_name, &task_id)?;
                let _ = ctx.conn.execute(
                    "UPDATE jobs SET video_download_url = ?1 WHERE id = ?2",
                    rusqlite::params![url, ctx.job_id],
                );
                break url;
            }
            Ok(PollOutcome::StillRunning { progress }) => {
                if should_reset_poll_timer(&mut max_progress, progress) {
                    poll_started = Instant::now();
                }
                let waited = poll_started.elapsed().as_secs();
                ctx.emit_step_progress(
                    &row_key,
                    "B3",
                    match progress {
                        Some(p) => format!("Đang tạo video - tiến độ {p}%"),
                        None => format!("Đang tạo video - poll lần {poll_no}, đã chờ {waited}s"),
                    },
                );

                if waited >= ctx.cfg.poll_max_s {
                    let msg = format!(
                        "Video chưa xong sau {} giây poll - task {task_id} vẫn sống phía server, lần sau sẽ poll tiếp task cũ (không tạo task mới)",
                        ctx.cfg.poll_max_s
                    );
                    let _ = record_attempt(
                        ctx.conn,
                        ctx.job_id,
                        AttemptRecord {
                            step: "B3",
                            fallback_attempt_no: 1,
                            result: "loi_tam_thoi",
                            http_status: None,
                            error_code: "POLL_TIMEOUT",
                            error_message: &msg,
                            count_toward_limit: false,
                        },
                    );
                    crate::settings::log(
                        ctx.conn,
                        "warn",
                        Some(ctx.run_id),
                        Some(&row_key),
                        Some("B3"),
                        &msg,
                    );
                    return Err(ApiFail {
                        kind: ApiFailKind::TamThoi { retry_after_s: None },
                        http_status: None,
                        code: "POLL_TIMEOUT".into(),
                        message: msg,
                    });
                }

                if adapters::sleep_interruptible(interval, ctx.stop_flag) {
                    return Err(ApiFail::stopped(format!(
                        "Dừng an toàn - task {task_id} vẫn được lưu để poll tiếp lần sau"
                    )));
                }
            }
            Ok(PollOutcome::Loi {
                message,
                kind,
                code,
            }) => {
                // Task đã kết thúc với trạng thái failed trên server -> xóa video_task_id để lần retry (nếu tạm thời) tạo task mới
                let _ = ctx.conn.execute(
                    "UPDATE jobs SET video_task_id = NULL, video_download_url = NULL WHERE id = ?1",
                    rusqlite::params![ctx.job_id],
                );
                let fail = ApiFail {
                    kind: kind.clone(),
                    http_status: None,
                    code: code.clone(),
                    message: format!("Endpoint báo task lỗi: {message}"),
                };
                let transient = is_transient_error(&fail);
                let _ = record_attempt(
                    ctx.conn,
                    ctx.job_id,
                    AttemptRecord {
                        step: "B3",
                        fallback_attempt_no: 1,
                        result: classify(&fail),
                        http_status: None,
                        error_code: &code,
                        error_message: &message,
                        count_toward_limit: !transient,
                    },
                );
                match kind {
                    ApiFailKind::NghiepVu | ApiFailKind::HetQuota => return Err(fail),
                    ApiFailKind::TamThoi { retry_after_s } => {
                        local_transient_try += 1;
                        if local_transient_try >= max_attempts as u32 {
                            return Err(fail);
                        }
                        let wait =
                            compute_retry_wait_s(ctx.cfg, local_transient_try, retry_after_s);
                        ctx.emit_step_progress(
                            &row_key,
                            "B3",
                            format!("Task video gặp lỗi máy chủ tạm thời - tạo lại sau {wait}s"),
                        );
                        if adapters::sleep_interruptible(Duration::from_secs(wait), ctx.stop_flag) {
                            return Err(ApiFail::stopped(
                                "Đã dừng an toàn trước khi tạo lại task video",
                            ));
                        }
                        task_id = create_video_task_with_retry(
                            ctx,
                            &row_key,
                            &img_path,
                            &row.promt_tao_video,
                            &format!("retry{local_transient_try}"),
                        )?;
                        is_fresh_task = true;
                        max_progress = 0;
                        poll_started = Instant::now();
                        poll_no = 0;
                    }
                }
            }
            Ok(PollOutcome::TaskGone) => {
                if !recreated_once {
                    recreated_once = true;
                    crate::settings::log(
                        ctx.conn,
                        "info",
                        Some(ctx.run_id),
                        Some(&row_key),
                        Some("B3"),
                        &format!("Task {task_id} trả 404 - tạo lại task mới 1 lần và tiếp tục poll (13.6)"),
                    );
                    let _ = ctx.conn.execute(
                        "UPDATE jobs SET video_task_id = NULL, video_download_url = NULL WHERE id = ?1",
                        rusqlite::params![ctx.job_id],
                    );
                    task_id = create_video_task_with_retry(
                        ctx,
                        &row_key,
                        &img_path,
                        &row.promt_tao_video,
                        "recreate404",
                    )?;
                    is_fresh_task = true;
                    max_progress = 0;
                    poll_started = Instant::now();
                    poll_no = 0;
                    continue;
                } else {
                    let _ = ctx.conn.execute(
                        "UPDATE jobs SET video_task_id = NULL, video_download_url = NULL WHERE id = ?1",
                        rusqlite::params![ctx.job_id],
                    );
                    let msg = "Task video vẫn trả 404 ngay cả sau khi đã tạo lại 1 lần";
                    let _ = record_attempt(
                        ctx.conn,
                        ctx.job_id,
                        AttemptRecord {
                            step: "B3",
                            fallback_attempt_no: 1,
                            result: "loi_tam_thoi",
                            http_status: Some(404),
                            error_code: "TASK_GONE",
                            error_message: msg,
                            count_toward_limit: false,
                        },
                    );
                    return Err(ApiFail {
                        kind: ApiFailKind::TamThoi { retry_after_s: None },
                        http_status: Some(404),
                        code: "TASK_GONE".into(),
                        message: msg.into(),
                    });
                }
            }
            Err(fail) => {
                local_transient_try += 1;
                let transient = is_transient_error(&fail);
                let _ = record_attempt(
                    ctx.conn,
                    ctx.job_id,
                    AttemptRecord {
                        step: "B3",
                        fallback_attempt_no: local_transient_try,
                        result: classify(&fail),
                        http_status: fail.http_status.map(|s| s as i64),
                        error_code: &fail.code,
                        error_message: &fail.message,
                        count_toward_limit: !transient,
                    },
                );
                match fail.kind {
                    ApiFailKind::NghiepVu | ApiFailKind::HetQuota => return Err(fail),
                    ApiFailKind::TamThoi { retry_after_s } => {
                        if local_transient_try >= max_attempts as u32 {
                            return Err(fail);
                        }
                        let wait =
                            compute_retry_wait_s(ctx.cfg, local_transient_try, retry_after_s);
                        ctx.emit_step_progress(
                            &row_key,
                            "B3",
                            format!(
                                "Lỗi mạng/tạm thời khi poll ({}) - thử lại sau {wait}s",
                                fail.code
                            ),
                        );
                        if adapters::sleep_interruptible(Duration::from_secs(wait), ctx.stop_flag) {
                            return Err(ApiFail::stopped(
                                "Đã dừng an toàn giữa các lần retry poll video",
                            ));
                        }
                    }
                }
            }
        }
    };

    download_and_commit_video(ctx, row, &resolved_url, &task_id).inspect_err(|fail| {
        if fail.code != "STOPPED" {
            let _ = record_attempt(
                ctx.conn,
                ctx.job_id,
                AttemptRecord {
                    step: "download",
                    fallback_attempt_no: 3,
                    result: classify(fail),
                    http_status: fail.http_status.map(|s| s as i64),
                    error_code: &fail.code,
                    error_message: &fail.message,
                    count_toward_limit: false,
                },
            );
        }
    })?;

    Ok(RowStatus::DaTaoVideo)
}

fn run_plugin_b3(
    ctx: &WorkerRowCtx<'_>,
    row: &mut prompts_csv::Row,
    row_key: &str,
    image: &Path,
    force_new: bool,
) -> Result<RowStatus, ApiFail> {
    let bytes = std::fs::read(image).map_err(|e| ApiFail {
        kind: ApiFailKind::NghiepVu,
        http_status: None,
        code: "IMAGE_READ_FAIL".into(),
        message: e.to_string(),
    })?;
    let image_hash = sha2hex(&base64::engine::general_purpose::STANDARD.encode(&bytes));
    let prior = if force_new {
        None
    } else {
        crate::state_db::load_plugin_operation(ctx.conn, row_key, "B3").map_err(|e| ApiFail {
            kind: ApiFailKind::NghiepVu,
            http_status: None,
            code: "DB_TX_FAIL".into(),
            message: format!("Không đọc được operation video: {e}"),
        })?
    };
    let reusable = prior.filter(|op| {
        op.media_kind == "video"
            && serde_json::from_str::<serde_json::Value>(&op.request_json)
                .ok()
                .is_some_and(|v| {
                    v["image_sha256"] == image_hash && v["prompt"] == row.promt_tao_video
                })
    });
    let lease = if let Some(op) = reusable.as_ref() {
        crate::plugins::routing::hold_owner(&op.owner, "video", ctx.stop_flag)
            .map_err(|e| e.as_api_fail())?
    } else {
        crate::plugins::routing::acquire(&ctx.cfg.video_source, "video", ctx.stop_flag)
            .map_err(|e| e.as_api_fail())?
    };
    let prior_snapshot = reusable
        .as_ref()
        .and_then(|o| serde_json::from_str::<serde_json::Value>(&o.request_json).ok());
    let request = serde_json::json!({
        "model":prior_snapshot.as_ref().and_then(|v|v["model"].as_str()).unwrap_or(&lease.model),
        "prompt":row.promt_tao_video,"image_sha256":image_hash,
        "size":prior_snapshot.as_ref().and_then(|v|v["size"].as_str()).unwrap_or(&ctx.cfg.video_size),
        "seconds":prior_snapshot.as_ref().and_then(|v|v["seconds"].as_u64()).unwrap_or(ctx.cfg.video_seconds as u64)
    });
    let fingerprint = sha2hex(&request.to_string());
    let op = if let Some(op) = reusable {
        op
    } else {
        crate::state_db::prepare_plugin_operation(
            ctx.conn,
            ctx.job_id,
            row_key,
            "B3",
            &lease.owner,
            "video",
            &request.to_string(),
            &fingerprint,
            force_new,
        )
        .map_err(|e| ApiFail {
            kind: ApiFailKind::NghiepVu,
            http_status: None,
            code: "DB_TX_FAIL".into(),
            message: format!("Không lưu được operation trước khi gửi yêu cầu: {e}"),
        })?
    };
    let saved_request =
        serde_json::from_str::<serde_json::Value>(&op.request_json).unwrap_or_default();
    let mut submit_cfg = ctx.cfg.clone();
    if let Some(size) = saved_request["size"].as_str() {
        submit_cfg.video_size = size.to_owned();
    }
    if let Some(seconds) = saved_request["seconds"].as_u64() {
        submit_cfg.video_seconds = seconds as u32;
    }
    let _ = ctx.conn.execute(
        "UPDATE plugin_operations SET job_id=?1,updated_at=?2 WHERE operation_id=?3",
        rusqlite::params![
            ctx.job_id,
            chrono::Local::now().to_rfc3339(),
            op.operation_id
        ],
    );
    if ctx.stop_flag.load(Ordering::SeqCst) {
        stop_plugin_operation(ctx, &op)?;
        return Err(ApiFail::stopped("Đang chờ plugin xác nhận dừng video"));
    }
    let mut value = if plugin_operation_can_retry(&op) {
        crate::plugins::client::submit_video(
            &submit_cfg,
            &op.owner,
            &op.operation_id,
            request["model"].as_str().unwrap_or(&lease.model),
            image,
            &row.promt_tao_video,
        )
        .map_err(|e| e.as_api_fail())?
    } else if op.status != "prepared" {
        match crate::plugins::client::lookup(&op.owner, &op.operation_id) {
            Ok(v) => v,
            Err(e) if e.http == Some(404) => {
                return Err(persist_plugin_operation_error(
                    ctx,
                    &op,
                    &e,
                    "reconcile_needed",
                ))
            }
            Err(e) => return Err(e.as_api_fail()),
        }
    } else {
        crate::state_db::update_plugin_operation(
            ctx.conn,
            &op.operation_id,
            "submitting",
            None,
            None,
            None,
        )
        .map_err(|e| ApiFail {
            kind: ApiFailKind::NghiepVu,
            http_status: None,
            code: "DB_TX_FAIL".into(),
            message: e.to_string(),
        })?;
        match crate::plugins::client::submit_video(
            &submit_cfg,
            &op.owner,
            &op.operation_id,
            request["model"].as_str().unwrap_or(&lease.model),
            image,
            &row.promt_tao_video,
        ) {
            Ok(v) => v,
            Err(e) => match crate::plugins::client::lookup(&op.owner, &op.operation_id) {
                Ok(v) => v,
                Err(_) => {
                    return Err(persist_plugin_operation_error(
                        ctx,
                        &op,
                        &e,
                        if e.http.is_some() {
                            "failed"
                        } else {
                            "reconcile_needed"
                        },
                    ))
                }
            },
        }
    };
    let mut task_id = op.task_id.clone();
    let start = Instant::now();
    loop {
        if let Some(t) = value.get("task_id").and_then(|v| v.as_str()) {
            task_id = Some(t.to_owned());
            let _ = ctx.conn.execute(
                "UPDATE jobs SET video_task_id=?1 WHERE id=?2",
                rusqlite::params![t, ctx.job_id],
            );
        }
        match value["status"].as_str().unwrap_or("") {
            "succeeded" => {
                let name = value["media_name"].as_str().ok_or_else(|| ApiFail {
                    kind: ApiFailKind::NghiepVu,
                    http_status: None,
                    code: "PLUGIN_BAD_RESPONSE".into(),
                    message: "Plugin thiếu media_name".into(),
                })?;
                crate::state_db::update_plugin_operation(
                    ctx.conn,
                    &op.operation_id,
                    "succeeded",
                    task_id.as_deref(),
                    Some(name),
                    None,
                )
                .map_err(|e| ApiFail {
                    kind: ApiFailKind::NghiepVu,
                    http_status: None,
                    code: "DB_TX_FAIL".into(),
                    message: e.to_string(),
                })?;
                let url = crate::plugins::client::media_url(&op.owner, name)
                    .map_err(|e| e.as_api_fail())?;
                let mut cfg = ctx.cfg.clone();
                cfg.video_source = op.owner.clone();
                let owner_ctx = WorkerRowCtx {
                    emitter: ctx.emitter,
                    cfg: &cfg,
                    conn: ctx.conn,
                    csv_path: ctx.csv_path,
                    image_out: ctx.image_out,
                    video_out: ctx.video_out,
                    stop_flag: ctx.stop_flag,
                    run_id: ctx.run_id,
                    job_id: ctx.job_id,
                    total: ctx.total,
                    done_count: ctx.done_count,
                    active_rows: ctx.active_rows,
                };
                download_and_commit_video(
                    &owner_ctx,
                    row,
                    &url,
                    task_id.as_deref().unwrap_or(&op.operation_id),
                )?;
                return Ok(RowStatus::DaTaoVideo);
            }
            "failed" | "reconcile_needed" | "cancelled" | "abandoned" => {
                let error = value.get("error").cloned().unwrap_or_default();
                let st = value["status"].as_str().unwrap_or("failed");
                let _ = crate::state_db::update_plugin_operation(
                    ctx.conn,
                    &op.operation_id,
                    st,
                    task_id.as_deref(),
                    None,
                    Some(&error.to_string()),
                );
                let failure = crate::plugins::client::PluginError::from_error(&error, None);
                crate::plugins::routing::block(&op.owner, &op.media_kind, &failure);
                return Err(failure.as_api_fail());
            }
            "processing" => {
                let _ = crate::state_db::update_plugin_operation(
                    ctx.conn,
                    &op.operation_id,
                    "processing",
                    task_id.as_deref(),
                    None,
                    None,
                );
                if ctx.stop_flag.load(Ordering::SeqCst) {
                    crate::state_db::request_plugin_operation_stop(ctx.conn, &op.operation_id)
                        .map_err(|e| ApiFail {
                            kind: ApiFailKind::NghiepVu,
                            http_status: None,
                            code: "DB_TX_FAIL".into(),
                            message: e.to_string(),
                        })?;
                    match crate::plugins::client::cancel(&op.owner, &op.operation_id) {
                        Ok(v) => {
                            let st = v["status"].as_str().unwrap_or("reconcile_needed");
                            let _ = crate::state_db::confirm_plugin_operation_stop(
                                ctx.conn,
                                &op.operation_id,
                                st,
                            );
                        }
                        Err(e) => crate::plugins::routing::block(&op.owner, "video", &e),
                    }
                    return Err(ApiFail::stopped("Đang chờ plugin xác nhận dừng video"));
                }
                if start.elapsed().as_secs() >= lease.poll_max_s {
                    return Err(ApiFail {
                        kind: ApiFailKind::TamThoi {
                            retry_after_s: None,
                        },
                        http_status: None,
                        code: "POLL_TIMEOUT".into(),
                        message:
                            "Hết thời gian poll; operation plugin được giữ để tiếp tục đối soát"
                                .into(),
                    });
                }
                if adapters::sleep_interruptible(
                    Duration::from_secs(lease.poll_interval_s.max(1)),
                    ctx.stop_flag,
                ) {
                    continue;
                }
                let read = match task_id.as_deref() {
                    Some(id) => crate::plugins::client::poll(&op.owner, id),
                    None => crate::plugins::client::lookup(&op.owner, &op.operation_id),
                };
                value = match read {
                    Ok(v) => v,
                    Err(e) => {
                        return Err(persist_plugin_operation_error(
                            ctx,
                            &op,
                            &e,
                            "reconcile_needed",
                        ));
                    }
                };
                // /videos/{task_id} may omit operation_id; the polling response is authoritative.
                if value.get("status").and_then(|v| v.as_str()) == Some("processing") {
                    continue;
                }
            }
            _ => {
                return Err(ApiFail {
                    kind: ApiFailKind::NghiepVu,
                    http_status: None,
                    code: "PLUGIN_BAD_RESPONSE".into(),
                    message: "Plugin trả trạng thái operation không hợp lệ".into(),
                })
            }
        }
    }
}

/// Recover a plugin clip that was abandoned or lost during polling. The saved
/// operation owner and recovery ID are authoritative across Settings changes.
pub fn recover_row_media(
    cfg: &Settings,
    project_path: &Path,
    row_key: &str,
) -> AppResult<serde_json::Value> {
    let csv_path = project_path.join("prompts.csv");
    let rows = prompts_csv::read_all(&csv_path)?;
    let mut row = prompts_csv::find_row_by_key(&rows, row_key)
        .cloned()
        .ok_or_else(|| {
            AppError::new("ROW_NOT_FOUND", format!("Không tìm thấy dòng '{row_key}'."))
        })?;
    let conn =
        crate::state_db::open(project_path).map_err(|e| AppError::new("DB_FAIL", e.to_string()))?;
    let op = crate::state_db::load_plugin_operation(&conn, row_key, "B3")
        .map_err(|e| AppError::new("DB_FAIL", e.to_string()))?
        .ok_or_else(|| {
            AppError::new(
                "NO_RECOVERABLE_MEDIA",
                "Không có operation plugin để khôi phục.",
            )
        })?;
    let recovery_id = op
        .error_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|v| v["recoverable_media_id"].as_str().map(str::to_owned))
        .ok_or_else(|| {
            AppError::new(
                "NO_RECOVERABLE_MEDIA",
                "Operation không có recoverable_media_id.",
            )
        })?;
    let lease = crate::plugins::routing::hold_owner(&op.owner, "video", &AtomicBool::new(false))
        .map_err(|e| AppError::new(&e.code, e.message))?;
    let action = crate::plugins::client::action(
        &op.owner,
        "recover_media",
        &serde_json::json!({"recoverable_media_id":recovery_id}),
    )
    .map_err(|e| AppError::new(&e.code, e.message))?;
    let mut task_id = action["task_id"]
        .as_str()
        .map(str::to_owned)
        .or(op.task_id.clone());
    if let Some(id) = task_id.as_deref() {
        crate::state_db::update_plugin_operation(
            &conn,
            &op.operation_id,
            "processing",
            Some(id),
            None,
            None,
        )
        .map_err(|e| AppError::new("DB_FAIL", e.to_string()))?;
    }
    let mut value = action;
    let started = Instant::now();
    loop {
        if value["status"] == "succeeded" {
            if let Some(name) = value["media_name"].as_str() {
                let url = crate::plugins::client::media_url(&op.owner, name)
                    .map_err(|e| AppError::new(&e.code, e.message))?;
                let job_id=op.job_id.or_else(||conn.query_row("SELECT id FROM jobs WHERE LOWER(row_key)=LOWER(?1) ORDER BY id DESC LIMIT 1",rusqlite::params![row_key],|r|r.get(0)).ok())
                    .ok_or_else(||AppError::new("JOB_NOT_FOUND","Không tìm thấy job để lưu video."))?;
                let no_op = |_: &str, _: serde_json::Value| {};
                let done = AtomicUsize::new(0);
                let active = parking_lot::Mutex::new(Vec::new());
                let stop = AtomicBool::new(false);
                let mut local_cfg = cfg.clone();
                local_cfg.video_source = op.owner.clone();
                let image_out = project_path.join("image_out");
                let video_out = project_path.join("video_out");
                let ctx = WorkerRowCtx {
                    emitter: &no_op,
                    cfg: &local_cfg,
                    conn: &conn,
                    csv_path: &csv_path,
                    image_out: &image_out,
                    video_out: &video_out,
                    stop_flag: &stop,
                    run_id: 0,
                    job_id,
                    total: 1,
                    done_count: &done,
                    active_rows: &active,
                };
                download_and_commit_video(
                    &ctx,
                    &mut row,
                    &url,
                    task_id.as_deref().unwrap_or(&op.operation_id),
                )
                .map_err(|e| AppError::new(&e.code, e.message))?;
                crate::state_db::update_plugin_operation(
                    &conn,
                    &op.operation_id,
                    "succeeded",
                    task_id.as_deref(),
                    Some(name),
                    None,
                )
                .map_err(|e| AppError::new("DB_FAIL", e.to_string()))?;
                return Ok(
                    serde_json::json!({"status":"succeeded","operation_id":op.operation_id,"owner":op.owner,"task_id":task_id,"media_name":name}),
                );
            }
        }
        if value["status"] == "failed"
            || value["status"] == "reconcile_needed"
            || value["status"] == "abandoned"
        {
            let error = value.get("error").cloned().unwrap_or_default();
            let _ = crate::state_db::update_plugin_operation(
                &conn,
                &op.operation_id,
                value["status"].as_str().unwrap_or("reconcile_needed"),
                task_id.as_deref(),
                None,
                Some(&error.to_string()),
            );
            return Err(AppError::new(
                "PLUGIN_RECOVERY_FAILED",
                error["message"]
                    .as_str()
                    .unwrap_or("Plugin không thể phục hồi media."),
            ));
        }
        if started.elapsed().as_secs() >= lease.poll_max_s {
            return Ok(
                serde_json::json!({"status":"processing","operation_id":op.operation_id,"owner":op.owner,"task_id":task_id}),
            );
        }
        std::thread::sleep(Duration::from_secs(lease.poll_interval_s.max(1)));
        let id = task_id.clone().ok_or_else(|| {
            AppError::new("PLUGIN_BAD_RESPONSE", "Action recovery thiếu task_id.")
        })?;
        value = crate::plugins::client::poll(&op.owner, &id)
            .map_err(|e| AppError::new(&e.code, e.message))?;
        if let Some(id) = value["task_id"].as_str() {
            task_id = Some(id.to_owned());
        }
    }
}

#[cfg(test)]
mod tests;
