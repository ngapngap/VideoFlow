//! Run Engine (Lớp 2 - mục 3): chạy đồng thời 1..=3 dòng theo cấu hình, máy trạng thái 5.3,
//! retry theo bảng 13.5 + Phụ lục v1.3.1, dừng an toàn (5.4), chạy tiếp sau
//! gián đoạn (5.3), versioning _vN (6.10), single-writer ghi CSV (7.2).

use crate::adapters::{self, ApiFail, ApiFailKind, PollOutcome};
use crate::error::{AppError, AppResult};
use crate::prompts_csv::{self, RowStatus};
use crate::settings::Settings;
use rusqlite::Connection;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

const MAX_RETRY_AFTER_S: u64 = 600;

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

// ━━ Lập kế hoạch chạy cho từng dòng (B1 + 13.4 + 6.10 + TC-45) ━━

#[derive(Debug, Clone, Default)]
pub struct EngineConfig {
    pub kind: &'static str,         // chung | rieng | lich | lam_lai
    pub row_keys: Vec<String>,      // rỗng = tất cả dòng đủ điều kiện
    pub force_step: Option<Step>,   // Some(B2) = ép chạy lại B2+B3 (_vN); Some(B3) = ép chạy lại B3 (_vN)
    pub reset_error_attempts: bool, // true cho rerun_error_rows (TC-45) để mở khóa dòng 3/3 lỗi của lượt đó
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowPlan {
    pub steps: Vec<Step>,
    pub force_new_image: bool,
    pub force_new_video: bool,
    pub carried_attempts: i64,
    pub prev_img_hash: Option<String>,
    pub prev_vid_hash: Option<String>,
    pub resume_task_id: Option<String>,
    pub resume_download_url: Option<String>,
}

/// Quyết định một dòng có chạy trong lượt này không và chạy những bước nào.
pub fn plan_row(
    row: &prompts_csv::Row,
    prev: Option<&LatestJobInfo>,
    _cfg: &Settings,
    engine_cfg: &EngineConfig,
    image_out: &Path,
    latest_current_image_in_db: Option<&str>,
    same_image: usize,
) -> Option<RowPlan> {
    if row.is_example() || row.is_empty_content() || row.anh_tham_chieu.trim().is_empty() {
        return None;
    }

    // `row_keys` đến từ UI = khoá dòng (tên ảnh#hash prompt); chỉ cho phép khớp tên ảnh trần
    // khi ảnh dùng cho ĐÚNG một dòng (`same_image == 1`), hoặc khớp chính xác `prev.row_key`.
    if !engine_cfg.row_keys.is_empty() {
        let current = row_key_low(row);
        let image = row.anh_tham_chieu.trim();
        let matched = engine_cfg.row_keys.iter().any(|k| {
            let k = k.trim();
            k.eq_ignore_ascii_case(&current)
                || (same_image == 1 && k.eq_ignore_ascii_case(image))
                || prev.is_some_and(|p| k.eq_ignore_ascii_case(&p.row_key))
        });
        if !matched {
            return None;
        }
    }

    let prev_img_hash = prev.and_then(|p| p.image_prompt_hash.clone());
    let prev_vid_hash = prev.and_then(|p| p.video_prompt_hash.clone());

    // Chế độ làm lại chủ động (rerun_row_step 6.10): ép bước và reset số lần thử về 0,
    // NHƯNG bảo toàn prev_img_hash / prev_vid_hash cho tới khi bước tương ứng thực sự hoàn tất ([P0-2])!
    if let Some(forced) = engine_cfg.force_step {
        return match forced {
            Step::B2 => Some(RowPlan {
                steps: vec![Step::B2, Step::B3],
                force_new_image: true,
                force_new_video: true,
                carried_attempts: 0,
                prev_img_hash,
                prev_vid_hash,
                resume_task_id: None,
                resume_download_url: None,
            }),
            Step::B3 => Some(RowPlan {
                steps: vec![Step::B3],
                force_new_image: false,
                force_new_video: true,
                carried_attempts: 0,
                prev_img_hash,
                prev_vid_hash,
                resume_task_id: None,
                resume_download_url: None,
            }),
        };
    }

    // Không bao giờ chạy dòng `bo_qua` trừ khi người dùng Đặt lại trạng thái hoặc bấm rerun_row_step (6.12)
    if prev.is_some_and(|p| p.status == "bo_qua") {
        return None;
    }

    let (img_changed, vid_changed) = detect_prompt_change(row, prev);
    let prev_attempts = prev.map(|p| p.attempts_count).unwrap_or(0);

    let base_status = effective_row_status(row, prev).unwrap_or_else(|| row.status());

    // 13.4: phát hiện sửa prompt (ngoài app hoặc qua Lưu prompt)
    if img_changed {
        return Some(RowPlan {
            steps: vec![Step::B2, Step::B3],
            force_new_image: true,
            force_new_video: true,
            carried_attempts: 0,
            prev_img_hash,
            prev_vid_hash,
            resume_task_id: None,
            resume_download_url: None,
        });
    }

    if vid_changed {
        let has_valid_img = matches!(
            base_status,
            RowStatus::DaTaoImage | RowStatus::DaTaoVideo | RowStatus::LoiVideo | RowStatus::LoiTamThoi
        ) && !row.anh_da_tao.is_empty()
            && image_out.join(&row.anh_da_tao).is_file();

        return if has_valid_img || row.promt_tao_image.trim().is_empty() {
            Some(RowPlan {
                steps: vec![Step::B3],
                force_new_image: false,
                force_new_video: true,
                carried_attempts: 0,
                prev_img_hash,
                prev_vid_hash,
                resume_task_id: None,
                resume_download_url: None,
            })
        } else {
            Some(RowPlan {
                steps: vec![Step::B2, Step::B3],
                force_new_image: true,
                force_new_video: true,
                carried_attempts: 0,
                prev_img_hash,
                prev_vid_hash,
                resume_task_id: None,
                resume_download_url: None,
            })
        };
    }

    let steps = if base_status == RowStatus::LoiTamThoi {
        let has_valid_img = !row.anh_da_tao.is_empty() && image_out.join(&row.anh_da_tao).is_file();
        if has_valid_img || row.promt_tao_image.trim().is_empty() {
            vec![Step::B3]
        } else {
            vec![Step::B2, Step::B3]
        }
    } else if base_status == RowStatus::Cho && row.promt_tao_image.trim().is_empty() {
        vec![Step::B3]
    } else {
        start_steps(base_status)
    };
    if steps.is_empty() {
        return None;
    }

    let carried_attempts = if engine_cfg.reset_error_attempts {
        0
    } else {
        prev_attempts
    };

    let mut resume_task_id = None;
    let mut resume_download_url = None;
    if steps == vec![Step::B3] {
        if let Some(p) = prev {
            let cur_vid_hash = sha2hex(row.promt_tao_video.trim());
            let same_vid_prompt = p.video_prompt_hash.as_deref() == Some(cur_vid_hash.as_str());
            let same_image_file = match latest_current_image_in_db {
                Some(db_img) => db_img.eq_ignore_ascii_case(&row.anh_da_tao),
                None => !row.anh_da_tao.is_empty() || row.promt_tao_image.trim().is_empty(),
            };
            if p.video_status != "xong" && same_vid_prompt && same_image_file {
                resume_task_id = p.video_task_id.clone().filter(|s| !s.trim().is_empty());
                resume_download_url = p
                    .video_download_url
                    .clone()
                    .filter(|s| !s.trim().is_empty());
            }
        }
    }

    Some(RowPlan {
        steps,
        force_new_image: false,
        force_new_video: false,
        carried_attempts,
        prev_img_hash,
        prev_vid_hash,
        resume_task_id,
        resume_download_url,
    })
}

/// Phân loại hậu quả khi một bước trả `Err(fail)` ([P0-2], [P0-4], [P0-7])
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureResolution {
    pub db_status: &'static str,
    pub csv_label: &'static str,
    pub image_status: &'static str,
    pub video_status: &'static str,
    pub last_error: Option<String>,
    pub count_as_run_error: bool,
    pub trigger_stop: bool,
}

pub fn is_transient_error(fail: &ApiFail) -> bool {
    match fail.kind {
        ApiFailKind::NghiepVu | ApiFailKind::HetQuota => return false,
        ApiFailKind::TamThoi { .. } => {}
    }
    if let Some(status) = fail.http_status {
        if status == 429 || (500..=599).contains(&status) {
            return true;
        }
    }
    let code_low = fail.code.to_lowercase();
    let msg_low = fail.message.to_lowercase();
    let transient_keywords = [
        "cdp",
        "cdperror",
        "connection closed",
        "timeout",
        "timed out",
        "rate_limit",
        "overloaded",
        "502",
        "503",
        "504",
        "poll_timeout",
        "task_transient_error",
        "task_gone",
        "tam_thoi",
    ];
    transient_keywords.iter().any(|kw| code_low.contains(kw) || msg_low.contains(kw))
        || matches!(fail.kind, ApiFailKind::TamThoi { .. })
}

pub fn resolve_step_failure(
    step: Step,
    fail: &ApiFail,
    has_existing_image: bool,
    b2_needs_fresh_image: bool,
) -> FailureResolution {
    if fail.code == "STOPPED" {
        // [P0-2]: Nếu dừng ngay giữa lúc B2 đang cần sinh ảnh mới (redo / sửa prompt ảnh / chưa có ảnh),
        // giữ trạng thái `dang_cho` + `prev_img_hash` chưa đổi để lần START kế tiếp BẮT BUỘC chạy lại B2!
        if step == Step::B2 && b2_needs_fresh_image {
            return FailureResolution {
                db_status: "dang_cho",
                csv_label: "Đang chờ",
                image_status: "chua_chay",
                video_status: "chua_chay",
                last_error: None,
                count_as_run_error: false,
                trigger_stop: true,
            };
        }
        if has_existing_image {
            return FailureResolution {
                db_status: "da_tao_image",
                csv_label: "Đã tạo image",
                image_status: "xong",
                video_status: "chua_chay",
                last_error: None,
                count_as_run_error: false,
                trigger_stop: true,
            };
        }
        return FailureResolution {
            db_status: "dang_cho",
            csv_label: "Đang chờ",
            image_status: "chua_chay",
            video_status: "chua_chay",
            last_error: None,
            count_as_run_error: false,
            trigger_stop: true,
        };
    }

    if fail.code == "REF_IMAGE_MISSING" {
        return FailureResolution {
            db_status: "dang_cho",
            csv_label: "Đang chờ",
            image_status: "chua_chay",
            video_status: "chua_chay",
            last_error: Some(fail.message.clone()),
            count_as_run_error: false,
            trigger_stop: false,
        };
    }

    let is_transient = is_transient_error(fail);
    if is_transient {
        match step {
            Step::B2 => FailureResolution {
                db_status: "loi_tam_thoi",
                csv_label: "Lỗi tạm thời",
                image_status: "chua_chay",
                video_status: "chua_chay",
                last_error: Some(fail.message.clone()),
                count_as_run_error: true,
                trigger_stop: false,
            },
            Step::B3 => FailureResolution {
                db_status: "loi_tam_thoi",
                csv_label: "Lỗi tạm thời",
                image_status: "xong",
                video_status: "loi",
                last_error: Some(fail.message.clone()),
                count_as_run_error: true,
                trigger_stop: false,
            },
        }
    } else {
        match step {
            Step::B2 => FailureResolution {
                db_status: "loi_tao_image",
                csv_label: "Lỗi tạo image",
                image_status: "loi",
                video_status: "chua_chay",
                last_error: Some(fail.message.clone()),
                count_as_run_error: true,
                trigger_stop: fail.kind == ApiFailKind::HetQuota,
            },
            Step::B3 => FailureResolution {
                db_status: "loi_tao_video",
                csv_label: "Lỗi tạo video",
                image_status: "xong",
                video_status: "loi",
                last_error: Some(fail.message.clone()),
                count_as_run_error: true,
                trigger_stop: fail.kind == ApiFailKind::HetQuota,
            },
        }
    }
}

fn finish_job_status(
    ctx: &WorkerRowCtx<'_>,
    row: &mut prompts_csv::Row,
    res: &FailureResolution,
    duration_ms: i64,
) {
    let ts = chrono::Local::now().to_rfc3339();
    let _ = ctx.conn.execute(
        "UPDATE jobs
         SET status = ?1, image_status = ?2, video_status = ?3,
             last_error = ?4, finished_at = ?5, duration_ms = ?6
         WHERE id = ?7",
        rusqlite::params![
            res.db_status,
            res.image_status,
            res.video_status,
            res.last_error,
            ts,
            duration_ms,
            ctx.job_id
        ],
    );

    let new_note = res.last_error.clone().unwrap_or_default();
    if row.trang_thai != res.csv_label || row.ghi_chu_loi != new_note {
        row.trang_thai = res.csv_label.to_string();
        row.ghi_chu_loi = new_note.clone();
        let row_key = crate::prompts_csv::row_key_of(
            &row.anh_tham_chieu,
            &row.promt_tao_image,
            &row.promt_tao_video,
        );
        let label = res.csv_label;
        if let Err(e) = prompts_csv::update_row(ctx.csv_path, &row_key, |r| {
            r.trang_thai = label.into();
            r.ghi_chu_loi = new_note;
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
    }
}

/// Tên file & thời điểm tạo của ảnh/video hiện hành (`is_current = 1`) trong `file_refs`
#[derive(Debug, Clone, Default)]
pub struct CurrentMediaRefs {
    pub image_file: Option<String>,
    pub image_created_at: Option<String>,
    pub video_file: Option<String>,
    pub video_created_at: Option<String>,
}

/// Nạp toàn bộ `file_refs` hiện hành (`is_current = 1`) bằng MỘT câu SQL duy nhất.
pub fn load_current_media_map(conn: &Connection) -> HashMap<String, CurrentMediaRefs> {
    let mut map: HashMap<String, CurrentMediaRefs> = HashMap::new();
    let Ok(mut stmt) = conn.prepare(
        "SELECT row_key, kind, file_name, created_at
         FROM file_refs
         WHERE is_current = 1 AND kind IN ('anh', 'video')
         ORDER BY version ASC, id ASC",
    ) else {
        return map;
    };
    let Ok(it) = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
        ))
    }) else {
        return map;
    };
    for (rk, kind, fname, created_at) in it.flatten() {
        let entry = map.entry(rk.to_lowercase()).or_default();
        let formatted_time = created_at
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(format_rfc3339_to_csv_time);
        match kind.as_str() {
            "anh" => {
                entry.image_file = Some(fname);
                entry.image_created_at = formatted_time;
            }
            "video" => {
                entry.video_file = Some(fname);
                entry.video_created_at = formatted_time;
            }
            _ => {}
        }
    }
    map
}

/// Đồng bộ toàn bộ cột D (`anh_da_Tao`), E (`video_da_tao`), G (`ngay_tao_anh`), H (`ngay_tao_video`),
/// I (`trang_thai`), J (`ghi_chu_loi`) từ `state.db` sang `rows` + `prompts.csv` (13.4 + TC-44).
pub fn sync_rows_from_state_db(
    project_path: &Path,
    conn: &Connection,
    rows: &mut [prompts_csv::Row],
) -> usize {
    let csv_path = project_path.join("prompts.csv");
    let image_out = project_path.join("image_out");
    let video_out = project_path.join("video_out");
    let latest_map = load_latest_jobs_map(conn);
    let media_map = load_current_media_map(conn);

    let mut synced = 0usize;
    // Đếm số dòng dùng mỗi ảnh TRƯỚC khi iter_mut (fallback legacy cần thông tin này
    // mà không được mượn `rows` cùng lúc với `iter_mut`).
    let image_counts: HashMap<String, usize> = rows
        .iter()
        .filter(|r| !r.is_example() && !r.is_empty_content())
        .fold(HashMap::new(), |mut acc, r| {
            *acc.entry(r.anh_tham_chieu.trim().to_lowercase()).or_insert(0) += 1;
            acc
        });

    for r in rows.iter_mut() {
        if r.is_example() || r.is_empty_content() {
            continue;
        }
        let key_low = row_key_low(r);
        // Fallback dữ liệu cũ: jobs/file_refs lưu theo tên ảnh hoặc thông qua file_refs hiện có.
        let legacy_key = r.anh_tham_chieu.trim().to_lowercase();
        let legacy_only = image_counts.get(&legacy_key).copied().unwrap_or(0) == 1;
        let prev = latest_job_for_csv_row(conn, r, &latest_map, &image_counts);
        let media_entry = media_map
            .get(&key_low)
            .or_else(|| prev.as_ref().and_then(|p| media_map.get(&p.row_key.to_lowercase())))
            .or_else(|| {
                if legacy_only && !legacy_key.is_empty() {
                    media_map.get(&legacy_key)
                } else {
                    None
                }
            });
        let mut changed = false;

        if let Some(m) = media_entry {
            if let Some(ref img_f) = m.image_file {
                if !img_f.is_empty() && image_out.join(img_f).is_file() {
                    if r.anh_da_tao != *img_f {
                        r.anh_da_tao = img_f.clone();
                        changed = true;
                        if let Some(ref t) = m.image_created_at {
                            r.ngay_tao_anh = t.clone();
                        }
                    } else if r.ngay_tao_anh.trim().is_empty() {
                        if let Some(ref t) = m.image_created_at {
                            r.ngay_tao_anh = t.clone();
                            changed = true;
                        }
                    }
                }
            }
            if let Some(ref vid_f) = m.video_file {
                if !vid_f.is_empty() && video_out.join(vid_f).is_file() {
                    if r.video_da_tao != *vid_f {
                        r.video_da_tao = vid_f.clone();
                        changed = true;
                        if let Some(ref t) = m.video_created_at {
                            r.ngay_tao_video = t.clone();
                        }
                    } else if r.ngay_tao_video.trim().is_empty() {
                        if let Some(ref t) = m.video_created_at {
                            r.ngay_tao_video = t.clone();
                            changed = true;
                        }
                    }
                }
            }
        }

        if let Some(ref j) = prev {
            let csv_label = match j.status.as_str() {
                "dang_cho" | "bo_qua" => Some("Đang chờ"),
                "da_tao_image" => Some("Đã tạo image"),
                "da_tao_video" => Some("Đã tạo video"),
                "loi_tao_image" => Some("Lỗi tạo image"),
                "loi_tao_video" => Some("Lỗi tạo video"),
                "loi_tam_thoi" => Some("Lỗi tạm thời"),
                _ => None,
            };
            if let Some(lbl) = csv_label {
                if r.trang_thai != lbl {
                    r.trang_thai = lbl.to_string();
                    changed = true;
                }
            }
            let expected_note = if j.status == "bo_qua" {
                "Bị bỏ qua bởi người dùng (Bỏ qua dòng)".to_string()
            } else {
                j.last_error.clone().unwrap_or_default()
            };
            if r.ghi_chu_loi != expected_note {
                r.ghi_chu_loi = expected_note;
                changed = true;
            }
        }

        if changed {
            synced += 1;
        }
    }

    if synced > 0 {
        match prompts_csv::write_all(&csv_path, rows) {
            Ok(()) => crate::settings::log(
                conn,
                "info",
                None,
                None,
                Some("dong_bo"),
                &format!(
                    "Đồng bộ {synced} dòng (trạng thái, thời gian & tên file) từ state.db vào prompts.csv (13.4)"
                ),
            ),
            Err(e) => crate::settings::log(conn, "warn", None, None, Some("ghi_sheet"), &e.message),
        }
    }

    synced
}

/// Đồng bộ ngầm từ `state.db -> prompts.csv` khi mở dự án / nạp lại bảng nếu `prompts.csv` không bị khóa.
pub fn sync_csv_if_unlocked(project_path: &Path) -> usize {
    let csv_path = project_path.join("prompts.csv");
    if is_csv_write_locked(&csv_path) {
        return 0;
    }
    let Ok(mut rows) = prompts_csv::read_all(&csv_path) else {
        return 0;
    };
    let Ok(conn) = crate::state_db::open(project_path) else {
        return 0;
    };
    sync_rows_from_state_db(project_path, &conn, &mut rows)
}

// ━━ Engine chính (Hỗ trợ chạy đồng thời `concurrency = 1..=3` worker threads) ━━

#[derive(Debug, Clone)]
pub struct EngineOutcome {
    pub run_id: i64,
    pub status: String,
    pub done: usize,
    pub errors: usize,
    pub stopped: bool,
    pub duration_ms: i64,
}

pub fn build_run_finished_payload(outcome: &EngineOutcome) -> serde_json::Value {
    serde_json::to_value(RunFinishedEvent {
        run_id: outcome.run_id,
        status: outcome.status.clone(),
        done_count: outcome.done,
        error_count: outcome.errors,
        duration_ms: outcome.duration_ms,
    })
    .unwrap_or_else(|_| {
        serde_json::json!({
            "run_id": outcome.run_id,
            "status": outcome.status,
            "done_count": outcome.done,
            "error_count": outcome.errors,
            "duration_ms": outcome.duration_ms,
        })
    })
}

struct WorkerRowCtx<'a> {
    emitter: EventEmitter<'a>,
    cfg: &'a Settings,
    conn: &'a Connection,
    csv_path: &'a Path,
    image_out: &'a Path,
    video_out: &'a Path,
    stop_flag: &'a AtomicBool,
    run_id: i64,
    job_id: i64,
    total: usize,
    done_count: &'a AtomicUsize,
    active_rows: &'a parking_lot::Mutex<Vec<ActiveRowProgress>>,
}

impl WorkerRowCtx<'_> {
    fn emit_step_progress(&self, row_key: &str, step: &str, detail: String) {
        let snap = {
            let mut active = self.active_rows.lock();
            if step == "done" {
                active.retain(|item| !item.row_key.eq_ignore_ascii_case(row_key));
            } else if let Some(existing) = active
                .iter_mut()
                .find(|item| item.row_key.eq_ignore_ascii_case(row_key))
            {
                existing.step = step.to_string();
                existing.detail = detail.clone();
            } else {
                active.push(ActiveRowProgress {
                    row_key: row_key.to_string(),
                    step: step.to_string(),
                    detail: detail.clone(),
                });
            }
            active.clone()
        };
        let done = self.done_count.load(Ordering::SeqCst);
        emit(
            self.emitter,
            "progress",
            &ProgressEvent {
                run_id: self.run_id,
                done,
                total: self.total,
                percent: (done as f64 / self.total.max(1) as f64) * 100.0,
                current_row_key: row_key.to_string(),
                current_step: step.to_string(),
                step_detail: detail,
                active_count: snap.len(),
                active_rows: snap,
            },
        );
    }

    fn clear_active_row(&self, row_key: &str) {
        self.active_rows
            .lock()
            .retain(|item| !item.row_key.eq_ignore_ascii_case(row_key));
    }

    /// Phát sự kiện `progress` phản ánh đúng danh sách dòng còn đang chạy sau khi một bước kết thúc.
    fn emit_active_snapshot(&self, fallback_row_key: &str) {
        let snap = self.active_rows.lock().clone();
        let done = self.done_count.load(Ordering::SeqCst);
        let (cur_key, cur_step, cur_detail) = if let Some(first) = snap.first() {
            (
                first.row_key.clone(),
                first.step.clone(),
                first.detail.clone(),
            )
        } else {
            (
                fallback_row_key.to_string(),
                "done".to_string(),
                format!("{done}/{} dòng", self.total),
            )
        };
        emit(
            self.emitter,
            "progress",
            &ProgressEvent {
                run_id: self.run_id,
                done,
                total: self.total,
                percent: (done as f64 / self.total.max(1) as f64) * 100.0,
                current_row_key: cur_key,
                current_step: cur_step,
                step_detail: cur_detail,
                active_count: snap.len(),
                active_rows: snap,
            },
        );
    }
}

/// Mỗi tài khoản Muse nội bộ chạy tối đa `MUSE2API_ACCOUNT_MAX_CONCURRENCY = 4` tác vụ đồng thời.
const MUSE_INTERNAL_SLOTS_PER_ACCOUNT: usize = 4;

/// Tính số luồng worker cho hai tầng pipeline tách rời `(image_workers, video_workers)`:
/// - Tầng tạo ảnh (B2) không phụ thuộc video nên chạy song song độc lập và cấp đầu vào cho tầng video.
/// - Tầng tạo video (B3) đợi ảnh xong; nếu có `noi_canh = true`, video ép về 1 luồng theo đúng thứ tự CSV,
///   trong khi luồng ảnh vẫn tiếp tục chạy song song phía trước.
/// - Khi nguồn video là Muse nội bộ (`video_source == "internal"`), số luồng video TUYỆT ĐỐI không được
///   vượt quá số tài khoản Muse khả dụng để bảo đảm mỗi tài khoản chỉ chạy tối đa 1 luồng video duy nhất.
/// - Nếu cả ảnh lẫn video cùng dùng sidecar Muse nội bộ, tổng luồng B2 + B3 được giữ trong ngưỡng slot
///   của các tài khoản khả dụng (`accounts * 4`) để tránh lỗi 503 "all accounts are busy".
pub fn compute_pipeline_workers(
    cfg: &Settings,
    b2_count: usize,
    b3_count: usize,
    has_noi_canh: bool,
    muse_available_accounts: Option<usize>,
) -> (usize, usize) {
    let base = cfg.concurrency.clamp(1, crate::settings::MAX_CONCURRENCY) as usize;
    let effective_base = if cfg.video_source == "internal" {
        base.min(muse_available_accounts.unwrap_or(1).max(1))
    } else {
        base
    };

    let video_workers = if b3_count == 0 {
        0
    } else if has_noi_canh {
        1
    } else {
        effective_base.min(b3_count)
    };

    let mut image_workers = if b2_count == 0 {
        0
    } else {
        effective_base.min(b2_count)
    };

    if image_workers > 0 && cfg.image_source == "internal" {
        let accounts = muse_available_accounts.unwrap_or(1).max(1);
        let max_total_slots = accounts.saturating_mul(MUSE_INTERNAL_SLOTS_PER_ACCOUNT);
        let used_by_video = if cfg.video_source == "internal" {
            video_workers
        } else {
            0
        };
        let max_img_slots = max_total_slots.saturating_sub(used_by_video).max(1);
        image_workers = image_workers.min(max_img_slots);
    }

    (image_workers, video_workers)
}

struct ImageWorkItem {
    seq_idx: usize,
    row: prompts_csv::Row,
    plan: RowPlan,
}

struct VideoWorkItem {
    seq_idx: usize,
    row: prompts_csv::Row,
    plan: RowPlan,
    job_id: Option<i64>,
    b2_duration_ms: i64,
    b2_completed: bool,
}

enum VideoSlot {
    PendingB2,
    Ready(Box<VideoWorkItem>),
    Skipped,
}

struct VideoQueueInner {
    ordered: bool,
    slots: Vec<VideoSlot>,
    next_seq: usize,
    ready_fifo: VecDeque<VideoWorkItem>,
    active_image_workers: usize,
    stop_requested: bool,
}

struct VideoPipelineQueue {
    state: parking_lot::Mutex<VideoQueueInner>,
    cv: parking_lot::Condvar,
}

impl VideoPipelineQueue {
    fn new(ordered: bool, total_slots: usize, active_image_workers: usize) -> Self {
        let slots = if ordered {
            (0..total_slots).map(|_| VideoSlot::PendingB2).collect()
        } else {
            Vec::new()
        };
        Self {
            state: parking_lot::Mutex::new(VideoQueueInner {
                ordered,
                slots,
                next_seq: 0,
                ready_fifo: VecDeque::new(),
                active_image_workers,
                stop_requested: false,
            }),
            cv: parking_lot::Condvar::new(),
        }
    }

    fn push_ready(&self, item: VideoWorkItem) {
        let mut st = self.state.lock();
        if st.ordered {
            let idx = item.seq_idx;
            if idx < st.slots.len() {
                st.slots[idx] = VideoSlot::Ready(Box::new(item));
            }
        } else {
            st.ready_fifo.push_back(item);
        }
        self.cv.notify_one();
    }

    fn mark_skipped(&self, seq_idx: usize) {
        let mut st = self.state.lock();
        if st.ordered && seq_idx < st.slots.len() {
            st.slots[seq_idx] = VideoSlot::Skipped;
            self.cv.notify_all();
        }
    }

    fn finish_image_worker(&self) {
        let mut st = self.state.lock();
        st.active_image_workers = st.active_image_workers.saturating_sub(1);
        if st.active_image_workers == 0 {
            self.cv.notify_all();
        }
    }

    fn request_stop(&self) {
        let mut st = self.state.lock();
        st.stop_requested = true;
        self.cv.notify_all();
    }

    fn pop_next(
        &self,
        stop_flag: &AtomicBool,
        stopped_flag: &AtomicBool,
    ) -> Option<VideoWorkItem> {
        let mut st = self.state.lock();
        loop {
            if stop_flag.load(Ordering::SeqCst)
                || stopped_flag.load(Ordering::SeqCst)
                || st.stop_requested
            {
                stopped_flag.store(true, Ordering::SeqCst);
                st.stop_requested = true;
                self.cv.notify_all();
                return None;
            }

            if st.ordered {
                while st.next_seq < st.slots.len()
                    && matches!(st.slots[st.next_seq], VideoSlot::Skipped)
                {
                    st.next_seq += 1;
                }
                if st.next_seq >= st.slots.len() {
                    return None;
                }
                if matches!(st.slots[st.next_seq], VideoSlot::Ready(_)) {
                    let idx = st.next_seq;
                    let slot = std::mem::replace(&mut st.slots[idx], VideoSlot::Skipped);
                    st.next_seq += 1;
                    if let VideoSlot::Ready(item) = slot {
                        return Some(*item);
                    }
                }
                if st.active_image_workers == 0 {
                    // Không còn luồng ảnh nào chạy; mọi slot PendingB2 còn lại không thể hoàn tất.
                    let idx = st.next_seq;
                    st.slots[idx] = VideoSlot::Skipped;
                    continue;
                }
            } else {
                if let Some(item) = st.ready_fifo.pop_front() {
                    return Some(item);
                }
                if st.active_image_workers == 0 {
                    return None;
                }
            }

            let _ = self.cv.wait_for(&mut st, Duration::from_millis(200));
        }
    }
}

struct ImageWorkerDropGuard<'a>(&'a VideoPipelineQueue);

impl Drop for ImageWorkerDropGuard<'_> {
    fn drop(&mut self) {
        self.0.finish_image_worker();
    }
}

struct PipelineShared<'a> {
    emitter: EventEmitter<'a>,
    cfg: &'a Settings,
    project_path: &'a Path,
    csv_path: &'a Path,
    image_out: &'a Path,
    video_out: &'a Path,
    stop_flag: &'a AtomicBool,
    run_id: i64,
    total: usize,
    done_count: &'a AtomicUsize,
    error_count: &'a AtomicUsize,
    stopped: &'a AtomicBool,
    active_rows: &'a parking_lot::Mutex<Vec<ActiveRowProgress>>,
    fatal_err: &'a parking_lot::Mutex<Option<AppError>>,
    video_queue: &'a VideoPipelineQueue,
}

fn update_runs_progress(
    conn: &Connection,
    run_id: i64,
    done_count: &AtomicUsize,
    error_count: &AtomicUsize,
) {
    let d_now = done_count.load(Ordering::SeqCst) as i64;
    let e_now = error_count.load(Ordering::SeqCst) as i64;
    let _ = conn.execute(
        "UPDATE runs
         SET done_count = MAX(done_count, ?1), error_count = MAX(error_count, ?2)
         WHERE id = ?3",
        rusqlite::params![d_now, e_now, run_id],
    );
}

fn insert_job_for_row(
    conn: &Connection,
    run_id: i64,
    row: &prompts_csv::Row,
    row_key: &str,
    plan: &RowPlan,
    img_ref: &Path,
) -> Result<i64, rusqlite::Error> {
    let prev_before_insert = latest_job_info(conn, row_key);
    let base_st = effective_row_status(row, prev_before_insert.as_ref())
        .unwrap_or_else(|| row.status());
    let (init_img_status, init_db_status) =
        initial_job_image_state(row, base_st, plan, img_ref);

    // [P0-2]: Khởi tạo `image_prompt_hash` và `video_prompt_hash` bằng hash LỊCH SỬ (`prev_*_hash`),
    // CHỈ cập nhật sang hash mới sau khi từng bước thực sự hoàn tất!
    let ts_job = chrono::Local::now().to_rfc3339();
    conn.execute(
        "INSERT INTO jobs(run_id, row_key, row_no, status, image_status, video_status,
                          image_prompt_hash, video_prompt_hash, video_task_id, video_download_url,
                          attempts_count, started_at)
         VALUES (?1,?2,?3,?4,?5,'chua_chay',?6,?7,?8,?9,?10,?11)",
        rusqlite::params![
            run_id,
            row_key,
            row.row_no as i64,
            init_db_status,
            init_img_status,
            plan.prev_img_hash,
            plan.prev_vid_hash,
            plan.resume_task_id,
            plan.resume_download_url,
            plan.carried_attempts,
            ts_job
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

struct StepFailInput<'a> {
    step: Step,
    fail: &'a ApiFail,
    row_key: &'a str,
    plan: &'a RowPlan,
    b2_completed_in_this_job: bool,
    duration_ms: i64,
}

fn handle_step_failure(
    shared: &PipelineShared<'_>,
    ctx: &WorkerRowCtx<'_>,
    row: &mut prompts_csv::Row,
    input: StepFailInput<'_>,
) -> bool {
    let StepFailInput {
        step,
        fail,
        row_key,
        plan,
        b2_completed_in_this_job,
        duration_ms,
    } = input;
    let step_name = if step == Step::B2 { "B2" } else { "B3" };
    let has_img = !row.anh_da_tao.is_empty() && shared.image_out.join(&row.anh_da_tao).is_file();
    let b2_needs_fresh =
        step == Step::B2 && !b2_completed_in_this_job && (plan.force_new_image || !has_img);
    let res = resolve_step_failure(step, fail, has_img, b2_needs_fresh);
    finish_job_status(ctx, row, &res, duration_ms);
    ctx.clear_active_row(row_key);

    if res.count_as_run_error {
        shared.error_count.fetch_add(1, Ordering::SeqCst);
        crate::settings::log(
            ctx.conn,
            "error",
            Some(shared.run_id),
            Some(row_key),
            Some(step_name),
            &fail.message,
        );
    } else {
        let lvl = if fail.code == "STOPPED" {
            "info"
        } else {
            "warn"
        };
        crate::settings::log(
            ctx.conn,
            lvl,
            Some(shared.run_id),
            Some(row_key),
            Some(step_name),
            &fail.message,
        );
    }

    emit(
        shared.emitter,
        "row_changed",
        &RowChangedEvent {
            row_key: row_key.to_string(),
            status: res.csv_label.into(),
            image_status: res.image_status.into(),
            video_status: res.video_status.into(),
            attempts_count: current_job_attempts(ctx.conn, ctx.job_id),
            last_error: res.last_error.clone(),
        },
    );

    let mut should_stop = res.trigger_stop;

    if fail.kind == ApiFailKind::HetQuota || fail.code == "VIDEO_LOCKED" {
        emit(
            shared.emitter,
            "run_error",
            &RunErrorEvent {
                run_id: shared.run_id,
                row_key: Some(row_key.to_string()),
                step: Some(step_name.into()),
                code: fail.code.clone(),
                message: fail.message.clone(),
            },
        );
        if fail.code == "VIDEO_LOCKED" {
            let mut account_id = None;
            let mut account_label = None;
            let acc_file = crate::settings::app_data_dir()
                .join(crate::muse_sidecar::MUSE_DATA_DIR)
                .join("accounts.json");
            if let Ok(content) = std::fs::read_to_string(&acc_file) {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                    if let Some(arr) = val.as_array() {
                        if let Some(locked) = arr.iter().find(|a| {
                            a.get("status").and_then(|s| s.as_str()) == Some("video_locked")
                        }) {
                            account_id = locked
                                .get("id")
                                .and_then(|s| s.as_str())
                                .map(|s| s.to_string());
                            account_label = locked
                                .get("label")
                                .and_then(|s| s.as_str())
                                .map(|s| s.to_string());
                        } else if arr.len() == 1 {
                            account_id = arr[0]
                                .get("id")
                                .and_then(|s| s.as_str())
                                .map(|s| s.to_string());
                            account_label = arr[0]
                                .get("label")
                                .and_then(|s| s.as_str())
                                .map(|s| s.to_string());
                        }
                    }
                }
            }
            let acc_id_val = account_id.clone().unwrap_or_default();
            let acc_lbl_val = account_label.or(account_id).unwrap_or_default();
            emit(
                shared.emitter,
                "account_video_locked",
                &serde_json::json!({
                    "code": "VIDEO_LOCKED",
                    "message": fail.message.clone(),
                    "account_id": acc_id_val,
                    "id": acc_id_val,
                    "label": acc_lbl_val,
                }),
            );
        }
        shared.stop_flag.store(true, Ordering::SeqCst);
        should_stop = true;
    }

    if should_stop {
        shared.stopped.store(true, Ordering::SeqCst);
        shared.video_queue.request_stop();
    }

    update_runs_progress(ctx.conn, shared.run_id, shared.done_count, shared.error_count);
    ctx.emit_active_snapshot(row_key);
    should_stop
}

pub fn run(
    app: &AppHandle,
    cfg: &Settings,
    project_path: &Path,
    engine_cfg: EngineConfig,
    stop_flag: &Arc<AtomicBool>,
) -> AppResult<EngineOutcome> {
    let cb = |name: &str, payload: serde_json::Value| {
        let _ = app.emit(name, payload);
    };
    let outcome =
        run_with_emitter_and_hook(&cb, None, cfg, project_path, engine_cfg, stop_flag)?;
    let _ = app.emit("run_finished", build_run_finished_payload(&outcome));
    Ok(outcome)
}

pub fn run_with_emitter(
    emitter: EventEmitter<'_>,
    cfg: &Settings,
    project_path: &Path,
    engine_cfg: EngineConfig,
    stop_flag: &Arc<AtomicBool>,
) -> AppResult<EngineOutcome> {
    let outcome =
        run_with_emitter_and_hook(emitter, None, cfg, project_path, engine_cfg, stop_flag)?;
    emitter("run_finished", build_run_finished_payload(&outcome));
    Ok(outcome)
}

fn initial_job_image_state(
    row: &prompts_csv::Row,
    base_st: prompts_csv::RowStatus,
    plan: &RowPlan,
    img_dir: &Path,
) -> (&'static str, &'static str) {
    let init_img_status = if !plan.force_new_image
        && matches!(
            base_st,
            prompts_csv::RowStatus::DaTaoImage
                | prompts_csv::RowStatus::DaTaoVideo
                | prompts_csv::RowStatus::LoiVideo
                | prompts_csv::RowStatus::LoiTamThoi
        )
        && !row.anh_da_tao.is_empty()
        && img_dir.join(&row.anh_da_tao).is_file()
    {
        "xong"
    } else {
        "chua_chay"
    };
    let init_db_status = if init_img_status == "xong" {
        "da_tao_image"
    } else {
        "dang_cho"
    };
    (init_img_status, init_db_status)
}

/// Nhân thực thi cốt lõi của Run Engine (hỗ trợ hook nhận `run_id` tức thì và chạy song song `1..=3` luồng).
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
    let has_noi_canh = planned_rows.iter().any(|(r, _)| r.noi_canh);

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
        if let Err(e) = auto_merge_batch(project_path, Some(run_id), Some(&conn), cfg) {
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
            return Err(ApiFail::stopped("Đã dừng an toàn trước khi gọi tạo ảnh"));
        }
        local_try += 1;
        match adapters::create_image(ctx.cfg, &ref_path, &row.promt_tao_image) {
            Ok(res) => break res.png_bytes,
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
    if !video_path.is_file() {
        return Err(format!("File video nguồn không tồn tại: {}", video_path.display()));
    }
    if let Some(parent) = output_frame_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let mut cmd = std::process::Command::new("ffmpeg");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.arg("-y")
        .arg("-sseof")
        .arg("-0.5")
        .arg("-i")
        .arg(video_path)
        .arg("-frames:v")
        .arg("1")
        .arg(output_frame_path);

    let output = cmd.output().map_err(|e| format!("Không thể thực thi lệnh FFmpeg: {e}"))?;
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

// ━━ P2-2: Tự động ghép nối video sau batch (Auto-Merge FFmpeg) ━━

/// Thu thập danh sách các file video thành phẩm theo đúng thứ tự tuần tự của các dòng trong prompts.csv.
pub fn collect_batch_video_paths(csv_path: &Path, video_out_dir: &Path) -> Result<Vec<PathBuf>, String> {
    collect_batch_video_paths_filtered(csv_path, video_out_dir, None)
}

/// Thu thập danh sách các file video thành phẩm theo đúng thứ tự dòng CSV (có hỗ trợ lọc tập dòng nếu cần).
pub fn collect_batch_video_paths_filtered(
    csv_path: &Path,
    video_out_dir: &Path,
    filter_keys: Option<&std::collections::HashSet<String>>,
) -> Result<Vec<PathBuf>, String> {
    if !csv_path.is_file() {
        return Err(format!("File CSV không tồn tại: {}", csv_path.display()));
    }
    let rows = prompts_csv::read_all(csv_path)
        .map_err(|e| format!("Không đọc được prompts.csv: {e}"))?;

    let mut videos = Vec::new();
    for row in rows {
        if row.is_example() || row.is_empty_content() {
            continue;
        }
        let key = prompts_csv::row_key_of(&row.anh_tham_chieu, &row.promt_tao_image, &row.promt_tao_video);
        if let Some(keys) = filter_keys {
            if !keys.contains(&key) && !keys.contains(&row.anh_tham_chieu) {
                continue;
            }
        } else if row.trang_thai.trim().eq_ignore_ascii_case("Bỏ qua") {
            continue;
        }
        let vname = row.video_da_tao.trim();
        if !vname.is_empty() {
            let vpath = video_out_dir.join(vname);
            if vpath.is_file() {
                videos.push(vpath);
            }
        }
    }
    Ok(videos)
}

/// Chuẩn hóa tên dự án an toàn theo chuẩn hệ thống file Windows NTFS (loại bỏ < > : " / \ | ? * và ký tự điều khiển).
pub fn sanitize_ntfs_filename(raw: &str) -> String {
    let forbidden = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
    let sanitized: String = raw
        .chars()
        .map(|c| {
            if c.is_control() || forbidden.contains(&c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = sanitized.trim().trim_matches('.').trim();
    trimmed.to_string()
}

/// Tạo tên file tổng hợp đầu ra: `<project_name>_merged_<timestamp>.mp4` (hoặc `merged_<timestamp>.mp4` nếu tên dự án rỗng).
pub fn generate_merged_output_filename(project_name: &str, timestamp: &str) -> String {
    let clean_name = sanitize_ntfs_filename(project_name);
    if clean_name.is_empty() {
        format!("merged_{timestamp}.mp4")
    } else {
        format!("{clean_name}_merged_{timestamp}.mp4")
    }
}

/// Kiểm tra xem file video có chứa luồng âm thanh hay không bằng FFmpeg (CREATE_NO_WINDOW trên Windows).
pub fn probe_has_audio_ffmpeg(video_path: &Path) -> Result<bool, String> {
    if !video_path.is_file() {
        return Err(format!("File video không tồn tại: {}", video_path.display()));
    }

    let mut cmd = std::process::Command::new("ffmpeg");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.args(["-hide_banner", "-i"]).arg(video_path);

    let output = cmd.output().map_err(|e| format!("Không thể thực thi lệnh FFmpeg: {e}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    let has_audio = stderr.lines().any(|line| line.contains("Stream #") && line.contains("Audio:"));
    Ok(has_audio)
}

/// Tách độ phân giải video từ chuỗi cấu hình `video_size` (mặc định 720x1280).
pub fn parse_video_dimensions(size_str: &str) -> (u32, u32) {
    let parts: Vec<&str> = size_str.split(['x', 'X', ':']).collect();
    if parts.len() == 2 {
        if let (Ok(w), Ok(h)) = (parts[0].trim().parse::<u32>(), parts[1].trim().parse::<u32>()) {
            if w > 0 && h > 0 {
                return (w, h);
            }
        }
    }
    (720, 1280)
}

/// Xây dựng tham số dòng lệnh FFmpeg để chuẩn hóa một clip:
/// - Chuẩn hóa khung hình về cùng tỷ lệ & độ phân giải bằng filter scale & pad.
/// - Đồng bộ FPS (30fps).
/// - Nếu clip thiếu âm thanh: bù track âm thanh câm bằng `anullsrc=channel_layout=stereo:sample_rate=44100` với `-shortest`.
pub fn build_normalize_clip_args(
    input_path: &Path,
    output_path: &Path,
    has_audio: bool,
    width: u32,
    height: u32,
    fps: u32,
) -> Vec<String> {
    let even_w = (width & !1).max(2);
    let even_h = (height & !1).max(2);

    let mut args = vec![
        "-y".into(),
        "-hide_banner".into(),
        "-i".into(),
        input_path.to_string_lossy().to_string(),
    ];

    if !has_audio {
        args.push("-f".into());
        args.push("lavfi".into());
        args.push("-i".into());
        args.push("anullsrc=channel_layout=stereo:sample_rate=44100".into());
        args.push("-map".into());
        args.push("0:v:0".into());
        args.push("-map".into());
        args.push("1:a:0".into());
    }

    let vf = format!(
        "scale={even_w}:{even_h}:force_original_aspect_ratio=decrease,pad={even_w}:{even_h}:(ow-iw)/2:(oh-ih)/2,setsar=1,fps={fps}"
    );
    args.push("-vf".into());
    args.push(vf);

    args.push("-c:v".into());
    args.push("libx264".into());
    args.push("-pix_fmt".into());
    args.push("yuv420p".into());

    args.push("-c:a".into());
    args.push("aac".into());
    args.push("-b:a".into());
    args.push("128k".into());
    args.push("-ar".into());
    args.push("44100".into());
    args.push("-ac".into());
    args.push("2".into());

    if !has_audio {
        args.push("-shortest".into());
    }

    args.push(output_path.to_string_lossy().to_string());
    args
}

/// Xây dựng tham số FFmpeg concat demuxer ghép nối nhanh các clip đã chuẩn hóa.
pub fn build_concat_demuxer_args(list_path: &Path, output_path: &Path) -> Vec<String> {
    vec![
        "-y".into(),
        "-hide_banner".into(),
        "-f".into(),
        "concat".into(),
        "-safe".into(),
        "0".into(),
        "-i".into(),
        list_path.to_string_lossy().to_string(),
        "-c".into(),
        "copy".into(),
        output_path.to_string_lossy().to_string(),
    ]
}

/// Xây dựng tham số FFmpeg filter_complex concat thay thế khi ghép nối luồng trực tiếp.
pub fn build_filter_complex_concat_args(
    inputs: &[PathBuf],
    output_path: &Path,
    input_has_audio: &[bool],
    width: u32,
    height: u32,
    fps: u32,
) -> Vec<String> {
    let mut args = vec!["-y".into(), "-hide_banner".into()];
    for inp in inputs {
        args.push("-i".into());
        args.push(inp.to_string_lossy().to_string());
    }

    let count = inputs.len();
    let mut filter_parts = Vec::new();
    let mut concat_inputs = String::new();

    for i in 0..count {
        let v_label = format!("v{i}");
        let a_label = format!("a{i}");
        filter_parts.push(format!(
            "[{i}:v]scale={width}:{height}:force_original_aspect_ratio=decrease,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2,setsar=1,fps={fps}[{v_label}]"
        ));
        let has_a = input_has_audio.get(i).copied().unwrap_or(true);
        if has_a {
            filter_parts.push(format!("[{i}:a]aformat=sample_rates=44100:channel_layouts=stereo[{a_label}]"));
        } else {
            filter_parts.push(format!("anullsrc=channel_layout=stereo:sample_rate=44100,atrim=duration=10[silent{i}];[silent{i}]aformat=sample_rates=44100:channel_layouts=stereo[{a_label}]"));
        }
        concat_inputs.push_str(&format!("[{v_label}][{a_label}]"));
    }

    filter_parts.push(format!("{concat_inputs}concat=n={count}:v=1:a=1[outv][outa]"));
    let full_filter = filter_parts.join(";");

    args.push("-filter_complex".into());
    args.push(full_filter);
    args.push("-map".into());
    args.push("[outv]".into());
    args.push("-map".into());
    args.push("[outa]".into());
    args.push("-c:v".into());
    args.push("libx264".into());
    args.push("-pix_fmt".into());
    args.push("yuv420p".into());
    args.push("-c:a".into());
    args.push("aac".into());
    args.push(output_path.to_string_lossy().to_string());

    args
}

/// Thực hiện ghép nối tự động các video thành phẩm của batch bằng FFmpeg.
/// Báo lỗi an toàn (fail-safe) để không làm hỏng dữ liệu gốc hoặc crash batch.
pub fn auto_merge_batch(
    project_path: &Path,
    run_id: Option<i64>,
    conn: Option<&Connection>,
    cfg: &Settings,
) -> Result<Option<PathBuf>, String> {
    let video_out = project_path.join("video_out");
    let csv_path = project_path.join("prompts.csv");

    if !video_out.is_dir() || !csv_path.is_file() {
        return Ok(None);
    }

    let video_paths = if let Some(c) = conn {
        let rows = prompts_csv::read_all(&csv_path)
            .map_err(|e| format!("Không đọc được prompts.csv: {e}"))?;
        let latest_map = load_latest_jobs_map(c);
        let media_map = load_current_media_map(c);
        let img_counts = image_counts(&rows);
        let mut paths = Vec::new();
        for row in &rows {
            if row.is_example() || row.is_empty_content() {
                continue;
            }
            if row.trang_thai.trim().eq_ignore_ascii_case("Bỏ qua") {
                continue;
            }
            let prev = latest_job_for_csv_row(c, row, &latest_map, &img_counts);
            if prev.as_ref().map(|j| j.status.as_str()) == Some("bo_qua") {
                continue;
            }
            let key_low = row_key_low(row);
            let mut vname = row.video_da_tao.trim().to_string();
            if vname.is_empty() || !video_out.join(&vname).is_file() {
                let db_vid = media_map
                    .get(&key_low)
                    .or_else(|| prev.as_ref().and_then(|p| media_map.get(&p.row_key.to_lowercase())))
                    .or_else(|| map_lookup_legacy(&media_map, &rows, row))
                    .and_then(|m| m.video_file.as_deref());
                if let Some(vf) = db_vid {
                    vname = vf.trim().to_string();
                }
            }
            if !vname.is_empty() {
                let vpath = video_out.join(&vname);
                if vpath.is_file() {
                    paths.push(vpath);
                }
            }
        }
        paths
    } else {
        collect_batch_video_paths(&csv_path, &video_out)?
    };
    if video_paths.len() < 2 {
        if let Some(c) = conn {
            crate::settings::log(
                c,
                "info",
                run_id,
                None,
                Some("auto_merge"),
                &format!("Số lượng video thành phẩm ({}) ít hơn 2 clip, bỏ qua tự động ghép nối.", video_paths.len()),
            );
        }
        return Ok(None);
    }

    let project_name = crate::project::project_display_name(project_path);
    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
    let out_filename = generate_merged_output_filename(&project_name, &timestamp);
    let final_output = video_out.join(&out_filename);

    let cache_dir = project_path
        .join(".cache")
        .join(format!("merge_{}_{}", timestamp, std::process::id()));
    std::fs::create_dir_all(&cache_dir)
        .map_err(|e| format!("Không tạo được thư mục tạm cho ghép nối video: {e}"))?;

    let (w, h) = parse_video_dimensions(&cfg.video_size);
    let fps = 30;

    // Bước 1: Chuẩn hóa từng clip (độ phân giải, fps, âm thanh câm nếu thiếu)
    let mut normalized_clips = Vec::new();
    for (i, vpath) in video_paths.iter().enumerate() {
        let norm_path = cache_dir.join(format!("norm_{i:04}.mp4"));
        let has_audio = probe_has_audio_ffmpeg(vpath).unwrap_or(false);
        let args = build_normalize_clip_args(vpath, &norm_path, has_audio, w, h, fps);

        let mut cmd = std::process::Command::new("ffmpeg");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        cmd.args(&args);

        let output = cmd
            .output()
            .map_err(|e| {
                let _ = std::fs::remove_dir_all(&cache_dir);
                format!("Không thể thực thi lệnh FFmpeg: {e}")
            })?;

        if !output.status.success() || !norm_path.is_file() {
            let err_msg = String::from_utf8_lossy(&output.stderr);
            let _ = std::fs::remove_dir_all(&cache_dir);
            return Err(format!(
                "FFmpeg chuẩn hóa clip thất bại cho {}: {}",
                vpath.display(),
                err_msg.trim()
            ));
        }
        normalized_clips.push(norm_path);
    }

    // Bước 2: Ghi file danh sách concat demuxer (escape dấu nháy đơn cho FFmpeg)
    let list_file = cache_dir.join("concat_list.txt");
    let mut list_content = String::new();
    for norm_path in &normalized_clips {
        let safe_path = norm_path
            .to_string_lossy()
            .replace('\\', "/")
            .replace('\'', "'\\''");
        list_content.push_str(&format!("file '{safe_path}'\n"));
    }
    if let Err(e) = std::fs::write(&list_file, list_content.as_bytes()) {
        let _ = std::fs::remove_dir_all(&cache_dir);
        return Err(format!("Không thể ghi file concat_list.txt: {e}"));
    }

    // Bước 3: Ghép nối ra file tạm rồi rename nguyên tử sang final_output
    let tmp_output = cache_dir.join("merged_tmp.mp4");
    let concat_args = build_concat_demuxer_args(&list_file, &tmp_output);
    let mut cmd = std::process::Command::new("ffmpeg");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.args(&concat_args);

    let output = cmd
        .output()
        .map_err(|e| {
            let _ = std::fs::remove_dir_all(&cache_dir);
            format!("Không thể thực thi lệnh FFmpeg concat: {e}")
        })?;

    if !output.status.success() || !tmp_output.is_file() {
        let err_msg = String::from_utf8_lossy(&output.stderr);
        let _ = std::fs::remove_dir_all(&cache_dir);
        return Err(format!("FFmpeg ghép nối thất bại: {}", err_msg.trim()));
    }

    let len = std::fs::metadata(&tmp_output)
        .map(|m| m.len())
        .unwrap_or(0);
    if len == 0 {
        let _ = std::fs::remove_dir_all(&cache_dir);
        return Err("File video sau ghép rỗng (0 bytes)".into());
    }

    if let Err(e) = std::fs::rename(&tmp_output, &final_output) {
        // Fallback copy+remove nếu khác ổ đĩa
        if let Err(copy_err) = std::fs::copy(&tmp_output, &final_output) {
            let _ = std::fs::remove_file(&final_output);
            let _ = std::fs::remove_dir_all(&cache_dir);
            return Err(format!("Không thể lưu file video sau ghép: {e} ({copy_err})"));
        }
    }
    let _ = std::fs::remove_dir_all(&cache_dir);

    if let Some(c) = conn {
        crate::settings::log(
            c,
            "info",
            run_id,
            None,
            Some("auto_merge"),
            &format!(
                "Đã tự động ghép nối {} video thành công -> {}",
                video_paths.len(),
                out_filename
            ),
        );
    }

    Ok(Some(final_output))
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

                match extract_last_frame_ffmpeg(&prev_video_path, &extracted_frame_path) {
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

// ━━ Dry-run (6.2b, TC-41) ━━

#[derive(Serialize, Clone)]
pub struct DryRunGroup {
    pub status: String,
    pub count: usize,
    pub row_keys: Vec<String>,
}

#[derive(Serialize, Clone)]
pub struct DryRunResult {
    pub groups: Vec<DryRunGroup>,
    pub total: usize,
    pub is_csv_locked: bool,
}

pub fn dry_run(project_path: &Path, cfg: &Settings) -> AppResult<DryRunResult> {
    let csv_path = project_path.join("prompts.csv");
    let is_csv_locked = is_csv_write_locked(&csv_path);
    let rows = prompts_csv::read_all(&csv_path)?;
    let conn = crate::state_db::open(project_path)
        .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;

    let latest_map = load_latest_jobs_map(&conn);
    let media_map = load_current_media_map(&conn);
    let image_out = project_path.join("image_out");
    let default_engine_cfg = EngineConfig {
        kind: "chung",
        row_keys: Vec::new(),
        force_step: None,
        reset_error_attempts: false,
    };

    let mut groups: Vec<DryRunGroup> = Vec::new();
    let mut push = |status: &str, key: &str| {
        if let Some(g) = groups.iter_mut().find(|g| g.status == status) {
            g.count += 1;
            g.row_keys.push(key.to_string());
        } else {
            groups.push(DryRunGroup {
                status: status.into(),
                count: 1,
                row_keys: vec![key.to_string()],
            });
        }
    };

    let img_counts = image_counts(&rows);
    let mut seen_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in &rows {
        if r.is_example() || r.is_empty_content() {
            continue;
        }
        let key_low = row_key_low(r);
        if !seen_keys.insert(key_low.clone()) {
            continue;
        }
        let prev_owned = latest_job_for_csv_row(&conn, r, &latest_map, &img_counts);
        let prev = prev_owned.as_ref();
        let db_img = media_map
            .get(&key_low)
            .or_else(|| prev.and_then(|p| media_map.get(&p.row_key.to_lowercase())))
            .or_else(|| map_lookup_legacy(&media_map, &rows, r))
            .and_then(|m| m.image_file.as_deref());
        let mut r_eff = r.clone();
        if r_eff.anh_da_tao.is_empty() {
            if let Some(img_f) = db_img {
                if image_out.join(img_f).is_file() {
                    r_eff.anh_da_tao = img_f.to_string();
                }
            }
        }
        let legacy_img = r.anh_tham_chieu.trim().to_lowercase();
        let same_image = img_counts.get(&legacy_img).copied().unwrap_or(0);
        if plan_row(&r_eff, prev, cfg, &default_engine_cfg, &image_out, db_img, same_image).is_some() {
            let eff_label = effective_status_label(&r_eff, prev);
            let group_label = if eff_label == "Đã tạo video" && is_prompt_modified(&r_eff, prev) {
                "Đã sửa prompt (chạy lại)"
            } else {
                eff_label
            };
            push(group_label, &r.anh_tham_chieu);
        }
    }

    let total = groups.iter().map(|g| g.count).sum();
    Ok(DryRunResult {
        groups,
        total,
        is_csv_locked,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompts_csv::Row;

    static TEST_IDX: AtomicUsize = AtomicUsize::new(0);

    fn make_temp_project(tag: &str) -> std::path::PathBuf {
        let idx = TEST_IDX.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "videoflow_engine_test_{}_{}_{}",
            tag,
            std::process::id(),
            idx
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("input")).unwrap();
        std::fs::create_dir_all(dir.join("image_out")).unwrap();
        std::fs::create_dir_all(dir.join("video_out")).unwrap();
        dir
    }

    #[test]
    fn test_next_version_checks_disk_even_when_state_db_recreated() {
        let dir = make_temp_project("next_ver");
        let conn = crate::state_db::open(&dir).unwrap();
        let img_out = dir.join("image_out");

        std::fs::write(img_out.join("cat_gen.png"), b"v1").unwrap();
        std::fs::write(img_out.join("cat_gen_v2.png"), b"v2").unwrap();

        let ver = next_version(&conn, &img_out, "cat.png", "anh");
        assert_eq!(ver, 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_external_prompt_edit_detected_on_completed_video_row_13_4() {
        let dir = make_temp_project("prompt_13_4");
        let img_out = dir.join("image_out");
        std::fs::write(img_out.join("hero_gen.png"), b"png").unwrap();

        let cfg = Settings::default();
        let eng_cfg = EngineConfig::default();

        let mut row = Row {
            row_no: 2,
            anh_tham_chieu: "hero.png".into(),
            promt_tao_image: "prompt anh cu".into(),
            promt_tao_video: "prompt video cu".into(),
            anh_da_tao: "hero_gen.png".into(),
            video_da_tao: "hero_gen.mp4".into(),
            trang_thai: "Đã tạo video".into(),
            ..Default::default()
        };

        let prev = LatestJobInfo {
            id: 1,
            row_key: "hero.png".into(),
            status: "da_tao_video".into(),
            image_status: "xong".into(),
            video_status: "xong".into(),
            image_prompt_hash: Some(sha2hex("prompt anh cu")),
            video_prompt_hash: Some(sha2hex("prompt video cu")),
            attempts_count: 0,
            last_error: None,
            video_task_id: None,
            video_download_url: None,
        };

        assert!(plan_row(&row, Some(&prev), &cfg, &eng_cfg, &img_out, Some("hero_gen.png"), 1).is_none());

        row.promt_tao_video = "prompt video MOI".into();
        let p_vid = plan_row(&row, Some(&prev), &cfg, &eng_cfg, &img_out, Some("hero_gen.png"), 1).unwrap();
        assert_eq!(p_vid.steps, vec![Step::B3]);
        assert!(!p_vid.force_new_image);
        assert!(p_vid.force_new_video);
        assert_eq!(p_vid.carried_attempts, 0);

        row.promt_tao_image = "prompt anh MOI".into();
        let p_img = plan_row(&row, Some(&prev), &cfg, &eng_cfg, &img_out, Some("hero_gen.png"), 1).unwrap();
        assert_eq!(p_img.steps, vec![Step::B2, Step::B3]);
        assert!(p_img.force_new_image);
        assert!(p_img.force_new_video);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_selection_guards_bo_qua_max_attempts_and_rerun_error_rows_unlock() {
        let dir = make_temp_project("guards");
        let img_out = dir.join("image_out");
        let cfg = Settings::default();

        let row = Row {
            row_no: 2,
            anh_tham_chieu: "item.png".into(),
            promt_tao_image: "img".into(),
            promt_tao_video: "vid".into(),
            trang_thai: "Lỗi tạo image".into(),
            ..Default::default()
        };

        let single_or_batch_cfg = EngineConfig {
            kind: "rieng",
            row_keys: vec!["item.png".into()],
            force_step: None,
            reset_error_attempts: false,
        };

        let prev_skipped = LatestJobInfo {
            id: 1,
            row_key: "item.png".into(),
            status: "bo_qua".into(),
            image_status: "chua_chay".into(),
            video_status: "chua_chay".into(),
            image_prompt_hash: Some(sha2hex("img")),
            video_prompt_hash: Some(sha2hex("vid")),
            attempts_count: 0,
            last_error: None,
            video_task_id: None,
            video_download_url: None,
        };
        assert!(plan_row(&row, Some(&prev_skipped), &cfg, &single_or_batch_cfg, &img_out, None, 1).is_none());

        let prev_maxed = LatestJobInfo {
            status: "loi_tao_image".into(),
            attempts_count: 3,
            ..prev_skipped.clone()
        };
        // Issue #6 [P0-6]: Bỏ khóa cứng, dòng 3/3 lỗi vẫn được plan để chạy lại (không bị chặn)
        assert!(plan_row(&row, Some(&prev_maxed), &cfg, &single_or_batch_cfg, &img_out, None, 1).is_some());

        // rerun_error_rows (reset_error_attempts: true) mở khóa các dòng lỗi của lượt trước và reset carried_attempts = 0!
        let rerun_err_cfg = EngineConfig {
            kind: "lam_lai",
            row_keys: vec!["item.png".into()],
            force_step: None,
            reset_error_attempts: true,
        };
        let unlocked = plan_row(&row, Some(&prev_maxed), &cfg, &rerun_err_cfg, &img_out, None, 1)
            .expect("rerun_error_rows must plan error rows");
        assert_eq!(unlocked.carried_attempts, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_video_task_resume_and_b2_redo_stop_preserves_pending_b2() {
        let stop_b2_redo = resolve_step_failure(Step::B2, &ApiFail::stopped("stop"), true, true);
        assert_eq!(
            stop_b2_redo.db_status, "dang_cho",
            "stopping mid-B2 redo must keep dang_cho so next START still executes B2"
        );
        assert_eq!(stop_b2_redo.image_status, "chua_chay");

        let stop_b3 = resolve_step_failure(Step::B3, &ApiFail::stopped("stop"), true, false);
        assert_eq!(stop_b3.db_status, "da_tao_image");
        assert_eq!(stop_b3.image_status, "xong");

        let b3_fail = ApiFail {
            kind: ApiFailKind::TamThoi { retry_after_s: None },
            http_status: Some(502),
            code: "HTTP_502".into(),
            message: "bad gateway".into(),
        };
        let b3_res = resolve_step_failure(Step::B3, &b3_fail, true, false);
        assert_eq!(b3_res.db_status, "loi_tam_thoi");
        assert_eq!(b3_res.image_status, "xong");

        let b3_biz_fail = ApiFail {
            kind: ApiFailKind::NghiepVu,
            http_status: None,
            code: "POLICY_VIOLATION".into(),
            message: "prompt rejected".into(),
        };
        let b3_biz_res = resolve_step_failure(Step::B3, &b3_biz_fail, true, false);
        assert_eq!(b3_biz_res.db_status, "loi_tao_video");
        assert_eq!(b3_biz_res.image_status, "xong");
    }

    #[test]
    fn test_tc44_excel_lock_recovery_syncs_media_files_and_timestamps() {
        let dir = make_temp_project("tc44_sync");
        std::fs::write(dir.join("image_out").join("lock_gen.png"), b"png_data").unwrap();
        std::fs::write(dir.join("video_out").join("lock_gen.mp4"), b"mp4_data").unwrap();

        let csv_path = dir.join("prompts.csv");
        prompts_csv::write_all(
            &csv_path,
            &[Row {
                row_no: 2,
                anh_tham_chieu: "lock.png".into(),
                promt_tao_image: "p_img".into(),
                promt_tao_video: "p_vid".into(),
                trang_thai: "Đang chờ".into(),
                ..Default::default()
            }],
        )
        .unwrap();

        let conn = crate::state_db::open(&dir).unwrap();
        conn.execute(
            "INSERT INTO runs(kind, status, started_at, total_rows) VALUES ('chung', 'hoan_tat', '2026-11-18T00:00:00Z', 1)",
            [],
        )
        .unwrap();
        let rid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO jobs(run_id, row_key, row_no, status, image_status, video_status, image_prompt_hash, video_prompt_hash)
             VALUES (?1, 'lock.png', 2, 'da_tao_video', 'xong', 'xong', ?2, ?3)",
            rusqlite::params![rid, sha2hex("p_img"), sha2hex("p_vid")],
        )
        .unwrap();
        let jid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
             VALUES (?1, 'lock.png', 'anh', 'lock_gen.png', 'image_out/lock_gen.png', 1, 1, '2026-11-18T08:15:00+07:00')",
            rusqlite::params![jid],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
             VALUES (?1, 'lock.png', 'video', 'lock_gen.mp4', 'video_out/lock_gen.mp4', 1, 1, '2026-11-18T08:20:00+07:00')",
            rusqlite::params![jid],
        )
        .unwrap();

        let synced = sync_csv_if_unlocked(&dir);
        assert_eq!(synced, 1);

        let reread = prompts_csv::read_all(&csv_path).unwrap();
        assert_eq!(reread[0].anh_da_tao, "lock_gen.png");
        assert_eq!(reread[0].video_da_tao, "lock_gen.mp4");
        assert!(!reread[0].ngay_tao_anh.is_empty(), "must sync col G ngay_tao_anh");
        assert!(!reread[0].ngay_tao_video.is_empty(), "must sync col H ngay_tao_video");
        assert_eq!(reread[0].trang_thai, "Đã tạo video");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ━━━━━━━━━━━━━━━━━━━━━━ Real Local-HTTP End-to-End Integration Suite ━━━━━━━━━━━━━━━━━━━━━━

    struct RawHttpReq {
        method: String,
        path: String,
        headers: HashMap<String, String>,
        body: Vec<u8>,
    }

    struct RawHttpResp {
        status: u16,
        reason: &'static str,
        content_type: &'static str,
        body: Vec<u8>,
    }

    impl RawHttpResp {
        fn json(status: u16, reason: &'static str, val: serde_json::Value) -> Self {
            Self {
                status,
                reason,
                content_type: "application/json",
                body: serde_json::to_vec(&val).unwrap(),
            }
        }

        fn binary(status: u16, reason: &'static str, content_type: &'static str, body: Vec<u8>) -> Self {
            Self {
                status,
                reason,
                content_type,
                body,
            }
        }
    }

    fn read_http_request(stream: &mut std::net::TcpStream) -> Option<RawHttpReq> {
        use std::io::Read as _;
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let header_end = loop {
            match stream.read(&mut chunk) {
                Ok(0) => return None,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos + 4;
                    }
                }
                Err(_) => return None,
            }
        };

        let head_str = String::from_utf8_lossy(&buf[..header_end - 4]);
        let mut lines = head_str.split("\r\n");
        let req_line = lines.next()?;
        let mut parts = req_line.split_whitespace();
        let method = parts.next()?.to_string();
        let path = parts.next()?.to_string();

        let mut headers = HashMap::new();
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }

        let mut body = buf[header_end..].to_vec();
        if let Some(cl_str) = headers.get("content-length") {
            if let Ok(cl) = cl_str.parse::<usize>() {
                while body.len() < cl {
                    match stream.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => body.extend_from_slice(&chunk[..n]),
                        Err(_) => break,
                    }
                }
                body.truncate(cl);
            }
        } else if headers
            .get("transfer-encoding")
            .is_some_and(|te| te.to_ascii_lowercase().contains("chunked"))
        {
            while !body.windows(5).any(|w| w == b"0\r\n\r\n") {
                match stream.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => body.extend_from_slice(&chunk[..n]),
                    Err(_) => break,
                }
            }
        }

        Some(RawHttpReq {
            method,
            path,
            headers,
            body,
        })
    }

    fn spawn_mock_http_server<F>(
        handler: F,
    ) -> (u16, Arc<AtomicBool>, std::thread::JoinHandle<()>)
    where
        F: Fn(RawHttpReq) -> RawHttpResp + Send + Sync + 'static,
    {
        use std::io::Write as _;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_bg = shutdown.clone();
        let handler = Arc::new(handler);

        let join = std::thread::spawn(move || {
            while !shutdown_bg.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let handler_req = handler.clone();
                        // Xử lý từng kết nối trên luồng con để kiểm thử thực tế `concurrency = 2..3` song song!
                        std::thread::spawn(move || {
                            if let Some(req) = read_http_request(&mut stream) {
                                let resp = handler_req(req);
                                let head = format!(
                                    "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                    resp.status,
                                    resp.reason,
                                    resp.content_type,
                                    resp.body.len()
                                );
                                let _ = stream.write_all(head.as_bytes());
                                let _ = stream.write_all(&resp.body);
                                let _ = stream.flush();
                            }
                        });
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        (port, shutdown, join)
    }

    fn sample_png_bytes(r: u8, g: u8, b: u8) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([r, g, b, 255]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    fn sample_jpeg_b64(r: u8, g: u8, b: u8) -> String {
        use base64::Engine as _;
        let img = image::RgbImage::from_pixel(4, 4, image::Rgb([r, g, b]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Jpeg)
            .unwrap();
        base64::engine::general_purpose::STANDARD.encode(out)
    }

    fn sample_mp4_bytes(tag: &[u8; 4]) -> Vec<u8> {
        let mut mp4 = vec![0x00, 0x00, 0x00, 0x18];
        mp4.extend_from_slice(b"ftypmp42");
        mp4.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        mp4.extend_from_slice(b"mp42isom");
        mp4.extend_from_slice(tag);
        mp4
    }

    #[test]
    fn test_e2e_http_concurrency_2_origin_boundary_and_v2_prompt_versioning() {
        let dir = make_temp_project("e2e_conc2_v2");
        std::fs::write(dir.join("input").join("shoe.png"), sample_png_bytes(10, 20, 30)).unwrap();
        std::fs::write(dir.join("input").join("hat.png"), sample_png_bytes(40, 50, 60)).unwrap();

        prompts_csv::write_all(
            &dir.join("prompts.csv"),
            &[
                Row {
                    row_no: 2,
                    anh_tham_chieu: "shoe.png".into(),
                    promt_tao_image: "anh giay studio v1".into(),
                    promt_tao_video: "video xoay 360 v1".into(),
                    trang_thai: "Đang chờ".into(),
                    ..Default::default()
                },
                Row {
                    row_no: 3,
                    anh_tham_chieu: "hat.png".into(),
                    promt_tao_image: "anh mu v1".into(),
                    promt_tao_video: "video mu v1".into(),
                    trang_thai: "Đang chờ".into(),
                    ..Default::default()
                },
            ],
        )
        .unwrap();

        let s3_had_auth = Arc::new(AtomicBool::new(false));
        let s3_auth_ref = s3_had_auth.clone();
        let (port_b, stop_b, join_b) = spawn_mock_http_server(move |req| {
            if req.headers.contains_key("authorization") {
                s3_auth_ref.store(true, Ordering::SeqCst);
            }
            RawHttpResp::binary(200, "OK", "video/mp4", sample_mp4_bytes(b"VIDX"))
        });

        let img_had_mime = Arc::new(AtomicBool::new(false));
        let vid_had_idem = Arc::new(AtomicBool::new(false));
        let create_vid_seq = Arc::new(AtomicUsize::new(0));
        let poll_counts: Arc<parking_lot::Mutex<HashMap<String, usize>>> =
            Arc::new(parking_lot::Mutex::new(HashMap::new()));

        let img_mime_ref = img_had_mime.clone();
        let vid_idem_ref = vid_had_idem.clone();
        let create_seq_ref = create_vid_seq.clone();
        let poll_map_ref = poll_counts.clone();

        let (port_a, stop_a, join_a) = spawn_mock_http_server(move |req| {
            if req.method == "POST" && req.path == "/v1/images/edits" {
                let body_lossy = String::from_utf8_lossy(&req.body);
                if body_lossy.to_ascii_lowercase().contains("content-type: image/png") {
                    img_mime_ref.store(true, Ordering::SeqCst);
                }
                return RawHttpResp::json(
                    200,
                    "OK",
                    serde_json::json!({ "data": [{ "b64_json": sample_jpeg_b64(80, 160, 240) }] }),
                );
            }

            if req.method == "POST" && req.path == "/v1/videos" {
                if req.headers.contains_key("idempotency-key") {
                    vid_idem_ref.store(true, Ordering::SeqCst);
                }
                let n = create_seq_ref.fetch_add(1, Ordering::SeqCst) + 1;
                return RawHttpResp::json(
                    201,
                    "Created",
                    serde_json::json!({ "id": format!("task_conc_{n}") }),
                );
            }

            if req.method == "GET" && req.path.starts_with("/v1/videos/task_conc_") {
                let mut pm = poll_map_ref.lock();
                let count = pm.entry(req.path.clone()).or_insert(0);
                *count += 1;
                let c = *count;
                drop(pm);
                return if c == 1 {
                    RawHttpResp::json(
                        200,
                        "OK",
                        serde_json::json!({ "status": "in_progress", "progress": 55 }),
                    )
                } else {
                    RawHttpResp::json(
                        200,
                        "OK",
                        serde_json::json!({
                            "status": "succeeded",
                            "progress": 100,
                            "video_url": format!("http://127.0.0.1:{port_b}/presigned/out.mp4?X-Amz-Signature=sig")
                        }),
                    )
                };
            }

            RawHttpResp::json(404, "Not Found", serde_json::json!({}))
        });

        let api_base = format!("http://127.0.0.1:{port_a}/v1");
        adapters::register_test_api_key(&api_base, "sk-e2e-api-secret");

        let cfg = Settings {
            endpoint_image_base_url: api_base.clone(),
            endpoint_video_base_url: api_base,
            concurrency: 2, // Chạy song song 2 dòng thực tế!
            poll_interval_s: 1,
            poll_max_s: 30,
            backoff_s: [1, 1, 1],
            ..Settings::default()
        };

        let max_seen_active = Arc::new(AtomicUsize::new(0));
        let max_active_ref = max_seen_active.clone();
        let emitter = move |name: &str, val: serde_json::Value| {
            if name == "progress" {
                if let Some(ac) = val.get("active_count").and_then(|v| v.as_u64()) {
                    max_active_ref.fetch_max(ac as usize, Ordering::SeqCst);
                }
            }
        };
        let stop_flag = Arc::new(AtomicBool::new(false));

        let out1 = run_with_emitter(&emitter, &cfg, &dir, EngineConfig::default(), &stop_flag)
            .expect("concurrency=2 E2E run must succeed");
        assert_eq!(out1.done, 2);
        assert_eq!(out1.errors, 0);
        assert!(img_had_mime.load(Ordering::SeqCst));
        assert!(vid_had_idem.load(Ordering::SeqCst), "POST /v1/videos must send Idempotency-Key");
        assert!(!s3_had_auth.load(Ordering::SeqCst), "Port B must NOT receive Bearer token");
        assert!(
            max_seen_active.load(Ordering::SeqCst) >= 2,
            "concurrency=2 must execute 2 active rows concurrently"
        );
        assert!(dir.join("video_out").join("shoe_gen.mp4").is_file());
        assert!(dir.join("video_out").join("hat_gen.mp4").is_file());

        // Sửa prompt shoe.png -> bấm STOP ngay trước B2 redo -> kiểm chứng prev_img_hash cũ được giữ nguyên
        // và lần chạy kế tiếp VẪN chạy đủ B2 + B3 sinh _v2! ([P0-2])
        let edited = crate::commands::save_row_prompts_at(
            &dir,
            "shoe.png",
            "anh giay studio v2",
            "video xoay 360 v2",
        )
        .unwrap();
        assert!(edited.row.prompt_modified);

        stop_flag.store(true, Ordering::SeqCst);
        let out_stopped_b2 =
            run_with_emitter(&emitter, &cfg, &dir, EngineConfig::default(), &stop_flag)
                .expect("mid-B2 redo stop must succeed cleanly");
        assert!(out_stopped_b2.stopped);
        assert_eq!(out_stopped_b2.done, 0);
        assert_eq!(out_stopped_b2.errors, 0);
        assert!(
            !dir.join("image_out").join("shoe_gen_v2.png").exists(),
            "shoe_gen_v2.png must not exist when stopped before B2"
        );

        // Bỏ cờ dừng -> chạy lượt kế tiếp phải bắt buộc chạy lại B2 + B3 sinh _v2!
        stop_flag.store(false, Ordering::SeqCst);
        let out2 = run_with_emitter(&emitter, &cfg, &dir, EngineConfig::default(), &stop_flag)
            .expect("v2 prompt edit run");
        assert_eq!(out2.done, 1);
        assert!(dir.join("image_out").join("shoe_gen.png").is_file());
        assert!(dir.join("image_out").join("shoe_gen_v2.png").is_file());
        assert!(dir.join("video_out").join("shoe_gen_v2.mp4").is_file());

        stop_a.store(true, Ordering::SeqCst);
        stop_b.store(true, Ordering::SeqCst);
        let _ = join_a.join();
        let _ = join_b.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_e2e_http_resume_quota_attempts_cap_3_of_3_and_reset_to_zero_on_success() {
        let dir = make_temp_project("e2e_resume_cap");
        std::fs::write(dir.join("input").join("bag.png"), sample_png_bytes(50, 60, 70)).unwrap();
        std::fs::write(
            dir.join("image_out").join("bag_gen.png"),
            sample_png_bytes(80, 90, 100),
        )
        .unwrap();

        prompts_csv::write_all(
            &dir.join("prompts.csv"),
            &[Row {
                row_no: 2,
                anh_tham_chieu: "bag.png".into(),
                promt_tao_image: "bag img".into(),
                promt_tao_video: "bag vid".into(),
                anh_da_tao: "bag_gen.png".into(),
                trang_thai: "Đã tạo image".into(),
                ..Default::default()
            }],
        )
        .unwrap();
        // Engine lưu jobs theo KHOÁ DÒNG (tên ảnh + hash prompt), không phải tên ảnh.
        let bag_key = crate::prompts_csv::row_key_of("bag.png", "bag img", "bag vid");

        let phase = Arc::new(AtomicUsize::new(1));
        let post_video_calls = Arc::new(AtomicUsize::new(0));
        let trigger_stop_on_poll = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::new(AtomicBool::new(false));

        let phase_b = phase.clone();
        let (port_b, stop_b, join_b) = spawn_mock_http_server(move |_req| {
            if phase_b.load(Ordering::SeqCst) == 2 {
                RawHttpResp::json(500, "Internal Server Error", serde_json::json!({ "error": "cdn down" }))
            } else {
                RawHttpResp::binary(200, "OK", "video/mp4", sample_mp4_bytes(b"RESM"))
            }
        });

        let phase_a = phase.clone();
        let post_calls_a = post_video_calls.clone();
        let trig_stop_a = trigger_stop_on_poll.clone();
        let stop_flag_a = stop_flag.clone();
        let (port_a, stop_a, join_a) = spawn_mock_http_server(move |req| {
            if req.method == "POST" && req.path == "/v1/videos" {
                let c = post_calls_a.fetch_add(1, Ordering::SeqCst) + 1;
                return RawHttpResp::json(
                    200,
                    "OK",
                    serde_json::json!({ "id": format!("task_quota_{c}") }),
                );
            }
            if req.method == "GET" && req.path.starts_with("/v1/videos/") {
                if trig_stop_a.load(Ordering::SeqCst) {
                    stop_flag_a.store(true, Ordering::SeqCst);
                    return RawHttpResp::json(
                        200,
                        "OK",
                        serde_json::json!({ "status": "running", "progress": 25 }),
                    );
                }
                if phase_a.load(Ordering::SeqCst) == 1 {
                    return RawHttpResp::json(
                        200,
                        "OK",
                        serde_json::json!({ "status": "running", "progress": 0 }),
                    );
                }
                if phase_a.load(Ordering::SeqCst) == 99 {
                    return RawHttpResp::json(
                        502,
                        "Bad Gateway",
                        serde_json::json!({ "error": "gateway down" }),
                    );
                }
                return RawHttpResp::json(
                    200,
                    "OK",
                    serde_json::json!({
                        "status": "succeeded",
                        "video_url": format!("http://127.0.0.1:{port_b}/cdn/bag.mp4")
                    }),
                );
            }
            RawHttpResp::json(404, "Not Found", serde_json::json!({}))
        });

        let api_base = format!("http://127.0.0.1:{port_a}/v1");
        adapters::register_test_api_key(&api_base, "sk-quota-key");
        let cfg = Settings {
            endpoint_image_base_url: api_base.clone(),
            endpoint_video_base_url: api_base,
            poll_interval_s: 1,
            poll_max_s: 1,
            backoff_s: [1, 1, 1],
            max_attempts: 3,
            ..Settings::default()
        };
        let noop_emit = |_name: &str, _val: serde_json::Value| {};

        // 1) Dừng an toàn mid-poll
        trigger_stop_on_poll.store(true, Ordering::SeqCst);
        let out_stop =
            run_with_emitter(&noop_emit, &cfg, &dir, EngineConfig::default(), &stop_flag).unwrap();
        assert!(out_stop.stopped);
        assert_eq!(out_stop.errors, 0);

        // 2) POLL_TIMEOUT -> transient error: ghi attempts nhưng attempts_count = 0, status = loi_tam_thoi
        trigger_stop_on_poll.store(false, Ordering::SeqCst);
        stop_flag.store(false, Ordering::SeqCst);
        let _ = run_with_emitter(&noop_emit, &cfg, &dir, EngineConfig::default(), &stop_flag).unwrap();
        {
            let conn = crate::state_db::open(&dir).unwrap();
            let job = latest_job_info(&conn, &bag_key).unwrap();
            assert_eq!(job.status, "loi_tam_thoi");
            assert_eq!(job.attempts_count, 0, "transient error must not increment attempts_count");
        }

        // 3) Poll gặp HTTP 502 liên tiếp -> transient error dừng sau local retry, attempts_count = 0
        phase.store(99, Ordering::SeqCst);
        let _ = run_with_emitter(&noop_emit, &cfg, &dir, EngineConfig::default(), &stop_flag).unwrap();
        {
            let conn = crate::state_db::open(&dir).unwrap();
            let job = latest_job_info(&conn, &bag_key).unwrap();
            assert_eq!(job.status, "loi_tam_thoi");
            assert_eq!(job.attempts_count, 0, "transient 502 error must not increment attempts_count");
        }

        // 4) Chạy lại bằng `reset_error_attempts: true` (TC-45 rerun_error_rows) ở Phase 2 (DOWNLOAD_FAIL) rồi Phase 3 (OK)
        phase.store(2, Ordering::SeqCst);
        let rerun_cfg = EngineConfig {
            kind: "lam_lai",
            row_keys: vec!["bag.png".into()],
            force_step: None,
            reset_error_attempts: true,
        };
        let _ = run_with_emitter(&noop_emit, &cfg, &dir, rerun_cfg, &stop_flag).unwrap();
        {
            let conn = crate::state_db::open(&dir).unwrap();
            assert_eq!(
                latest_job_info(&conn, &bag_key).unwrap().attempts_count,
                0,
                "DOWNLOAD_FAIL must not increment generation attempts_count"
            );
        }

        phase.store(3, Ordering::SeqCst);
        let out_ok =
            run_with_emitter(&noop_emit, &cfg, &dir, EngineConfig::default(), &stop_flag).unwrap();
        assert_eq!(out_ok.done, 1);
        assert_eq!(
            post_video_calls.load(Ordering::SeqCst),
            1,
            "POST /v1/videos must still be called ONLY ONCE across all 5 runs!"
        );
        {
            let conn = crate::state_db::open(&dir).unwrap();
            assert_eq!(
                latest_job_info(&conn, &bag_key).unwrap().attempts_count,
                0,
                "[P1-1]: attempts_count must reset to 0 on row completion"
            );
        }

        stop_a.store(true, Ordering::SeqCst);
        stop_b.store(true, Ordering::SeqCst);
        let _ = join_a.join();
        let _ = join_b.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_p0_6_is_transient_error_classification() {
        let t_502 = ApiFail {
            kind: ApiFailKind::TamThoi { retry_after_s: None },
            http_status: Some(502),
            code: "502".into(),
            message: "Bad gateway".into(),
        };
        assert!(is_transient_error(&t_502));

        let t_429 = ApiFail {
            kind: ApiFailKind::TamThoi { retry_after_s: Some(10) },
            http_status: Some(429),
            code: "RATE_LIMIT".into(),
            message: "Too many requests".into(),
        };
        assert!(is_transient_error(&t_429));

        let t_cdp = ApiFail {
            kind: ApiFailKind::TamThoi { retry_after_s: None },
            http_status: None,
            code: "CDP_CLOSED".into(),
            message: "cdp connection closed".into(),
        };
        assert!(is_transient_error(&t_cdp));

        let t_timeout = ApiFail {
            kind: ApiFailKind::TamThoi { retry_after_s: None },
            http_status: None,
            code: "POLL_TIMEOUT".into(),
            message: "poll timed out".into(),
        };
        assert!(is_transient_error(&t_timeout));

        let business_refused = ApiFail {
            kind: ApiFailKind::NghiepVu,
            http_status: Some(502),
            code: "UPSTREAM_REFUSED".into(),
            message: "Prompt refused by safety policy".into(),
        };
        assert!(!is_transient_error(&business_refused));

        let quota_err = ApiFail {
            kind: ApiFailKind::HetQuota,
            http_status: Some(429),
            code: "QUOTA_EXHAUSTED".into(),
            message: "Quota out".into(),
        };
        assert!(!is_transient_error(&quota_err));
    }

    #[test]
    fn test_p0_6_resolve_step_failure_sets_loi_tam_thoi() {
        let transient_fail = ApiFail {
            kind: ApiFailKind::TamThoi { retry_after_s: None },
            http_status: Some(502),
            code: "502".into(),
            message: "Gateway Error".into(),
        };
        let res_b2 = resolve_step_failure(Step::B2, &transient_fail, false, false);
        assert_eq!(res_b2.db_status, "loi_tam_thoi");
        assert_eq!(res_b2.csv_label, "Lỗi tạm thời");

        let res_b3 = resolve_step_failure(Step::B3, &transient_fail, true, false);
        assert_eq!(res_b3.db_status, "loi_tam_thoi");
        assert_eq!(res_b3.csv_label, "Lỗi tạm thời");

        let business_fail = ApiFail {
            kind: ApiFailKind::NghiepVu,
            http_status: None,
            code: "UPSTREAM_REFUSED".into(),
            message: "Refused".into(),
        };
        let res_biz_b2 = resolve_step_failure(Step::B2, &business_fail, false, false);
        assert_eq!(res_biz_b2.db_status, "loi_tao_image");
        assert_eq!(res_biz_b2.csv_label, "Lỗi tạo image");

        let res_biz_b3 = resolve_step_failure(Step::B3, &business_fail, true, false);
        assert_eq!(res_biz_b3.db_status, "loi_tao_video");
        assert_eq!(res_biz_b3.csv_label, "Lỗi tạo video");
    }

    #[test]
    fn test_p0_6_record_attempt_transient_vs_business() {
        let dir = make_temp_project("p06_record_attempt");
        let conn = crate::state_db::open(&dir).unwrap();
        conn.execute(
            "INSERT INTO runs(kind, status, started_at, total_rows) VALUES ('chung', 'dang_chay', '2026-01-01', 1)",
            [],
        ).unwrap();
        let run_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO jobs(run_id, row_key, row_no, status, image_status, video_status, attempts_count)
             VALUES (?1, 'row1.png', 1, 'dang_cho', 'chua_chay', 'chua_chay', 0)",
            rusqlite::params![run_id],
        ).unwrap();
        let job_id = conn.last_insert_rowid();

        // 1. Transient attempt: count_toward_limit = false
        let total_after_transient = record_attempt(
            &conn,
            job_id,
            AttemptRecord {
                step: "B2",
                fallback_attempt_no: 1,
                result: "loi_tam_thoi",
                http_status: Some(502),
                error_code: "502",
                error_message: "Gateway Error",
                count_toward_limit: false,
            },
        );
        assert_eq!(total_after_transient, 0);
        assert_eq!(current_job_attempts(&conn, job_id), 0);

        // 2. Business attempt: count_toward_limit = true
        let total_after_biz = record_attempt(
            &conn,
            job_id,
            AttemptRecord {
                step: "B2",
                fallback_attempt_no: 2,
                result: "loi_nghiep_vu",
                http_status: None,
                error_code: "POLICY_VIOLATION",
                error_message: "Prompt refused",
                count_toward_limit: true,
            },
        );
        assert_eq!(total_after_biz, 1);
        assert_eq!(current_job_attempts(&conn, job_id), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_p0_6_reset_row_status_at_legacy_fallback() {
        let dir = make_temp_project("p06_reset_legacy");
        let conn = crate::state_db::open(&dir).unwrap();
        conn.execute(
            "INSERT INTO runs(kind, status, started_at, total_rows) VALUES ('chung', 'dang_chay', '2026-01-01', 1)",
            [],
        ).unwrap();
        let run_id = conn.last_insert_rowid();
        // Insert job with legacy row_key (image name without hash)
        conn.execute(
            "INSERT INTO jobs(run_id, row_key, row_no, status, image_status, video_status, attempts_count, last_error)
             VALUES (?1, 'photo.png', 1, 'loi_tao_image', 'loi', 'chua_chay', 3, 'Old error')",
            rusqlite::params![run_id],
        ).unwrap();

        // CSV has the row with full key
        prompts_csv::write_all(
            &dir.join("prompts.csv"),
            &[Row {
                row_no: 1,
                anh_tham_chieu: "photo.png".into(),
                promt_tao_image: "prompt img".into(),
                promt_tao_video: "prompt vid".into(),
                trang_thai: "Lỗi tạo image".into(),
                ghi_chu_loi: "Old error".into(),
                ..Default::default()
            }],
        )
        .unwrap();

        let full_key = crate::prompts_csv::row_key_of("photo.png", "prompt img", "prompt vid");
        let detail = crate::commands::reset_row_status_at(&dir, &full_key).expect("reset should succeed");

        assert_eq!(detail.row.status, "Đang chờ");
        assert_eq!(detail.row.attempts_count, 0);

        // Verify SQLite was updated via legacy key fallback
        let job = latest_job_info(&conn, "photo.png").unwrap();
        assert_eq!(job.status, "dang_cho");
        assert_eq!(job.attempts_count, 0);
        assert!(job.last_error.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_p0_6_loi_tam_thoi_with_valid_image_plans_b3_and_inits_as_xong() {
        let dir = make_temp_project("loi_tam_thoi_init_img");
        let img_out = dir.join("image_out");
        std::fs::write(img_out.join("valid_gen.png"), b"image_data").unwrap();
        let cfg = Settings::default();
        let eng_cfg = EngineConfig {
            kind: "chung",
            row_keys: vec![],
            force_step: None,
            reset_error_attempts: false,
        };

        let row = Row {
            row_no: 1,
            anh_tham_chieu: "ref.png".into(),
            promt_tao_image: "p_img".into(),
            promt_tao_video: "p_vid".into(),
            trang_thai: "Lỗi tạm thời".into(),
            anh_da_tao: "valid_gen.png".into(),
            video_da_tao: "".into(),
            ..Default::default()
        };

        let plan = plan_row(&row, None, &cfg, &eng_cfg, &img_out, Some("valid_gen.png"), 1)
            .expect("must plan B3 for LoiTamThoi with valid image");
        assert_eq!(plan.steps, vec![Step::B3]);
        assert!(!plan.force_new_image);

        let (init_img_status, init_db_status) =
            initial_job_image_state(&row, row.status(), &plan, &img_out);
        assert_eq!(init_img_status, "xong");
        assert_eq!(init_db_status, "da_tao_image");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_find_previous_scene_video_found_and_missing() {
        let dir = make_temp_project("p2_1_find_prev");
        let csv_path = dir.join("prompts.csv");
        let video_out = dir.join("video_out");
        std::fs::create_dir_all(&video_out).unwrap();

        let conn = crate::state_db::open(&dir).unwrap();

        // 3 rows: row 1 (done), row 2 (noi_canh = 1), row 3 (noi_canh = 1)
        let row1 = Row {
            row_no: 2,
            anh_tham_chieu: "scene1.png".into(),
            promt_tao_image: "img1".into(),
            promt_tao_video: "vid1".into(),
            anh_da_tao: "scene1_gen.png".into(),
            video_da_tao: "scene1_v1.mp4".into(),
            trang_thai: "Đã tạo video".into(),
            noi_canh: false,
            ..Default::default()
        };
        let row2 = Row {
            row_no: 3,
            anh_tham_chieu: "scene2.png".into(),
            promt_tao_image: "img2".into(),
            promt_tao_video: "vid2".into(),
            anh_da_tao: "scene2_gen.png".into(),
            video_da_tao: "".into(),
            trang_thai: "Đang chờ".into(),
            noi_canh: true,
            ..Default::default()
        };
        let row3 = Row {
            row_no: 4,
            anh_tham_chieu: "scene3.png".into(),
            promt_tao_image: "img3".into(),
            promt_tao_video: "vid3".into(),
            anh_da_tao: "scene3_gen.png".into(),
            video_da_tao: "".into(),
            trang_thai: "Đang chờ".into(),
            noi_canh: true,
            ..Default::default()
        };
        prompts_csv::write_all(&csv_path, &[row1.clone(), row2.clone(), row3.clone()]).unwrap();

        // Create the video file for row 1 in video_out/
        let row1_vid = video_out.join("scene1_v1.mp4");
        std::fs::write(&row1_vid, b"fake-mp4-data").unwrap();

        // 1. row 1 is the first row -> has no previous scene
        let prev_for_row1 = find_previous_scene_video_impl(&csv_path, &video_out, &conn, 1, 1, &row1);
        assert!(prev_for_row1.is_none(), "first row should have no previous scene video");

        // 2. row 2 has row 1 preceding it with finished video -> finds scene1_v1.mp4
        let prev_for_row2 = find_previous_scene_video_impl(&csv_path, &video_out, &conn, 1, 2, &row2);
        assert_eq!(prev_for_row2, Some(row1_vid.clone()));

        // 3. row 3 has row 2 preceding it, but row 2 has NO video -> returns None (fail-safe)
        let prev_for_row3 = find_previous_scene_video_impl(&csv_path, &video_out, &conn, 1, 3, &row3);
        assert!(prev_for_row3.is_none(), "preceding row without video should return None");

        // 4. If row 1's video file is deleted from disk -> row 2 returns None
        std::fs::remove_file(&row1_vid).unwrap();
        let prev_after_delete = find_previous_scene_video_impl(&csv_path, &video_out, &conn, 1, 2, &row2);
        assert!(prev_after_delete.is_none(), "missing video on disk should return None");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_extract_last_frame_ffmpeg_error_handling() {
        let dir = make_temp_project("p2_1_ffmpeg");
        let non_existent_video = dir.join("not_found.mp4");
        let out_frame = dir.join(".cache").join("last_frame.png");

        // Non-existent source video must return Err
        let res = extract_last_frame_ffmpeg(&non_existent_video, &out_frame);
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("không tồn tại"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_auto_merge_collect_video_paths_order_and_filtering() {
        let dir = make_temp_project("p2_2_collect");
        let csv_path = dir.join("prompts.csv");
        let video_out = dir.join("video_out");
        std::fs::create_dir_all(&video_out).unwrap();

        let row1 = prompts_csv::Row {
            anh_tham_chieu: "scene1.png".into(),
            video_da_tao: "scene1_v1.mp4".into(),
            ..Default::default()
        };
        let row2 = prompts_csv::Row {
            anh_tham_chieu: "scene2.png".into(),
            video_da_tao: "".into(), // Chưa có video
            ..Default::default()
        };
        let row3 = prompts_csv::Row {
            anh_tham_chieu: "scene3.png".into(),
            video_da_tao: "scene3_v1.mp4".into(),
            ..Default::default()
        };
        let row4 = prompts_csv::Row {
            anh_tham_chieu: "scene4.png".into(),
            video_da_tao: "scene4_missing.mp4".into(), // Có tên file nhưng file không tồn tại trên đĩa
            ..Default::default()
        };
        prompts_csv::write_all(&csv_path, &[row1.clone(), row2.clone(), row3.clone(), row4.clone()]).unwrap();

        // Tạo file thực tế cho row1 và row3
        let vid1 = video_out.join("scene1_v1.mp4");
        let vid3 = video_out.join("scene3_v1.mp4");
        std::fs::write(&vid1, b"mp4-content-1").unwrap();
        std::fs::write(&vid3, b"mp4-content-3").unwrap();

        // Thu thập toàn bộ: phải đúng thứ tự CSV và chỉ chứa các file thực sự tồn tại
        let collected = collect_batch_video_paths(&csv_path, &video_out).expect("collect videos");
        assert_eq!(collected.len(), 2, "chỉ 2 video tồn tại thực tế được thu thập");
        assert_eq!(collected[0], vid1, "video dòng 1 phải đứng đầu tiên");
        assert_eq!(collected[1], vid3, "video dòng 3 phải đứng thứ hai");

        // Thu thập có lọc filter_keys
        let mut filter = std::collections::HashSet::new();
        filter.insert("scene1.png".to_string());
        let filtered = collect_batch_video_paths_filtered(&csv_path, &video_out, Some(&filter)).expect("filtered collect");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0], vid1);

        // Lỗi khi csv không tồn tại
        let non_csv = dir.join("non_existent.csv");
        assert!(collect_batch_video_paths(&non_csv, &video_out).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_auto_merge_command_formulation_and_silent_audio() {
        let input = Path::new("test_in.mp4");
        let output = Path::new("test_out.mp4");

        // 1. Trường hợp thiếu audio: phải có anullsrc và cờ -shortest
        let args_no_audio = build_normalize_clip_args(input, output, false, 720, 1280, 30);
        let joined_no_audio = args_no_audio.join(" ");
        assert!(joined_no_audio.contains("anullsrc=channel_layout=stereo:sample_rate=44100"), "phải bù anullsrc khi thiếu audio");
        assert!(joined_no_audio.contains("-shortest"), "phải có -shortest khi bù anullsrc");
        assert!(joined_no_audio.contains("scale=720:1280:force_original_aspect_ratio=decrease,pad=720:1280"));
        assert!(joined_no_audio.contains("fps=30"));
        assert!(joined_no_audio.contains("-c:a aac"));

        // 2. Trường hợp đã có audio: KHÔNG được chứa anullsrc và KHÔNG chứa -shortest
        let args_has_audio = build_normalize_clip_args(input, output, true, 1080, 1920, 24);
        let joined_has_audio = args_has_audio.join(" ");
        assert!(!joined_has_audio.contains("anullsrc"), "không bù anullsrc khi đã có audio");
        assert!(!joined_has_audio.contains("-shortest"), "không có -shortest khi không dùng infinite filter source");
        assert!(joined_has_audio.contains("scale=1080:1920"));
        assert!(joined_has_audio.contains("fps=24"));

        // 3. Concat demuxer command formulation
        let list_file = Path::new("list.txt");
        let concat_args = build_concat_demuxer_args(list_file, output);
        assert_eq!(concat_args[0], "-y");
        assert_eq!(concat_args[1], "-hide_banner");
        assert_eq!(concat_args[2], "-f");
        assert_eq!(concat_args[3], "concat");
        assert_eq!(concat_args[4], "-safe");
        assert_eq!(concat_args[5], "0");
        assert_eq!(concat_args[6], "-i");
        assert_eq!(concat_args[7], "list.txt");
        assert_eq!(concat_args[8], "-c");
        assert_eq!(concat_args[9], "copy");

        // 4. Filter complex concat command formulation
        let inps = vec![PathBuf::from("clip0.mp4"), PathBuf::from("clip1.mp4")];
        let has_a = vec![true, false];
        let fc_args = build_filter_complex_concat_args(&inps, output, &has_a, 720, 1280, 30);
        let joined_fc = fc_args.join(" ");
        assert!(joined_fc.contains("-filter_complex"));
        assert!(joined_fc.contains("concat=n=2:v=1:a=1"));
        assert!(joined_fc.contains("[outv]"));
        assert!(joined_fc.contains("[outa]"));
    }

    #[test]
    fn test_auto_merge_ntfs_filename_sanitization_and_output_naming() {
        // Sanitize forbidden NTFS characters
        let dirty = "Project: <Special> \"Name\" / Path \\ With | Illegal * Chars ?";
        let clean = sanitize_ntfs_filename(dirty);
        assert!(!clean.contains(':'));
        assert!(!clean.contains('<'));
        assert!(!clean.contains('>'));
        assert!(!clean.contains('"'));
        assert!(!clean.contains('/'));
        assert!(!clean.contains('\\'));
        assert!(!clean.contains('|'));
        assert!(!clean.contains('*'));
        assert!(!clean.contains('?'));
        assert_eq!(clean, "Project_ _Special_ _Name_ _ Path _ With _ Illegal _ Chars _");

        // Output naming with valid project name
        let name1 = generate_merged_output_filename("MyStory", "20261120_120000");
        assert_eq!(name1, "MyStory_merged_20261120_120000.mp4");

        // Output naming with empty or whitespace project name
        let name2 = generate_merged_output_filename("", "20261120_120000");
        assert_eq!(name2, "merged_20261120_120000.mp4");

        let name3 = generate_merged_output_filename("   ", "20261120_120000");
        assert_eq!(name3, "merged_20261120_120000.mp4");

        // Dimensions parser
        assert_eq!(parse_video_dimensions("720x1280"), (720, 1280));
        assert_eq!(parse_video_dimensions("1080x1920"), (1080, 1920));
        assert_eq!(parse_video_dimensions("1280:720"), (1280, 720));
        assert_eq!(parse_video_dimensions("invalid"), (720, 1280));
        assert_eq!(parse_video_dimensions(""), (720, 1280));
    }

    #[test]
    fn test_auto_merge_batch_fail_safe_and_clip_preservation() {
        let dir = make_temp_project("p2_2_failsafe");
        let csv_path = dir.join("prompts.csv");
        let video_out = dir.join("video_out");
        std::fs::create_dir_all(&video_out).unwrap();

        let row1 = prompts_csv::Row {
            anh_tham_chieu: "scene1.png".into(),
            video_da_tao: "clip1.mp4".into(),
            ..Default::default()
        };
        let row2 = prompts_csv::Row {
            anh_tham_chieu: "scene2.png".into(),
            video_da_tao: "clip2.mp4".into(),
            ..Default::default()
        };
        prompts_csv::write_all(&csv_path, &[row1.clone(), row2.clone()]).unwrap();

        let vid1 = video_out.join("clip1.mp4");
        let vid2 = video_out.join("clip2.mp4");
        std::fs::write(&vid1, b"original-content-1").unwrap();
        std::fs::write(&vid2, b"original-content-2").unwrap();

        let cfg = Settings {
            auto_merge: true,
            video_size: "720x1280".into(),
            ..Default::default()
        };

        // Khi các file video không phải là stream MP4 hợp lệ, auto_merge_batch bắt buộc trả về Err,
        // nhưng toàn bộ các clip gốc trong video_out/ phải hoàn toàn nguyên vẹn!
        let merge_res = auto_merge_batch(&dir, Some(1), None, &cfg);
        assert!(
            merge_res.is_err(),
            "auto_merge_batch phải báo lỗi khi clip giả không phải MP4 hợp lệ"
        );
        // Xác nhận clips lẻ vẫn nguyên vẹn 100%
        assert!(vid1.is_file(), "clip1 phải luôn được bảo toàn");
        assert!(vid2.is_file(), "clip2 phải luôn được bảo toàn");
        assert_eq!(std::fs::read(&vid1).unwrap(), b"original-content-1");
        assert_eq!(std::fs::read(&vid2).unwrap(), b"original-content-2");

        // Kiểm tra trường hợp chỉ có 1 video thành phẩm -> auto_merge_batch trả về Ok(None) bỏ qua an toàn
        let dir_single = make_temp_project("p2_2_single");
        let csv_single = dir_single.join("prompts.csv");
        let vout_single = dir_single.join("video_out");
        std::fs::create_dir_all(&vout_single).unwrap();
        prompts_csv::write_all(&csv_single, &[row1]).unwrap();
        std::fs::write(vout_single.join("clip1.mp4"), b"single-clip").unwrap();

        let single_res = auto_merge_batch(&dir_single, Some(1), None, &cfg);
        assert_eq!(single_res, Ok(None), "ít hơn 2 video phải trả về Ok(None)");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir_single);
    }

    #[test]
    fn test_auto_merge_probe_has_audio_error_on_missing_file() {
        let missing = Path::new("non_existent_video_file.mp4");
        let res = probe_has_audio_ffmpeg(missing);
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("không tồn tại"));
    }

    #[test]
    fn test_p0_2_rerun_row_step_isolation_for_multi_rows_sharing_same_image() {
        let dir = make_temp_project("p0_2_row_isolation");
        let img_out = dir.join("image_out");
        let cfg = Settings::default();

        // 2 dòng trong cùng CSV có cùng ảnh tham chiếu `anh_tham_chieu` nhưng prompt khác nhau
        let row1 = Row {
            row_no: 1,
            anh_tham_chieu: "shared_product.png".into(),
            promt_tao_image: "Prompt image 1 for shared product".into(),
            promt_tao_video: "Prompt video 1 for shared product".into(),
            trang_thai: "Đã tạo video".into(),
            anh_da_tao: "shared_product_gen1.png".into(),
            video_da_tao: "shared_product_gen1.mp4".into(),
            ..Default::default()
        };

        let row2 = Row {
            row_no: 2,
            anh_tham_chieu: "shared_product.png".into(),
            promt_tao_image: "Prompt image 2 for shared product (variant B)".into(),
            promt_tao_video: "Prompt video 2 for shared product (variant B)".into(),
            trang_thai: "Đã tạo video".into(),
            anh_da_tao: "shared_product_gen2.png".into(),
            video_da_tao: "shared_product_gen2.mp4".into(),
            ..Default::default()
        };

        let key1 = prompts_csv::row_key_of(
            &row1.anh_tham_chieu,
            &row1.promt_tao_image,
            &row1.promt_tao_video,
        );
        let key2 = prompts_csv::row_key_of(
            &row2.anh_tham_chieu,
            &row2.promt_tao_image,
            &row2.promt_tao_video,
        );

        assert_ne!(key1, key2, "Các dòng có prompt khác nhau phải có row_key phân biệt");
        assert!(key1.starts_with("shared_product.png#"));
        assert!(key2.starts_with("shared_product.png#"));

        // 1. Khi same_image = 2 (nhiều dòng dùng chung ảnh), truyền tên ảnh trần (`row.anh_tham_chieu`)
        // KHÔNG còn khớp nhầm sang các dòng anh em (triệt tiêu lỗi chạy chùm).
        let cfg_bare_image = EngineConfig {
            kind: "lam_lai",
            row_keys: vec![row1.anh_tham_chieu.clone()],
            force_step: Some(Step::B3),
            reset_error_attempts: false,
        };
        assert!(
            plan_row(&row1, None, &cfg, &cfg_bare_image, &img_out, None, 2).is_none(),
            "same_image=2 must block bare image matching on row 1"
        );
        assert!(
            plan_row(&row2, None, &cfg, &cfg_bare_image, &img_out, None, 2).is_none(),
            "same_image=2 must block bare image matching on row 2"
        );

        // 2. Kiểm chứng sau khi sửa: rerun_row_step truyền `key_of_row` (chứa hash)
        // Khi chạy lại dòng 1 (Step::B3):
        let cfg_isolated_b3 = EngineConfig {
            kind: "lam_lai",
            row_keys: vec![key1.clone()],
            force_step: Some(Step::B3),
            reset_error_attempts: false,
        };

        let plan1_b3 = plan_row(&row1, None, &cfg, &cfg_isolated_b3, &img_out, None, 2);
        assert!(plan1_b3.is_some(), "Dòng 1 được chọn chính xác");
        let p1 = plan1_b3.unwrap();
        assert_eq!(p1.steps, vec![Step::B3]);
        assert!(!p1.force_new_image);
        assert!(p1.force_new_video);

        let plan2_b3 = plan_row(&row2, None, &cfg, &cfg_isolated_b3, &img_out, None, 2);
        assert!(
            plan2_b3.is_none(),
            "Dòng 2 KHÔNG được chạy khi người dùng bấm chạy lại dòng 1 (đã triệt tiêu lỗi chạy chùm!)"
        );

        // Khi chạy lại dòng 1 (Step::B2):
        let cfg_isolated_b2 = EngineConfig {
            kind: "lam_lai",
            row_keys: vec![key1.clone()],
            force_step: Some(Step::B2),
            reset_error_attempts: false,
        };

        let plan1_b2 = plan_row(&row1, None, &cfg, &cfg_isolated_b2, &img_out, None, 2);
        assert!(plan1_b2.is_some(), "Dòng 1 được chọn chạy B2+B3");
        let p1_b2 = plan1_b2.unwrap();
        assert_eq!(p1_b2.steps, vec![Step::B2, Step::B3]);
        assert!(p1_b2.force_new_image);
        assert!(p1_b2.force_new_video);

        let plan2_b2 = plan_row(&row2, None, &cfg, &cfg_isolated_b2, &img_out, None, 2);
        assert!(
            plan2_b2.is_none(),
            "Dòng 2 KHÔNG được chạy khi người dùng bấm chạy lại B2 dòng 1"
        );

        // 3. Khi chạy lại dòng 2 với `key2`:
        let cfg_target_row2 = EngineConfig {
            kind: "lam_lai",
            row_keys: vec![key2.clone()],
            force_step: Some(Step::B3),
            reset_error_attempts: false,
        };

        assert!(
            plan_row(&row1, None, &cfg, &cfg_target_row2, &img_out, None, 2).is_none(),
            "Dòng 1 KHÔNG được chạy khi mục tiêu là dòng 2"
        );
        let plan2 = plan_row(&row2, None, &cfg, &cfg_target_row2, &img_out, None, 2);
        assert!(plan2.is_some(), "Dòng 2 được chọn chính xác");
        assert_eq!(plan2.unwrap().steps, vec![Step::B3]);

        // 4. Lập kế hoạch duyệt toàn bộ CSV rows chỉ chọn đúng 1 dòng mục tiêu
        let all_rows = [row1.clone(), row2.clone()];
        let planned_for_key1: Vec<_> = all_rows
            .iter()
            .filter_map(|r| plan_row(r, None, &cfg, &cfg_isolated_b3, &img_out, None, 2))
            .collect();
        assert_eq!(
            planned_for_key1.len(),
            1,
            "Chỉ duy nhất 1 dòng được lên lịch thực thi khi rerun_row_step dùng key_of_row"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_next_version_escapes_sqlite_like_wildcards() {
        let dir = make_temp_project("next_ver_like_escape");
        let conn = crate::state_db::open(&dir).unwrap();
        let img_out = dir.join("image_out");

        conn.execute(
            "INSERT INTO runs(kind, status, started_at, total_rows) VALUES ('chung', 'hoan_tat', '2026-11-18T00:00:00Z', 2)",
            [],
        )
        .unwrap();
        let run_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO jobs(run_id, row_key, status) VALUES (?1, 'other.png', 'da_tao_image')",
            rusqlite::params![run_id],
        )
        .unwrap();
        let j1 = conn.last_insert_rowid();

        // Các file có ký tự bình thường khớp wildcard '_' / '%' nếu không escape:
        // 'a_1' không escape sẽ khớp 'ab1_gen_v5.png'; '100%_sale' không escape sẽ khớp '100x_sale_gen_v9.png'.
        conn.execute(
            "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
             VALUES (?1, 'ab1.png', 'anh', 'ab1_gen_v5.png', 'image_out/ab1_gen_v5.png', 5, 1, '2026-11-18T00:01:00Z')",
            rusqlite::params![j1],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
             VALUES (?1, '100x_sale.png', 'anh', '100x_sale_gen_v9.png', 'image_out/100x_sale_gen_v9.png', 9, 1, '2026-11-18T00:02:00Z')",
            rusqlite::params![j1],
        )
        .unwrap();

        assert_eq!(next_version(&conn, &img_out, "a_1.png", "anh"), 1);
        assert_eq!(next_version(&conn, &img_out, "100%_sale.png", "anh"), 1);

        // Nếu thực sự có 'a_1_gen.png' (version 1) trong DB, next_version cho 'a_1.png' trả về 2
        conn.execute(
            "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
             VALUES (?1, 'a_1.png', 'anh', 'a_1_gen.png', 'image_out/a_1_gen.png', 1, 1, '2026-11-18T00:03:00Z')",
            rusqlite::params![j1],
        )
        .unwrap();
        assert_eq!(next_version(&conn, &img_out, "a_1.png", "anh"), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_reserve_next_version_concurrent_same_stem_no_collision() {
        let dir = make_temp_project("reserve_concurrent");
        let img_out = dir.join("image_out");
        let _init_conn = crate::state_db::open(&dir).unwrap();

        let workers = 4usize;
        let barrier = Arc::new(std::sync::Barrier::new(workers));
        let reserved_versions = Arc::new(parking_lot::Mutex::new(Vec::<i64>::new()));

        std::thread::scope(|scope| {
            for idx in 0..workers {
                let dir_ref = &dir;
                let img_ref = &img_out;
                let barrier_ref = barrier.clone();
                let vers_ref = reserved_versions.clone();
                scope.spawn(move || {
                    let conn = crate::state_db::open(dir_ref).unwrap();
                    let rk = format!("shared_stem.png#h{idx}");
                    barrier_ref.wait();
                    let res = reserve_next_version(&conn, img_ref, &rk, "anh");
                    vers_ref.lock().push(res.version);
                    // Giữ reservation qua giai đoạn I/O mô phỏng trước khi ghi file & commit DB
                    std::thread::sleep(Duration::from_millis(25));
                    std::fs::write(&res.dest, format!("img-{idx}").as_bytes()).unwrap();
                    conn.execute(
                        "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
                         VALUES (NULL, ?1, 'anh', ?2, ?3, ?4, 1, '2026-11-18T00:00:00Z')",
                        rusqlite::params![rk, res.file_name, format!("image_out/{}", res.file_name), res.version],
                    )
                    .unwrap();
                    drop(res);
                });
            }
        });

        let mut got = reserved_versions.lock().clone();
        got.sort_unstable();
        assert_eq!(got, vec![1, 2, 3, 4], "concurrent workers sharing same stem must get distinct versions");

        // Kiểm tra Drop của VersionReservation nhả reservation khi không ghi đĩa/DB (nhánh lỗi)
        let conn = crate::state_db::open(&dir).unwrap();
        let aborted = reserve_next_version(&conn, &img_out, "shared_stem.png#abort", "anh");
        assert_eq!(aborted.version, 5);
        drop(aborted);
        let retried = reserve_next_version(&conn, &img_out, "shared_stem.png#retry", "anh");
        assert_eq!(retried.version, 5, "dropped reservation must release version 5 for reuse");
        drop(retried);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_e2e_transient_retries_not_starved_when_carried_attempts_at_max() {
        let dir = make_temp_project("e2e_transient_not_starved");
        std::fs::write(dir.join("input").join("cup.png"), sample_png_bytes(15, 25, 35)).unwrap();

        prompts_csv::write_all(
            &dir.join("prompts.csv"),
            &[Row {
                row_no: 2,
                anh_tham_chieu: "cup.png".into(),
                promt_tao_image: "cup img prompt".into(),
                promt_tao_video: "cup vid prompt".into(),
                trang_thai: "Đang chờ".into(),
                ..Default::default()
            }],
        )
        .unwrap();

        // Pre-seed job với carried_attempts = 3 == max_attempts = 3
        let cup_key = crate::prompts_csv::row_key_of("cup.png", "cup img prompt", "cup vid prompt");
        {
            let conn = crate::state_db::open(&dir).unwrap();
            conn.execute(
                "INSERT INTO runs(kind, status, started_at, total_rows) VALUES ('chung', 'hoan_tat', '2026-11-18T00:00:00Z', 1)",
                [],
            )
            .unwrap();
            let rid = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO jobs(run_id, row_key, row_no, status, image_status, video_status, image_prompt_hash, video_prompt_hash, attempts_count)
                 VALUES (?1, ?2, 2, 'loi_tao_image', 'loi', 'chua_chay', ?3, ?4, 3)",
                rusqlite::params![rid, cup_key, sha2hex("cup img prompt"), sha2hex("cup vid prompt")],
            )
            .unwrap();
        }

        let (port_b, stop_b, join_b) = spawn_mock_http_server(move |_req| {
            RawHttpResp::binary(200, "OK", "video/mp4", sample_mp4_bytes(b"CUPV"))
        });

        let img_calls = Arc::new(AtomicUsize::new(0));
        let vid_calls = Arc::new(AtomicUsize::new(0));
        let img_calls_ref = img_calls.clone();
        let vid_calls_ref = vid_calls.clone();

        let (port_a, stop_a, join_a) = spawn_mock_http_server(move |req| {
            if req.method == "POST" && req.path == "/v1/images/edits" {
                let n = img_calls_ref.fetch_add(1, Ordering::SeqCst) + 1;
                if n <= 2 {
                    return RawHttpResp::json(502, "Bad Gateway", serde_json::json!({ "error": "transient b2" }));
                }
                return RawHttpResp::json(
                    200,
                    "OK",
                    serde_json::json!({ "data": [{ "b64_json": sample_jpeg_b64(10, 120, 220) }] }),
                );
            }
            if req.method == "POST" && req.path == "/v1/videos" {
                let n = vid_calls_ref.fetch_add(1, Ordering::SeqCst) + 1;
                if n <= 2 {
                    return RawHttpResp::json(502, "Bad Gateway", serde_json::json!({ "error": "transient b3" }));
                }
                return RawHttpResp::json(201, "Created", serde_json::json!({ "id": "task_cup_1" }));
            }
            if req.method == "GET" && req.path == "/v1/videos/task_cup_1" {
                return RawHttpResp::json(
                    200,
                    "OK",
                    serde_json::json!({
                        "status": "succeeded",
                        "video_url": format!("http://127.0.0.1:{port_b}/cup.mp4")
                    }),
                );
            }
            RawHttpResp::json(404, "Not Found", serde_json::json!({}))
        });

        let api_base = format!("http://127.0.0.1:{port_a}/v1");
        adapters::register_test_api_key(&api_base, "sk-transient-test");
        let cfg = Settings {
            endpoint_image_base_url: api_base.clone(),
            endpoint_video_base_url: api_base,
            max_attempts: 3,
            backoff_s: [1, 1, 1],
            poll_interval_s: 1,
            poll_max_s: 20,
            ..Settings::default()
        };
        let noop_emit = |_name: &str, _val: serde_json::Value| {};
        let stop_flag = Arc::new(AtomicBool::new(false));
        let eng_cfg = EngineConfig {
            kind: "chung",
            row_keys: Vec::new(),
            force_step: None,
            reset_error_attempts: false,
        };

        let out = run_with_emitter(&noop_emit, &cfg, &dir, eng_cfg, &stop_flag)
            .expect("transient retries must not be starved when carried_attempts == max_attempts");
        assert_eq!(out.done, 1);
        assert_eq!(out.errors, 0);
        assert_eq!(img_calls.load(Ordering::SeqCst), 3, "B2 must retry twice and succeed on 3rd try");
        assert_eq!(vid_calls.load(Ordering::SeqCst), 3, "B3 must retry twice and succeed on 3rd try");
        assert!(dir.join("image_out").join("cup_gen.png").is_file());
        assert!(dir.join("video_out").join("cup_gen.mp4").is_file());

        stop_a.store(true, Ordering::SeqCst);
        stop_b.store(true, Ordering::SeqCst);
        let _ = join_a.join();
        let _ = join_b.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_plan_row_legacy_key_same_image_and_empty_img_prompt_b3_resume() {
        let dir = make_temp_project("plan_row_m7");
        let img_out = dir.join("image_out");
        let cfg = Settings::default();

        let empty_img_row = Row {
            row_no: 2,
            anh_tham_chieu: "shared.png".into(),
            promt_tao_image: "   ".into(),
            promt_tao_video: "animate shared".into(),
            anh_da_tao: "".into(),
            trang_thai: "Lỗi tạm thời".into(),
            ..Default::default()
        };

        // 1. same_image == 2 chặn bare image key, same_image == 1 cho phép, và khớp theo prev.row_key luôn hoạt động
        let bare_cfg = EngineConfig {
            kind: "lam_lai",
            row_keys: vec!["shared.png".into()],
            force_step: None,
            reset_error_attempts: false,
        };
        assert!(plan_row(&empty_img_row, None, &cfg, &bare_cfg, &img_out, None, 2).is_none());
        assert!(plan_row(&empty_img_row, None, &cfg, &bare_cfg, &img_out, None, 1).is_some());

        let prev_resume = LatestJobInfo {
            id: 10,
            row_key: "shared.png#oldhash".into(),
            status: "loi_tam_thoi".into(),
            image_status: "xong".into(),
            video_status: "loi".into(),
            image_prompt_hash: Some(sha2hex("")),
            video_prompt_hash: Some(sha2hex("animate shared")),
            attempts_count: 1,
            last_error: Some("502".into()),
            video_task_id: Some("task_resume_99".into()),
            video_download_url: Some("https://cdn.example.com/resumed.mp4".into()),
        };

        let prev_key_cfg = EngineConfig {
            kind: "lam_lai",
            row_keys: vec!["shared.png#oldhash".into()],
            force_step: None,
            reset_error_attempts: false,
        };
        let plan_resume = plan_row(
            &empty_img_row,
            Some(&prev_resume),
            &cfg,
            &prev_key_cfg,
            &img_out,
            None,
            2,
        )
        .expect("empty promt_tao_image + LoiTamThoi + prev.row_key must plan B3 resume");
        assert_eq!(plan_resume.steps, vec![Step::B3]);
        assert_eq!(plan_resume.resume_task_id.as_deref(), Some("task_resume_99"));
        assert_eq!(
            plan_resume.resume_download_url.as_deref(),
            Some("https://cdn.example.com/resumed.mp4")
        );

        // 2. Khi vid_changed với promt_tao_image rỗng và anh_da_tao rỗng -> vẫn chỉ chạy B3 (không chạy B2!)
        let mut vid_edited_row = empty_img_row.clone();
        vid_edited_row.promt_tao_video = "new video prompt".into();
        let plan_vid_changed = plan_row(
            &vid_edited_row,
            Some(&prev_resume),
            &cfg,
            &EngineConfig::default(),
            &img_out,
            None,
            1,
        )
        .expect("vid_changed with empty promt_tao_image must plan B3");
        assert_eq!(plan_vid_changed.steps, vec![Step::B3]);
        assert!(plan_vid_changed.force_new_video);
        assert!(plan_vid_changed.resume_task_id.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_run_with_emitter_and_hook_defers_run_finished() {
        let dir = make_temp_project("defer_run_finished");
        std::fs::write(dir.join("input").join("solo.png"), sample_png_bytes(90, 80, 70)).unwrap();

        prompts_csv::write_all(
            &dir.join("prompts.csv"),
            &[Row {
                row_no: 2,
                anh_tham_chieu: "solo.png".into(),
                promt_tao_image: "".into(),
                promt_tao_video: "solo vid".into(),
                trang_thai: "Đã tạo image".into(),
                ..Default::default()
            }],
        )
        .unwrap();

        let (port_b, stop_b, join_b) = spawn_mock_http_server(move |_req| {
            RawHttpResp::binary(200, "OK", "video/mp4", sample_mp4_bytes(b"SOLO"))
        });
        let (port_a, stop_a, join_a) = spawn_mock_http_server(move |req| {
            if req.method == "POST" && req.path == "/v1/videos" {
                return RawHttpResp::json(201, "Created", serde_json::json!({ "id": "task_solo" }));
            }
            if req.method == "GET" && req.path == "/v1/videos/task_solo" {
                return RawHttpResp::json(
                    200,
                    "OK",
                    serde_json::json!({
                        "status": "succeeded",
                        "video_url": format!("http://127.0.0.1:{port_b}/solo.mp4")
                    }),
                );
            }
            RawHttpResp::json(404, "Not Found", serde_json::json!({}))
        });

        let api_base = format!("http://127.0.0.1:{port_a}/v1");
        adapters::register_test_api_key(&api_base, "sk-defer-test");
        let cfg = Settings {
            endpoint_image_base_url: api_base.clone(),
            endpoint_video_base_url: api_base,
            poll_interval_s: 1,
            poll_max_s: 20,
            ..Settings::default()
        };

        let events = Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
        let events_ref = events.clone();
        let emitter = move |name: &str, _val: serde_json::Value| {
            events_ref.lock().push(name.to_string());
        };
        let stop_flag = Arc::new(AtomicBool::new(false));

        let outcome = run_with_emitter_and_hook(
            &emitter,
            None,
            &cfg,
            &dir,
            EngineConfig::default(),
            &stop_flag,
        )
        .expect("run_with_emitter_and_hook must succeed");

        let hook_events = events.lock().clone();
        assert!(hook_events.iter().any(|e| e == "run_started"));
        assert!(
            !hook_events.iter().any(|e| e == "run_finished"),
            "run_with_emitter_and_hook MUST NOT emit run_finished before caller clears running flag"
        );
        assert_eq!(outcome.status, "hoan_tat");
        assert_eq!(outcome.done, 1);
        assert_eq!(outcome.errors, 0);
        assert!(!outcome.stopped);
        assert!(outcome.duration_ms >= 0);

        let payload = build_run_finished_payload(&outcome);
        assert_eq!(payload.get("run_id").and_then(|v| v.as_i64()), Some(outcome.run_id));
        assert_eq!(payload.get("status").and_then(|v| v.as_str()), Some("hoan_tat"));
        assert_eq!(payload.get("done_count").and_then(|v| v.as_u64()), Some(1));
        assert_eq!(payload.get("error_count").and_then(|v| v.as_u64()), Some(0));
        assert_eq!(
            payload.get("duration_ms").and_then(|v| v.as_i64()),
            Some(outcome.duration_ms)
        );

        stop_a.store(true, Ordering::SeqCst);
        stop_b.store(true, Ordering::SeqCst);
        let _ = join_a.join();
        let _ = join_b.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_plan_row_cho_empty_image_prompt_skips_to_b3() {
        let dir = make_temp_project("plan_row_cho_empty_img");
        let img_out = dir.join("image_out");
        let cfg = Settings::default();
        let row = Row {
            row_no: 1,
            anh_tham_chieu: "ref.png".into(),
            promt_tao_image: "   ".into(),
            promt_tao_video: "make it move".into(),
            anh_da_tao: "".into(),
            trang_thai: "Đang chờ".into(),
            ..Default::default()
        };
        let eng_cfg = EngineConfig {
            kind: "chung",
            row_keys: vec![],
            force_step: None,
            reset_error_attempts: false,
        };
        let plan = plan_row(&row, None, &cfg, &eng_cfg, &img_out, None, 1)
            .expect("Cho with empty image prompt must plan B3");
        assert_eq!(plan.steps, vec![Step::B3]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_run_b2_migrates_old_key_before_insert_and_preserves_single_current() {
        let dir = make_temp_project("run_b2_old_key_ver");
        let csv_path = dir.join("prompts.csv");
        let img_out = dir.join("image_out");
        let vid_out = dir.join("video_out");
        std::fs::write(dir.join("input").join("new_ref.png"), sample_png_bytes(10, 20, 30)).unwrap();
        std::fs::write(img_out.join("legacy_old_gen.png"), sample_png_bytes(1, 2, 3)).unwrap();

        let mut row = Row {
            row_no: 2,
            anh_tham_chieu: "new_ref.png".into(),
            promt_tao_image: "img prompt".into(),
            promt_tao_video: "vid prompt".into(),
            anh_da_tao: "legacy_old_gen.png".into(),
            trang_thai: "Đang chờ".into(),
            ..Default::default()
        };
        prompts_csv::write_all(&csv_path, std::slice::from_ref(&row)).unwrap();

        let old_row_key = "old_ref.png#abcd";
        let new_row_key = prompts_csv::row_key_of(&row.anh_tham_chieu, &row.promt_tao_image, &row.promt_tao_video);
        let conn = crate::state_db::open(&dir).unwrap();
        conn.execute(
            "INSERT INTO runs(kind, status, started_at, total_rows) VALUES ('chung', 'dang_chay', '2026-11-20T00:00:00Z', 1)",
            [],
        )
        .unwrap();
        let run_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO jobs(run_id, row_key, row_no, status, image_status, video_status)
             VALUES (?1, ?2, 2, 'da_tao_image', 'xong', 'chua_chay')",
            rusqlite::params![run_id, old_row_key],
        )
        .unwrap();
        let old_job_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
             VALUES (?1, ?2, 'anh', 'legacy_old_gen.png', 'image_out/legacy_old_gen.png', 1, 1, '2026-11-20T00:00:01Z')",
            rusqlite::params![old_job_id, old_row_key],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO jobs(run_id, row_key, row_no, status, image_status, video_status)
             VALUES (?1, ?2, 2, 'dang_cho', 'chua_chay', 'chua_chay')",
            rusqlite::params![run_id, new_row_key],
        )
        .unwrap();
        let new_job_id = conn.last_insert_rowid();

        let (port, stop_srv, join_srv) = spawn_mock_http_server(move |req| {
            if req.method == "POST" && req.path == "/v1/images/edits" {
                return RawHttpResp::json(
                    200,
                    "OK",
                    serde_json::json!({ "data": [{ "b64_json": sample_jpeg_b64(50, 60, 70) }] }),
                );
            }
            RawHttpResp::json(404, "Not Found", serde_json::json!({}))
        });

        let api_base = format!("http://127.0.0.1:{port}/v1");
        adapters::register_test_api_key(&api_base, "sk-b2-old-key");
        let cfg = Settings {
            endpoint_image_base_url: api_base.clone(),
            endpoint_video_base_url: api_base,
            ..Settings::default()
        };
        let noop = |_name: &str, _val: serde_json::Value| {};
        let stop_flag = AtomicBool::new(false);
        let done_count = AtomicUsize::new(0);
        let active_rows = parking_lot::Mutex::new(Vec::new());
        let ctx = WorkerRowCtx {
            emitter: &noop,
            cfg: &cfg,
            conn: &conn,
            csv_path: &csv_path,
            image_out: &img_out,
            video_out: &vid_out,
            stop_flag: &stop_flag,
            run_id,
            job_id: new_job_id,
            total: 1,
            done_count: &done_count,
            active_rows: &active_rows,
        };

        let status = run_b2(&ctx, &mut row, true).expect("run_b2 should succeed");
        assert_eq!(status, RowStatus::DaTaoImage);
        assert_eq!(row.anh_da_tao, "new_ref_gen_v2.png");

        let refs: Vec<(String, String, i64, i64)> = {
            let mut stmt = conn
                .prepare("SELECT row_key, file_name, version, is_current FROM file_refs WHERE kind = 'anh' ORDER BY version ASC")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .unwrap()
                .map(|x| x.unwrap())
                .collect()
        };
        assert_eq!(
            refs,
            vec![
                (new_row_key.clone(), "legacy_old_gen.png".to_string(), 1, 0),
                (new_row_key.clone(), "new_ref_gen_v2.png".to_string(), 2, 1),
            ]
        );

        stop_srv.store(true, Ordering::SeqCst);
        let _ = join_srv.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_download_and_commit_video_migrates_old_key_before_insert_and_preserves_single_current() {
        let dir = make_temp_project("dl_vid_old_key_ver");
        let csv_path = dir.join("prompts.csv");
        let img_out = dir.join("image_out");
        let vid_out = dir.join("video_out");
        std::fs::write(img_out.join("old_clip_gen.png"), sample_png_bytes(1, 2, 3)).unwrap();
        std::fs::write(vid_out.join("old_clip_gen.mp4"), sample_mp4_bytes(b"OLD1")).unwrap();

        let mut row = Row {
            row_no: 2,
            anh_tham_chieu: "new_clip.png".into(),
            promt_tao_image: "".into(),
            promt_tao_video: "animate clip".into(),
            anh_da_tao: "old_clip_gen.png".into(),
            video_da_tao: "old_clip_gen.mp4".into(),
            trang_thai: "Đã tạo image".into(),
            ..Default::default()
        };
        prompts_csv::write_all(&csv_path, std::slice::from_ref(&row)).unwrap();

        let old_row_key = "old_clip.png#1234";
        let new_row_key = prompts_csv::row_key_of(&row.anh_tham_chieu, &row.promt_tao_image, &row.promt_tao_video);
        let conn = crate::state_db::open(&dir).unwrap();
        conn.execute(
            "INSERT INTO runs(kind, status, started_at, total_rows) VALUES ('chung', 'dang_chay', '2026-11-20T00:00:00Z', 1)",
            [],
        )
        .unwrap();
        let run_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO jobs(run_id, row_key, row_no, status, image_status, video_status)
             VALUES (?1, ?2, 2, 'da_tao_video', 'xong', 'xong')",
            rusqlite::params![run_id, old_row_key],
        )
        .unwrap();
        let old_job_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
             VALUES (?1, ?2, 'anh', 'old_clip_gen.png', 'image_out/old_clip_gen.png', 1, 1, '2026-11-20T00:00:01Z')",
            rusqlite::params![old_job_id, old_row_key],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
             VALUES (?1, ?2, 'video', 'old_clip_gen.mp4', 'video_out/old_clip_gen.mp4', 1, 1, '2026-11-20T00:00:02Z')",
            rusqlite::params![old_job_id, old_row_key],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO jobs(run_id, row_key, row_no, status, image_status, video_status)
             VALUES (?1, ?2, 2, 'da_tao_image', 'xong', 'dang_chay')",
            rusqlite::params![run_id, new_row_key],
        )
        .unwrap();
        let new_job_id = conn.last_insert_rowid();

        let (port, stop_srv, join_srv) = spawn_mock_http_server(move |_req| {
            RawHttpResp::binary(200, "OK", "video/mp4", sample_mp4_bytes(b"NEW2"))
        });

        let cfg = Settings {
            endpoint_video_base_url: format!("http://127.0.0.1:{port}/v1"),
            ..Settings::default()
        };
        let noop = |_name: &str, _val: serde_json::Value| {};
        let stop_flag = AtomicBool::new(false);
        let done_count = AtomicUsize::new(0);
        let active_rows = parking_lot::Mutex::new(Vec::new());
        let ctx = WorkerRowCtx {
            emitter: &noop,
            cfg: &cfg,
            conn: &conn,
            csv_path: &csv_path,
            image_out: &img_out,
            video_out: &vid_out,
            stop_flag: &stop_flag,
            run_id,
            job_id: new_job_id,
            total: 1,
            done_count: &done_count,
            active_rows: &active_rows,
        };

        let url = format!("http://127.0.0.1:{port}/out.mp4");
        download_and_commit_video(&ctx, &mut row, &url, "task_v2")
            .expect("download_and_commit_video should succeed");
        assert_eq!(row.video_da_tao, "new_clip_gen_v2.mp4");

        let video_refs: Vec<(String, String, i64, i64)> = {
            let mut stmt = conn
                .prepare("SELECT row_key, file_name, version, is_current FROM file_refs WHERE kind = 'video' ORDER BY version ASC")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .unwrap()
                .map(|x| x.unwrap())
                .collect()
        };
        assert_eq!(
            video_refs,
            vec![
                (new_row_key.clone(), "old_clip_gen.mp4".to_string(), 1, 0),
                (new_row_key.clone(), "new_clip_gen_v2.mp4".to_string(), 2, 1),
            ]
        );

        let img_ref: (String, i64, i64) = conn
            .query_row(
                "SELECT row_key, version, is_current FROM file_refs WHERE kind = 'anh'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(img_ref, (new_row_key, 1, 1));

        stop_srv.store(true, Ordering::SeqCst);
        let _ = join_srv.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_reserve_next_version_with_keys_accounts_for_colliding_old_key_bump() {
        let dir = make_temp_project("reserve_colliding_old_key");
        let img_out = dir.join("image_out");
        let conn = crate::state_db::open(&dir).unwrap();
        let target_key = "cur.png#h2";
        let old_key = "old.png#h1";

        conn.execute(
            "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
             VALUES (NULL, ?1, 'anh', 'cur_a.png', 'image_out/cur_a.png', 1, 1, '2026-11-20T00:00:00Z')",
            rusqlite::params![target_key],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
             VALUES (NULL, ?1, 'anh', 'old_b.png', 'image_out/old_b.png', 1, 1, '2026-11-20T00:00:01Z')",
            rusqlite::params![old_key],
        )
        .unwrap();

        let res =
            reserve_next_version_with_keys(&conn, &img_out, target_key, &[old_key.into()], "anh");
        assert_eq!(
            res.version, 3,
            "colliding v1 from old_key will bump to v2 during migration, so new ref must reserve v3"
        );
        drop(res);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_compute_pipeline_workers_matrix() {
        // Concurrency = 1, gateway -> 1 image + 1 video chạy song song độc lập
        let mut cfg = Settings {
            concurrency: 1,
            ..Settings::default()
        };
        assert_eq!(compute_pipeline_workers(&cfg, 5, 5, false, None), (1, 1));
        assert_eq!(compute_pipeline_workers(&cfg, 5, 5, true, None), (1, 1));

        // Concurrency = 2 -> 2 image + 2 video; khi có noi_canh -> 2 image + 1 video
        cfg.concurrency = 2;
        assert_eq!(compute_pipeline_workers(&cfg, 5, 5, false, None), (2, 2));
        assert_eq!(compute_pipeline_workers(&cfg, 5, 5, true, None), (2, 1));

        // Chỉ có B3 (b2_count = 0) -> không spawn worker ảnh
        assert_eq!(compute_pipeline_workers(&cfg, 0, 5, false, None), (0, 2));
        // Chỉ có B2 (b3_count = 0) -> không spawn worker video
        assert_eq!(compute_pipeline_workers(&cfg, 5, 0, false, None), (2, 0));

        // Nguồn video là Muse nội bộ: số luồng TUYỆT ĐỐI <= số tài khoản Muse khả dụng
        // để bảo đảm 1 tài khoản chỉ chạy tối đa 1 luồng video duy nhất và khớp hiển thị UI!
        cfg.concurrency = 5;
        cfg.image_source = "internal".into();
        cfg.video_source = "internal".into();
        // 1 tài khoản -> chặn cứng về (1 image, 1 video)
        assert_eq!(compute_pipeline_workers(&cfg, 10, 10, false, Some(1)), (1, 1));
        assert_eq!(compute_pipeline_workers(&cfg, 10, 10, false, None), (1, 1));
        assert_eq!(compute_pipeline_workers(&cfg, 10, 10, false, Some(0)), (1, 1));
        // 2 tài khoản -> (2 image, 2 video) (mỗi TK 1 video)
        assert_eq!(compute_pipeline_workers(&cfg, 10, 10, false, Some(2)), (2, 2));
        // 5 tài khoản -> (5 image, 5 video) (mỗi TK 1 video)
        assert_eq!(compute_pipeline_workers(&cfg, 10, 10, false, Some(5)), (5, 5));
        // 5 tài khoản + noi_canh -> video ép về 1 theo thứ tự CSV; image vẫn chạy song song 5
        assert_eq!(compute_pipeline_workers(&cfg, 10, 10, true, Some(5)), (5, 1));

        // Nguồn hỗn hợp (gateway ảnh + internal video): số luồng bị chặn <= số tài khoản Muse
        cfg.image_source = "gateway".into();
        cfg.video_source = "internal".into();
        assert_eq!(compute_pipeline_workers(&cfg, 10, 10, false, Some(1)), (1, 1));
        assert_eq!(compute_pipeline_workers(&cfg, 10, 10, false, Some(3)), (3, 3));
        assert_eq!(compute_pipeline_workers(&cfg, 10, 10, false, Some(6)), (5, 5));

        // Nguồn hỗn hợp (internal ảnh + gateway video): ảnh vẫn bị chặn trong ngưỡng 4 slot / TK Muse
        cfg.image_source = "internal".into();
        cfg.video_source = "gateway".into();
        cfg.concurrency = 10;
        assert_eq!(compute_pipeline_workers(&cfg, 10, 10, false, Some(1)), (4, 10));
        assert_eq!(compute_pipeline_workers(&cfg, 10, 0, false, Some(1)), (4, 0));
        assert_eq!(compute_pipeline_workers(&cfg, 10, 10, false, Some(3)), (10, 10));

        // Gateway ngoài cho cả ảnh và video -> chạy được > 3 luồng (tối đa MAX_CONCURRENCY)
        cfg.image_source = "gateway".into();
        cfg.video_source = "gateway".into();
        cfg.concurrency = 6;
        assert_eq!(compute_pipeline_workers(&cfg, 10, 10, false, Some(1)), (6, 6));
    }

    #[test]
    fn test_e2e_decoupled_image_pipeline_runs_parallel_with_video_even_at_concurrency_1() {
        let dir = make_temp_project("e2e_decoupled_c1");
        for (name, rgb) in [
            ("r1.png", (10, 20, 30)),
            ("r2.png", (40, 50, 60)),
            ("r3.png", (70, 80, 90)),
        ] {
            std::fs::write(dir.join("input").join(name), sample_png_bytes(rgb.0, rgb.1, rgb.2))
                .unwrap();
        }

        prompts_csv::write_all(
            &dir.join("prompts.csv"),
            &[
                Row {
                    row_no: 2,
                    anh_tham_chieu: "r1.png".into(),
                    promt_tao_image: "img 1".into(),
                    promt_tao_video: "vid 1".into(),
                    trang_thai: "Đang chờ".into(),
                    ..Default::default()
                },
                Row {
                    row_no: 3,
                    anh_tham_chieu: "r2.png".into(),
                    promt_tao_image: "img 2".into(),
                    promt_tao_video: "vid 2".into(),
                    trang_thai: "Đang chờ".into(),
                    ..Default::default()
                },
                Row {
                    row_no: 4,
                    anh_tham_chieu: "r3.png".into(),
                    promt_tao_image: "img 3".into(),
                    promt_tao_video: "vid 3".into(),
                    trang_thai: "Đang chờ".into(),
                    ..Default::default()
                },
            ],
        )
        .unwrap();

        let (port_b, stop_b, join_b) = spawn_mock_http_server(move |_req| {
            RawHttpResp::binary(200, "OK", "video/mp4", sample_mp4_bytes(b"DECP"))
        });

        // Cổng đồng bộ chứng minh: video dòng 1 CHỈ hoàn tất sau khi CẢ 3 ảnh (r1, r2, r3) đã tạo xong!
        // Nếu B2 phải đợi B3 như kiến trúc cũ, r2 & r3 sẽ không bao giờ chạy trước khi video 1 xong.
        let images_completed = Arc::new(AtomicUsize::new(0));
        let all_images_seen_during_v1 = Arc::new(AtomicBool::new(false));
        let vid_seq = Arc::new(AtomicUsize::new(0));

        let img_cnt_ref = images_completed.clone();
        let seen_ref = all_images_seen_during_v1.clone();
        let vid_seq_ref = vid_seq.clone();

        let (port_a, stop_a, join_a) = spawn_mock_http_server(move |req| {
            if req.method == "POST" && req.path == "/v1/images/edits" {
                std::thread::sleep(Duration::from_millis(60));
                img_cnt_ref.fetch_add(1, Ordering::SeqCst);
                return RawHttpResp::json(
                    200,
                    "OK",
                    serde_json::json!({ "data": [{ "b64_json": sample_jpeg_b64(30, 120, 210) }] }),
                );
            }
            if req.method == "POST" && req.path == "/v1/videos" {
                let n = vid_seq_ref.fetch_add(1, Ordering::SeqCst) + 1;
                return RawHttpResp::json(
                    201,
                    "Created",
                    serde_json::json!({ "id": format!("task_dec_{n}") }),
                );
            }
            if req.method == "GET" && req.path.starts_with("/v1/videos/task_dec_") {
                if req.path == "/v1/videos/task_dec_1" {
                    let deadline = Instant::now() + Duration::from_secs(6);
                    while img_cnt_ref.load(Ordering::SeqCst) < 3 && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    if img_cnt_ref.load(Ordering::SeqCst) >= 3 {
                        seen_ref.store(true, Ordering::SeqCst);
                    }
                }
                return RawHttpResp::json(
                    200,
                    "OK",
                    serde_json::json!({
                        "status": "succeeded",
                        "progress": 100,
                        "video_url": format!("http://127.0.0.1:{port_b}/dec.mp4")
                    }),
                );
            }
            RawHttpResp::json(404, "Not Found", serde_json::json!({}))
        });

        let api_base = format!("http://127.0.0.1:{port_a}/v1");
        adapters::register_test_api_key(&api_base, "sk-decoupled-test");
        let cfg = Settings {
            endpoint_image_base_url: api_base.clone(),
            endpoint_video_base_url: api_base,
            concurrency: 1, // Ngay cả khi concurrency=1, ảnh & video vẫn chạy song song 2 tầng!
            poll_interval_s: 1,
            poll_max_s: 30,
            backoff_s: [1, 1, 1],
            ..Settings::default()
        };

        let saw_b2_and_b3_concurrent = Arc::new(AtomicBool::new(false));
        let conc_ref = saw_b2_and_b3_concurrent.clone();
        let emitter = move |name: &str, val: serde_json::Value| {
            if name == "progress" {
                if let Some(arr) = val.get("active_rows").and_then(|v| v.as_array()) {
                    let has_b2 = arr
                        .iter()
                        .any(|r| r.get("step").and_then(|s| s.as_str()) == Some("B2"));
                    let has_b3 = arr
                        .iter()
                        .any(|r| r.get("step").and_then(|s| s.as_str()) == Some("B3"));
                    if has_b2 && has_b3 {
                        conc_ref.store(true, Ordering::SeqCst);
                    }
                }
            }
        };
        let stop_flag = Arc::new(AtomicBool::new(false));

        let out = run_with_emitter(&emitter, &cfg, &dir, EngineConfig::default(), &stop_flag)
            .expect("decoupled concurrency=1 run must succeed");

        assert_eq!(out.done, 3);
        assert_eq!(out.errors, 0);
        assert!(
            all_images_seen_during_v1.load(Ordering::SeqCst),
            "r2 & r3 images MUST complete while r1 video is still in progress"
        );
        assert!(
            saw_b2_and_b3_concurrent.load(Ordering::SeqCst),
            "progress events must observe B2 and B3 running concurrently"
        );
        for stem in ["r1", "r2", "r3"] {
            assert!(dir.join("image_out").join(format!("{stem}_gen.png")).is_file());
            assert!(dir.join("video_out").join(format!("{stem}_gen.mp4")).is_file());
        }

        stop_a.store(true, Ordering::SeqCst);
        stop_b.store(true, Ordering::SeqCst);
        let _ = join_a.join();
        let _ = join_b.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_e2e_decoupled_pipeline_noi_canh_preserves_video_order_while_images_run_ahead() {
        let dir = make_temp_project("e2e_decoupled_noi_canh");
        for (name, rgb) in [
            ("s1.png", (10, 20, 30)),
            ("s2.png", (40, 50, 60)),
            ("s3.png", (70, 80, 90)),
        ] {
            std::fs::write(dir.join("input").join(name), sample_png_bytes(rgb.0, rgb.1, rgb.2))
                .unwrap();
        }

        prompts_csv::write_all(
            &dir.join("prompts.csv"),
            &[
                Row {
                    row_no: 2,
                    anh_tham_chieu: "s1.png".into(),
                    promt_tao_image: "scene 1 img".into(),
                    promt_tao_video: "scene 1 vid".into(),
                    trang_thai: "Đang chờ".into(),
                    noi_canh: false,
                    ..Default::default()
                },
                Row {
                    row_no: 3,
                    anh_tham_chieu: "s2.png".into(),
                    promt_tao_image: "scene 2 img".into(),
                    promt_tao_video: "scene 2 vid".into(),
                    trang_thai: "Đang chờ".into(),
                    noi_canh: true,
                    ..Default::default()
                },
                Row {
                    row_no: 4,
                    anh_tham_chieu: "s3.png".into(),
                    promt_tao_image: "scene 3 img".into(),
                    promt_tao_video: "scene 3 vid".into(),
                    trang_thai: "Đang chờ".into(),
                    noi_canh: true,
                    ..Default::default()
                },
            ],
        )
        .unwrap();

        let downloads_completed = Arc::new(AtomicUsize::new(0));
        let dl_ref = downloads_completed.clone();
        let (port_b, stop_b, join_b) = spawn_mock_http_server(move |_req| {
            dl_ref.fetch_add(1, Ordering::SeqCst);
            RawHttpResp::binary(200, "OK", "video/mp4", sample_mp4_bytes(b"NOIC"))
        });

        let video_create_prompts: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let video_start_prev_downloads: Arc<parking_lot::Mutex<Vec<usize>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let images_done = Arc::new(AtomicUsize::new(0));
        let images_done_before_s1_video_finish = Arc::new(AtomicUsize::new(0));

        let vprompts_ref = video_create_prompts.clone();
        let vprev_dl_ref = video_start_prev_downloads.clone();
        let dl_check_ref = downloads_completed.clone();
        let img_done_ref = images_done.clone();
        let img_before_s1_ref = images_done_before_s1_video_finish.clone();

        let (port_a, stop_a, join_a) = spawn_mock_http_server(move |req| {
            if req.method == "POST" && req.path == "/v1/images/edits" {
                let body_str = String::from_utf8_lossy(&req.body);
                // Cố tình làm chậm B2 của scene 1 hơn scene 2 để kiểm chứng hàng đợi video
                // vẫn giữ đúng thứ tự scene 1 -> scene 2 -> scene 3 (không để scene 2 nhảy trước!).
                if body_str.contains("scene 1 img") {
                    std::thread::sleep(Duration::from_millis(250));
                } else {
                    std::thread::sleep(Duration::from_millis(30));
                }
                img_done_ref.fetch_add(1, Ordering::SeqCst);
                return RawHttpResp::json(
                    200,
                    "OK",
                    serde_json::json!({ "data": [{ "b64_json": sample_jpeg_b64(90, 150, 210) }] }),
                );
            }
            if req.method == "POST" && req.path == "/v1/videos" {
                let parsed: serde_json::Value =
                    serde_json::from_slice(&req.body).unwrap_or_default();
                let prompt = parsed
                    .get("prompt")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let idx = {
                    let mut vp = vprompts_ref.lock();
                    vp.push(prompt.clone());
                    vp.len()
                };
                vprev_dl_ref.lock().push(dl_check_ref.load(Ordering::SeqCst));
                return RawHttpResp::json(
                    201,
                    "Created",
                    serde_json::json!({ "id": format!("task_nc_{idx}") }),
                );
            }
            if req.method == "GET" && req.path.starts_with("/v1/videos/task_nc_") {
                if req.path == "/v1/videos/task_nc_1" {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while img_done_ref.load(Ordering::SeqCst) < 3 && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    img_before_s1_ref.store(img_done_ref.load(Ordering::SeqCst), Ordering::SeqCst);
                }
                return RawHttpResp::json(
                    200,
                    "OK",
                    serde_json::json!({
                        "status": "succeeded",
                        "progress": 100,
                        "video_url": format!("http://127.0.0.1:{port_b}/nc.mp4")
                    }),
                );
            }
            RawHttpResp::json(404, "Not Found", serde_json::json!({}))
        });

        let api_base = format!("http://127.0.0.1:{port_a}/v1");
        adapters::register_test_api_key(&api_base, "sk-noicanh-test");
        let cfg = Settings {
            endpoint_image_base_url: api_base.clone(),
            endpoint_video_base_url: api_base,
            concurrency: 2,
            poll_interval_s: 1,
            poll_max_s: 30,
            backoff_s: [1, 1, 1],
            ..Settings::default()
        };
        let noop_emit = |_name: &str, _val: serde_json::Value| {};
        let stop_flag = Arc::new(AtomicBool::new(false));

        let out = run_with_emitter(&noop_emit, &cfg, &dir, EngineConfig::default(), &stop_flag)
            .expect("noi_canh decoupled run must succeed");
        assert_eq!(out.done, 3);
        assert_eq!(out.errors, 0);
        assert_eq!(
            images_done_before_s1_video_finish.load(Ordering::SeqCst),
            3,
            "all 3 images must finish ahead while scene 1 video is rendering"
        );
        assert_eq!(
            *video_create_prompts.lock(),
            vec![
                "scene 1 vid".to_string(),
                "scene 2 vid".to_string(),
                "scene 3 vid".to_string(),
            ],
            "noi_canh videos MUST run in strict CSV order even when scene 2 image finished before scene 1 image"
        );
        assert_eq!(
            *video_start_prev_downloads.lock(),
            vec![0, 1, 2],
            "each noi_canh video MUST wait for the previous scene's video download to complete"
        );

        stop_a.store(true, Ordering::SeqCst);
        stop_b.store(true, Ordering::SeqCst);
        let _ = join_a.join();
        let _ = join_b.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_e2e_decoupled_pipeline_b2_failure_does_not_block_or_deadlock_other_rows() {
        for ordered_noi_canh in [false, true] {
            let dir = make_temp_project(&format!("e2e_dec_b2_fail_{ordered_noi_canh}"));
            for (name, rgb) in [
                ("f1.png", (10, 20, 30)),
                ("f2.png", (40, 50, 60)),
                ("f3.png", (70, 80, 90)),
            ] {
                std::fs::write(
                    dir.join("input").join(name),
                    sample_png_bytes(rgb.0, rgb.1, rgb.2),
                )
                .unwrap();
            }

            prompts_csv::write_all(
                &dir.join("prompts.csv"),
                &[
                    Row {
                        row_no: 2,
                        anh_tham_chieu: "f1.png".into(),
                        promt_tao_image: "bad prompt".into(),
                        promt_tao_video: "vid 1".into(),
                        trang_thai: "Đang chờ".into(),
                        noi_canh: false,
                        ..Default::default()
                    },
                    Row {
                        row_no: 3,
                        anh_tham_chieu: "f2.png".into(),
                        promt_tao_image: "good prompt 2".into(),
                        promt_tao_video: "vid 2".into(),
                        trang_thai: "Đang chờ".into(),
                        noi_canh: ordered_noi_canh,
                        ..Default::default()
                    },
                    Row {
                        row_no: 4,
                        anh_tham_chieu: "f3.png".into(),
                        promt_tao_image: "good prompt 3".into(),
                        promt_tao_video: "vid 3".into(),
                        trang_thai: "Đang chờ".into(),
                        noi_canh: ordered_noi_canh,
                        ..Default::default()
                    },
                ],
            )
            .unwrap();

            let (port_b, stop_b, join_b) = spawn_mock_http_server(move |_req| {
                RawHttpResp::binary(200, "OK", "video/mp4", sample_mp4_bytes(b"FAIL"))
            });

            let vid_seq = Arc::new(AtomicUsize::new(0));
            let vid_seq_ref = vid_seq.clone();
            let (port_a, stop_a, join_a) = spawn_mock_http_server(move |req| {
                if req.method == "POST" && req.path == "/v1/images/edits" {
                    let body_str = String::from_utf8_lossy(&req.body);
                    if body_str.contains("bad prompt") {
                        return RawHttpResp::json(
                            400,
                            "Bad Request",
                            serde_json::json!({ "error": "rejected prompt" }),
                        );
                    }
                    return RawHttpResp::json(
                        200,
                        "OK",
                        serde_json::json!({ "data": [{ "b64_json": sample_jpeg_b64(10, 90, 180) }] }),
                    );
                }
                if req.method == "POST" && req.path == "/v1/videos" {
                    let n = vid_seq_ref.fetch_add(1, Ordering::SeqCst) + 1;
                    return RawHttpResp::json(
                        201,
                        "Created",
                        serde_json::json!({ "id": format!("task_f_{n}") }),
                    );
                }
                if req.method == "GET" && req.path.starts_with("/v1/videos/task_f_") {
                    return RawHttpResp::json(
                        200,
                        "OK",
                        serde_json::json!({
                            "status": "succeeded",
                            "progress": 100,
                            "video_url": format!("http://127.0.0.1:{port_b}/f.mp4")
                        }),
                    );
                }
                RawHttpResp::json(404, "Not Found", serde_json::json!({}))
            });

            let api_base = format!("http://127.0.0.1:{port_a}/v1");
            adapters::register_test_api_key(&api_base, "sk-b2fail-test");
            let cfg = Settings {
                endpoint_image_base_url: api_base.clone(),
                endpoint_video_base_url: api_base,
                concurrency: 2,
                poll_interval_s: 1,
                poll_max_s: 30,
                backoff_s: [1, 1, 1],
                ..Settings::default()
            };
            let noop_emit = |_name: &str, _val: serde_json::Value| {};
            let stop_flag = Arc::new(AtomicBool::new(false));

            let out = run_with_emitter(&noop_emit, &cfg, &dir, EngineConfig::default(), &stop_flag)
                .expect("run must complete cleanly when row 1 B2 fails");
            assert_eq!(out.status, "hoan_tat");
            assert_eq!(out.done, 2);
            assert_eq!(out.errors, 1);
            assert_eq!(vid_seq.load(Ordering::SeqCst), 2, "only 2 rows with valid images should call B3");
            assert!(!dir.join("video_out").join("f1_gen.mp4").exists());
            assert!(dir.join("video_out").join("f2_gen.mp4").is_file());
            assert!(dir.join("video_out").join("f3_gen.mp4").is_file());

            let conn = crate::state_db::open(&dir).unwrap();
            let f1_key = crate::prompts_csv::row_key_of("f1.png", "bad prompt", "vid 1");
            assert_eq!(
                latest_job_info(&conn, &f1_key).unwrap().status,
                "loi_tao_image"
            );

            stop_a.store(true, Ordering::SeqCst);
            stop_b.store(true, Ordering::SeqCst);
            let _ = join_a.join();
            let _ = join_b.join();
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn test_e2e_decoupled_pipeline_stop_mid_video_preserves_completed_ahead_images() {
        let dir = make_temp_project("e2e_dec_stop_resume");
        for (name, rgb) in [
            ("p1.png", (10, 20, 30)),
            ("p2.png", (40, 50, 60)),
            ("p3.png", (70, 80, 90)),
        ] {
            std::fs::write(dir.join("input").join(name), sample_png_bytes(rgb.0, rgb.1, rgb.2))
                .unwrap();
        }

        prompts_csv::write_all(
            &dir.join("prompts.csv"),
            &[
                Row {
                    row_no: 2,
                    anh_tham_chieu: "p1.png".into(),
                    promt_tao_image: "p1 img".into(),
                    promt_tao_video: "p1 vid".into(),
                    trang_thai: "Đang chờ".into(),
                    ..Default::default()
                },
                Row {
                    row_no: 3,
                    anh_tham_chieu: "p2.png".into(),
                    promt_tao_image: "p2 img".into(),
                    promt_tao_video: "p2 vid".into(),
                    trang_thai: "Đang chờ".into(),
                    ..Default::default()
                },
                Row {
                    row_no: 4,
                    anh_tham_chieu: "p3.png".into(),
                    promt_tao_image: "p3 img".into(),
                    promt_tao_video: "p3 vid".into(),
                    trang_thai: "Đang chờ".into(),
                    ..Default::default()
                },
            ],
        )
        .unwrap();

        let (port_b, stop_b, join_b) = spawn_mock_http_server(move |_req| {
            RawHttpResp::binary(200, "OK", "video/mp4", sample_mp4_bytes(b"STOP"))
        });

        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_on_first_poll = Arc::new(AtomicBool::new(true));
        let img_calls = Arc::new(AtomicUsize::new(0));
        let vid_create_calls = Arc::new(AtomicUsize::new(0));

        let stop_flag_srv = stop_flag.clone();
        let stop_poll_srv = stop_on_first_poll.clone();
        let img_calls_srv = img_calls.clone();
        let vid_calls_srv = vid_create_calls.clone();

        let (port_a, stop_a, join_a) = spawn_mock_http_server(move |req| {
            if req.method == "POST" && req.path == "/v1/images/edits" {
                img_calls_srv.fetch_add(1, Ordering::SeqCst);
                return RawHttpResp::json(
                    200,
                    "OK",
                    serde_json::json!({ "data": [{ "b64_json": sample_jpeg_b64(25, 115, 205) }] }),
                );
            }
            if req.method == "POST" && req.path == "/v1/videos" {
                let n = vid_calls_srv.fetch_add(1, Ordering::SeqCst) + 1;
                return RawHttpResp::json(
                    201,
                    "Created",
                    serde_json::json!({ "id": format!("task_sr_{n}") }),
                );
            }
            if req.method == "GET" && req.path.starts_with("/v1/videos/task_sr_") {
                if stop_poll_srv.load(Ordering::SeqCst) {
                    // Đợi đủ cả 3 ảnh hoàn tất rồi mới kích hoạt STOP ngay giữa lượt poll video 1
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while img_calls_srv.load(Ordering::SeqCst) < 3 && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    stop_flag_srv.store(true, Ordering::SeqCst);
                    return RawHttpResp::json(
                        200,
                        "OK",
                        serde_json::json!({ "status": "in_progress", "progress": 30 }),
                    );
                }
                return RawHttpResp::json(
                    200,
                    "OK",
                    serde_json::json!({
                        "status": "succeeded",
                        "progress": 100,
                        "video_url": format!("http://127.0.0.1:{port_b}/sr.mp4")
                    }),
                );
            }
            RawHttpResp::json(404, "Not Found", serde_json::json!({}))
        });

        let api_base = format!("http://127.0.0.1:{port_a}/v1");
        adapters::register_test_api_key(&api_base, "sk-stop-resume-test");
        let cfg = Settings {
            endpoint_image_base_url: api_base.clone(),
            endpoint_video_base_url: api_base,
            concurrency: 1,
            poll_interval_s: 1,
            poll_max_s: 30,
            backoff_s: [1, 1, 1],
            ..Settings::default()
        };
        let noop_emit = |_name: &str, _val: serde_json::Value| {};

        // Lượt 1: dừng an toàn giữa lúc poll video dòng 1, sau khi cả 3 ảnh đã tạo xong phía trước
        let out1 = run_with_emitter(&noop_emit, &cfg, &dir, EngineConfig::default(), &stop_flag)
            .expect("stopped run must succeed cleanly");
        assert!(out1.stopped);
        assert_eq!(out1.status, "da_dung");
        assert_eq!(out1.done, 0);
        assert_eq!(out1.errors, 0);
        assert_eq!(img_calls.load(Ordering::SeqCst), 3);
        assert_eq!(vid_create_calls.load(Ordering::SeqCst), 1);

        {
            let conn = crate::state_db::open(&dir).unwrap();
            for (img, ip, vp) in [
                ("p1.png", "p1 img", "p1 vid"),
                ("p2.png", "p2 img", "p2 vid"),
                ("p3.png", "p3 img", "p3 vid"),
            ] {
                let k = crate::prompts_csv::row_key_of(img, ip, vp);
                let job = latest_job_info(&conn, &k).expect("job must be persisted");
                assert_eq!(job.status, "da_tao_image");
                assert_eq!(job.image_status, "xong");
            }
        }

        // Lượt 2: tiếp tục chạy — KHÔNG gọi tạo ảnh lại lần nào, resume task_sr_1 + tạo 2 video còn lại
        stop_on_first_poll.store(false, Ordering::SeqCst);
        stop_flag.store(false, Ordering::SeqCst);
        let out2 = run_with_emitter(&noop_emit, &cfg, &dir, EngineConfig::default(), &stop_flag)
            .expect("resumed run must succeed");
        assert_eq!(out2.status, "hoan_tat");
        assert_eq!(out2.done, 3);
        assert_eq!(out2.errors, 0);
        assert_eq!(
            img_calls.load(Ordering::SeqCst),
            3,
            "resumed run MUST NOT re-generate already completed images"
        );
        assert_eq!(
            vid_create_calls.load(Ordering::SeqCst),
            3,
            "task_sr_1 must be resumed without recreating, plus 2 new tasks for p2 and p3"
        );

        stop_a.store(true, Ordering::SeqCst);
        stop_b.store(true, Ordering::SeqCst);
        let _ = join_a.join();
        let _ = join_b.join();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
