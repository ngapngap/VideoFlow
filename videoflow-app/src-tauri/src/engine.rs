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
use std::path::Path;
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
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, v)| v)
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
        Ok((
            r.get::<_, String>(1)?,
            LatestJobInfo {
                id: r.get(0)?,
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
        "SELECT id, status, image_status, video_status,
                image_prompt_hash, video_prompt_hash, attempts_count,
                last_error, video_task_id, video_download_url
         FROM jobs
         WHERE LOWER(row_key) = LOWER(?1)
         ORDER BY id DESC LIMIT 1",
        rusqlite::params![row_key],
        |r| {
            Ok(LatestJobInfo {
                id: r.get(0)?,
                status: r.get(1)?,
                image_status: r.get(2)?,
                video_status: r.get(3)?,
                image_prompt_hash: r.get(4)?,
                video_prompt_hash: r.get(5)?,
                attempts_count: r.get(6)?,
                last_error: r.get(7)?,
                video_task_id: r.get(8)?,
                video_download_url: r.get(9)?,
            })
        },
    )
    .ok()
}

pub fn start_steps(status: RowStatus) -> Vec<Step> {
    match status {
        RowStatus::Cho | RowStatus::LoiImage => vec![Step::B2, Step::B3],
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
    }
}

pub fn row_status_to_label(s: RowStatus) -> &'static str {
    match s {
        RowStatus::Cho => "Đang chờ",
        RowStatus::DaTaoImage => "Đã tạo image",
        RowStatus::DaTaoVideo => "Đã tạo video",
        RowStatus::LoiImage => "Lỗi tạo image",
        RowStatus::LoiVideo => "Lỗi tạo video",
    }
}

pub fn db_status_to_label(db_status: &str) -> &'static str {
    match db_status {
        "da_tao_image" => "Đã tạo image",
        "da_tao_video" => "Đã tạo video",
        "loi_tao_image" => "Lỗi tạo image",
        "loi_tao_video" => "Lỗi tạo video",
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

pub fn next_version(conn: &Connection, out_dir: &Path, row_key: &str, kind: &str) -> i64 {
    let db_max: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM file_refs WHERE LOWER(row_key) = LOWER(?1) AND kind = ?2",
            rusqlite::params![row_key, kind],
            |r| r.get(0),
        )
        .unwrap_or(0);

    let ext = if kind == "anh" { "png" } else { "mp4" };
    // row_key là "<tên ảnh>#<hash prompt>" → stem phải lấy từ tên ảnh, nếu không tên
    // file đầu ra sẽ dính hash và khác hoàn toàn quy ước `stem_gen.png`.
    let image_name = crate::prompts_csv::row_key_image_name(row_key);
    let stem = Path::new(image_name)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "anh".into());
    let stem_low = stem.to_lowercase();
    let v1_name = format!("{stem_low}_gen.{ext}");
    let vn_prefix = format!("{stem_low}_gen_v");
    let vn_suffix = format!(".{ext}");

    let mut disk_max: i64 = 0;
    if let Ok(entries) = std::fs::read_dir(out_dir) {
        for entry in entries.flatten() {
            let fname_low = entry.file_name().to_string_lossy().to_lowercase();
            if fname_low == v1_name {
                disk_max = disk_max.max(1);
            } else if let Some(mid) = fname_low
                .strip_prefix(&vn_prefix)
                .and_then(|s| s.strip_suffix(&vn_suffix))
            {
                if let Ok(v) = mid.parse::<i64>() {
                    disk_max = disk_max.max(v);
                }
            }
        }
    }

    let mut candidate = db_max.max(disk_max) + 1;
    while out_dir
        .join(versioned_file_name(&stem, candidate, ext))
        .exists()
    {
        candidate += 1;
    }
    candidate
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
    std::fs::write(&tmp, bytes)
        .map_err(|e| AppError::new("WRITE_FAIL", format!("Không ghi được file tạm: {e}")))?;
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
    cfg: &Settings,
    engine_cfg: &EngineConfig,
    image_out: &Path,
    latest_current_image_in_db: Option<&str>,
) -> Option<RowPlan> {
    if row.is_example() || row.is_empty_content() || row.anh_tham_chieu.trim().is_empty() {
        return None;
    }

    // `row_keys` đến từ UI = khoá dòng (tên ảnh#hash prompt); vẫn nhận cả dạng legacy
    // (chỉ tên ảnh) để lời gọi cũ và CLI không bị bỏ qua im lặng.
    if !engine_cfg.row_keys.is_empty() {
        let current = row_key_low(row);
        let image = row.anh_tham_chieu.trim();
        let matched = engine_cfg.row_keys.iter().any(|k| {
            let k = k.trim();
            k.eq_ignore_ascii_case(&current) || k.eq_ignore_ascii_case(image)
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
    let prompt_changed = img_changed || vid_changed;
    let prev_attempts = prev.map(|p| p.attempts_count).unwrap_or(0);

    // Đủ số lần thử lỗi tối đa và prompt chưa đổi -> khóa an toàn (trừ khi rerun_error_rows đặt reset_error_attempts)
    if !engine_cfg.reset_error_attempts
        && !prompt_changed
        && prev_attempts >= cfg.max_attempts as i64
    {
        return None;
    }

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
            RowStatus::DaTaoImage | RowStatus::DaTaoVideo | RowStatus::LoiVideo
        ) && !row.anh_da_tao.is_empty()
            && image_out.join(&row.anh_da_tao).is_file();

        return if has_valid_img {
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

    let steps = start_steps(base_status);
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
                None => !row.anh_da_tao.is_empty(),
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
        // Fallback dữ liệu cũ: jobs/file_refs lưu theo tên ảnh.
        let legacy_key = r.anh_tham_chieu.trim().to_lowercase();
        let legacy_only = image_counts.get(&legacy_key).copied().unwrap_or(0) == 1;
        let mut image_lookup_key = key_low.clone();
        if !media_map.contains_key(&key_low) && legacy_only && !legacy_key.is_empty() {
            image_lookup_key = legacy_key.clone();
        }
        let mut changed = false;

        if let Some(m) = media_map.get(&image_lookup_key) {
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

        // Ở đây `rows` đang bị mượn mut → tự quyết định lookup key bằng image_counts
        // (giống quy tắc job_for_row) thay vì gọi hàm cần &[Row].
        let job_lookup_key = if latest_map.contains_key(&key_low) {
            key_low.clone()
        } else if legacy_only && !legacy_key.is_empty() {
            legacy_key.clone()
        } else {
            key_low.clone()
        };
        if let Some(j) = latest_map.get(&job_lookup_key) {
            let csv_label = match j.status.as_str() {
                "dang_cho" | "bo_qua" => Some("Đang chờ"),
                "da_tao_image" => Some("Đã tạo image"),
                "da_tao_video" => Some("Đã tạo video"),
                "loi_tao_image" => Some("Lỗi tạo image"),
                "loi_tao_video" => Some("Lỗi tạo video"),
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

pub struct EngineOutcome {
    pub run_id: i64,
    pub done: usize,
    pub errors: usize,
    pub stopped: bool,
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
    run_with_emitter_and_hook(&cb, None, cfg, project_path, engine_cfg, stop_flag)
}

pub fn run_with_emitter(
    emitter: EventEmitter<'_>,
    cfg: &Settings,
    project_path: &Path,
    engine_cfg: EngineConfig,
    stop_flag: &Arc<AtomicBool>,
) -> AppResult<EngineOutcome> {
    run_with_emitter_and_hook(emitter, None, cfg, project_path, engine_cfg, stop_flag)
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
        let prev = job_for_row(&latest_map, &rows, r);
        // media_map dùng chung khoá với jobs; fallback tên ảnh khi ảnh duy nhất.
        let db_img = map_lookup_legacy(&media_map, &rows, r)
            .and_then(|m| m.image_file.as_deref());
        if let Some(plan) = plan_row(r, prev, cfg, &engine_cfg, &image_out, db_img) {
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

    let worker_count = (cfg.concurrency.clamp(1, 3) as usize).min(total.max(1));
    let queue = parking_lot::Mutex::new(planned_rows);
    let active_rows: parking_lot::Mutex<Vec<ActiveRowProgress>> = parking_lot::Mutex::new(Vec::new());
    let done_count = AtomicUsize::new(0);
    let error_count = AtomicUsize::new(0);
    let stopped = AtomicBool::new(false);
    let fatal_err: parking_lot::Mutex<Option<AppError>> = parking_lot::Mutex::new(None);
    let run_start = Instant::now();

    std::thread::scope(|scope| {
        for _worker_idx in 0..worker_count {
            let queue_ref = &queue;
            let active_ref = &active_rows;
            let done_ref = &done_count;
            let err_ref = &error_count;
            let stopped_ref = &stopped;
            let fatal_ref = &fatal_err;
            let csv_ref = &csv_path;
            let img_ref = &image_out;
            let vid_ref = &video_out;
            let stop_ref = stop_flag.as_ref();

            scope.spawn(move || {
                let w_conn = match crate::state_db::open(project_path) {
                    Ok(c) => c,
                    Err(e) => {
                        *fatal_ref.lock() =
                            Some(AppError::new("DB_FAIL", format!("Worker không mở được state.db: {e}")));
                        stopped_ref.store(true, Ordering::SeqCst);
                        return;
                    }
                };

                loop {
                    if stop_ref.load(Ordering::SeqCst) || stopped_ref.load(Ordering::SeqCst) {
                        stopped_ref.store(true, Ordering::SeqCst);
                        break;
                    }

                    let Some((mut row, plan)) = queue_ref.lock().pop_front() else {
                        break;
                    };

                    if stop_ref.load(Ordering::SeqCst) || stopped_ref.load(Ordering::SeqCst) {
                        stopped_ref.store(true, Ordering::SeqCst);
                        break;
                    }

                    let row_key = crate::prompts_csv::row_key_of(
                        &row.anh_tham_chieu,
                        &row.promt_tao_image,
                        &row.promt_tao_video,
                    );
                    let row_timer = Instant::now();

                    let prev_before_insert = latest_job_info(&w_conn, &row_key);
                    let base_st = effective_row_status(&row, prev_before_insert.as_ref())
                        .unwrap_or_else(|| row.status());
                    let init_img_status = if !plan.force_new_image
                        && matches!(
                            base_st,
                            RowStatus::DaTaoImage | RowStatus::DaTaoVideo | RowStatus::LoiVideo
                        )
                        && !row.anh_da_tao.is_empty()
                        && img_ref.join(&row.anh_da_tao).is_file()
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

                    // [P0-2]: Khởi tạo `image_prompt_hash` và `video_prompt_hash` bằng hash LỊCH SỬ (`prev_*_hash`),
                    // CHỈ cập nhật sang hash mới sau khi từng bước thực sự hoàn tất!
                    let ts_job = chrono::Local::now().to_rfc3339();
                    if let Err(e) = w_conn.execute(
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
                    ) {
                        crate::settings::log(
                            &w_conn,
                            "error",
                            Some(run_id),
                            Some(&row_key),
                            Some("B1"),
                            &format!("Không chèn được bản ghi job: {e}"),
                        );
                        err_ref.fetch_add(1, Ordering::SeqCst);
                        continue;
                    }
                    let job_id = w_conn.last_insert_rowid();

                    let ctx = WorkerRowCtx {
                        emitter,
                        cfg,
                        conn: &w_conn,
                        csv_path: csv_ref,
                        image_out: img_ref,
                        video_out: vid_ref,
                        stop_flag: stop_ref,
                        run_id,
                        job_id,
                        total,
                        done_count: done_ref,
                        active_rows: active_ref,
                    };

                    let mut row_incomplete = false;
                    let mut b2_completed_in_this_job = false;

                    for step in &plan.steps {
                        if stop_ref.load(Ordering::SeqCst) || stopped_ref.load(Ordering::SeqCst) {
                            let has_img =
                                !row.anh_da_tao.is_empty() && img_ref.join(&row.anh_da_tao).is_file();
                            let b2_needs_fresh =
                                *step == Step::B2 && (plan.force_new_image || !has_img);
                            let stop_fail = ApiFail::stopped("Đã dừng an toàn giữa các bước");
                            let res =
                                resolve_step_failure(*step, &stop_fail, has_img, b2_needs_fresh);
                            finish_job_status(
                                &ctx,
                                &mut row,
                                &res,
                                row_timer.elapsed().as_millis() as i64,
                            );
                            ctx.clear_active_row(&row_key);
                            emit(
                                emitter,
                                "row_changed",
                                &RowChangedEvent {
                                    row_key: row_key.clone(),
                                    status: res.csv_label.into(),
                                    image_status: res.image_status.into(),
                                    video_status: res.video_status.into(),
                                    attempts_count: current_job_attempts(&w_conn, job_id),
                                    last_error: None,
                                },
                            );
                            stopped_ref.store(true, Ordering::SeqCst);
                            row_incomplete = true;
                            break;
                        }

                        let step_name = if *step == Step::B2 { "B2" } else { "B3" };
                        let step_msg = if *step == Step::B2 {
                            "Đang tạo image".to_string()
                        } else {
                            "Đang tạo video".to_string()
                        };
                        ctx.emit_step_progress(&row_key, step_name, step_msg);

                        let result = match step {
                            Step::B2 => run_b2(&ctx, &mut row, plan.force_new_image),
                            Step::B3 => run_b3(&ctx, &mut row, plan.force_new_video),
                        };

                        match result {
                            Ok(next_status) => {
                                if *step == Step::B2 {
                                    b2_completed_in_this_job = true;
                                }
                                let (img_st, vid_st) = match next_status {
                                    RowStatus::DaTaoImage => ("xong", "chua_chay"),
                                    RowStatus::DaTaoVideo => ("xong", "xong"),
                                    _ => ("chua_chay", "chua_chay"),
                                };
                                let dur_ms = row_timer.elapsed().as_millis() as i64;
                                let ts_done = chrono::Local::now().to_rfc3339();
                                let _ = w_conn.execute(
                                    "UPDATE jobs SET finished_at = ?1, duration_ms = ?2 WHERE id = ?3",
                                    rusqlite::params![ts_done, dur_ms, job_id],
                                );
                                emit(
                                    emitter,
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
                            }
                            Err(fail) => {
                                row_incomplete = true;
                                let has_img = !row.anh_da_tao.is_empty()
                                    && img_ref.join(&row.anh_da_tao).is_file();
                                let b2_needs_fresh = *step == Step::B2
                                    && !b2_completed_in_this_job
                                    && (plan.force_new_image || !has_img);
                                let res =
                                    resolve_step_failure(*step, &fail, has_img, b2_needs_fresh);
                                finish_job_status(
                                    &ctx,
                                    &mut row,
                                    &res,
                                    row_timer.elapsed().as_millis() as i64,
                                );
                                ctx.clear_active_row(&row_key);

                                if res.count_as_run_error {
                                    err_ref.fetch_add(1, Ordering::SeqCst);
                                    crate::settings::log(
                                        &w_conn,
                                        "error",
                                        Some(run_id),
                                        Some(&row_key),
                                        Some(step_name),
                                        &fail.message,
                                    );
                                } else {
                                    let lvl =
                                        if fail.code == "STOPPED" { "info" } else { "warn" };
                                    crate::settings::log(
                                        &w_conn,
                                        lvl,
                                        Some(run_id),
                                        Some(&row_key),
                                        Some(step_name),
                                        &fail.message,
                                    );
                                }

                                emit(
                                    emitter,
                                    "row_changed",
                                    &RowChangedEvent {
                                        row_key: row_key.clone(),
                                        status: res.csv_label.into(),
                                        image_status: res.image_status.into(),
                                        video_status: res.video_status.into(),
                                        attempts_count: current_job_attempts(&w_conn, job_id),
                                        last_error: res.last_error.clone(),
                                    },
                                );

                                if fail.kind == ApiFailKind::HetQuota {
                                    emit(
                                        emitter,
                                        "run_error",
                                        &RunErrorEvent {
                                            run_id,
                                            row_key: Some(row_key.clone()),
                                            step: Some(step_name.into()),
                                            code: fail.code.clone(),
                                            message: fail.message.clone(),
                                        },
                                    );
                                    stop_ref.store(true, Ordering::SeqCst);
                                }

                                if res.trigger_stop {
                                    stopped_ref.store(true, Ordering::SeqCst);
                                }
                                break;
                            }
                        }
                    }

                    ctx.clear_active_row(&row_key);
                    if !row_incomplete && !stopped_ref.load(Ordering::SeqCst) {
                        done_ref.fetch_add(1, Ordering::SeqCst);
                    }

                    let d_now = done_ref.load(Ordering::SeqCst);
                    let e_now = err_ref.load(Ordering::SeqCst);
                    let _ = w_conn.execute(
                        "UPDATE runs SET done_count = ?1, error_count = ?2 WHERE id = ?3",
                        rusqlite::params![d_now as i64, e_now as i64, run_id],
                    );
                    ctx.emit_step_progress(&row_key, "done", format!("{d_now}/{total} dòng"));

                    if stopped_ref.load(Ordering::SeqCst) {
                        break;
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

    emit(
        emitter,
        "run_finished",
        &RunFinishedEvent {
            run_id,
            status: final_status.into(),
            done_count: final_done,
            error_count: final_errors,
            duration_ms: run_start.elapsed().as_millis() as i64,
        },
    );

    Ok(EngineOutcome {
        run_id,
        done: final_done,
        errors: final_errors,
        stopped: was_stopped,
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
                let total_attempts = record_attempt(
                    ctx.conn,
                    ctx.job_id,
                    AttemptRecord {
                        step: "B2",
                        fallback_attempt_no: local_try,
                        result: classify(&fail),
                        http_status: fail.http_status.map(|s| s as i64),
                        error_code: &fail.code,
                        error_message: &fail.message,
                        count_toward_limit: true,
                    },
                );
                match fail.kind {
                    ApiFailKind::NghiepVu | ApiFailKind::HetQuota => return Err(fail),
                    ApiFailKind::TamThoi { retry_after_s } => {
                        if total_attempts >= max_attempts {
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

    let stem = Path::new(&row_key)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "anh".into());
    let ver = next_version(ctx.conn, ctx.image_out, &row_key, "anh");
    let file_name = versioned_file_name(&stem, ver, "png");
    let dest = ctx.image_out.join(&file_name);
    write_atomic(&dest, &bytes).map_err(|e| ApiFail {
        kind: ApiFailKind::NghiepVu,
        http_status: None,
        code: e.code,
        message: e.message,
    })?;

    // [P2-1] + [P0-2]: Bọc cập nhật `file_refs` + `jobs` (bao gồm `image_prompt_hash` mới!) trong 1 transaction nguyên tử
    let rel = format!("image_out/{file_name}");
    let ts = chrono::Local::now().to_rfc3339();
    let tx = ctx.conn.unchecked_transaction().map_err(|e| ApiFail {
        kind: ApiFailKind::NghiepVu,
        http_status: None,
        code: "DB_TX_FAIL".into(),
        message: format!("Lỗi mở transaction lưu ảnh: {e}"),
    })?;
    tx.execute(
        "UPDATE file_refs SET is_current = 0 WHERE LOWER(row_key) = LOWER(?1) AND kind = 'anh'",
        rusqlite::params![row_key],
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
                let total_attempts = record_attempt(
                    ctx.conn,
                    ctx.job_id,
                    AttemptRecord {
                        step: "B3",
                        fallback_attempt_no: local_try,
                        result: classify(&fail),
                        http_status: fail.http_status.map(|s| s as i64),
                        error_code: &fail.code,
                        error_message: &fail.message,
                        count_toward_limit: true,
                    },
                );
                match fail.kind {
                    ApiFailKind::NghiepVu | ApiFailKind::HetQuota => return Err(fail),
                    ApiFailKind::TamThoi { retry_after_s } => {
                        if total_attempts >= max_attempts {
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

    let stem = Path::new(&row_key)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "anh".into());
    let ver = next_version(ctx.conn, ctx.video_out, &row_key, "video");
    let file_name = versioned_file_name(&stem, ver, "mp4");
    let dest = ctx.video_out.join(&file_name);

    let dl = adapters::download_video(ctx.cfg, url, &dest, task_id, ctx.stop_flag)?;

    // [P2-1] + [P1-1] + [P0-2]: Transaction nguyên tử lưu `file_refs` + `jobs`, cập nhật `video_prompt_hash` mới & xóa `attempts_count = 0`!
    let rel = format!("video_out/{file_name}");
    let ts = chrono::Local::now().to_rfc3339();
    let cur_vid_hash = sha2hex(row.promt_tao_video.trim());

    let tx = ctx.conn.unchecked_transaction().map_err(|e| ApiFail {
        kind: ApiFailKind::NghiepVu,
        http_status: None,
        code: "DB_TX_FAIL".into(),
        message: format!("Lỗi mở transaction lưu video: {e}"),
    })?;
    tx.execute(
        "UPDATE file_refs SET is_current = 0 WHERE LOWER(row_key) = LOWER(?1) AND kind = 'video'",
        rusqlite::params![row_key],
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

    if row.anh_da_tao.is_empty() {
        return Err(ApiFail {
            kind: ApiFailKind::NghiepVu,
            http_status: None,
            code: "NO_IMAGE".into(),
            message: "Chưa có ảnh sản phẩm - không thể tạo video (6.12)".into(),
        });
    }
    let img_path = ctx.image_out.join(&row.anh_da_tao);
    if !img_path.is_file() {
        return Err(ApiFail {
            kind: ApiFailKind::NghiepVu,
            http_status: None,
            code: "IMAGE_MISSING".into(),
            message: format!("Ảnh sản phẩm '{}' không còn trong image_out/", row.anh_da_tao),
        });
    }

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
                            count_toward_limit: true,
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
                let total_attempts = record_attempt(
                    ctx.conn,
                    ctx.job_id,
                    AttemptRecord {
                        step: "B3",
                        fallback_attempt_no: 1,
                        result: classify(&fail),
                        http_status: None,
                        error_code: &code,
                        error_message: &message,
                        count_toward_limit: true,
                    },
                );
                match kind {
                    ApiFailKind::NghiepVu | ApiFailKind::HetQuota => return Err(fail),
                    ApiFailKind::TamThoi { retry_after_s } => {
                        local_transient_try += 1;
                        if total_attempts >= max_attempts {
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
                            count_toward_limit: true,
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
                let total_attempts = record_attempt(
                    ctx.conn,
                    ctx.job_id,
                    AttemptRecord {
                        step: "B3",
                        fallback_attempt_no: local_transient_try,
                        result: classify(&fail),
                        http_status: fail.http_status.map(|s| s as i64),
                        error_code: &fail.code,
                        error_message: &fail.message,
                        count_toward_limit: true,
                    },
                );
                match fail.kind {
                    ApiFailKind::NghiepVu | ApiFailKind::HetQuota => return Err(fail),
                    ApiFailKind::TamThoi { retry_after_s } => {
                        if total_attempts >= max_attempts {
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

    for r in &rows {
        let prev = job_for_row(&latest_map, &rows, r);
        let db_img = map_lookup_legacy(&media_map, &rows, r)
            .and_then(|m| m.image_file.as_deref());
        let mut r_eff = r.clone();
        if r_eff.anh_da_tao.is_empty() {
            if let Some(img_f) = db_img {
                if image_out.join(img_f).is_file() {
                    r_eff.anh_da_tao = img_f.to_string();
                }
            }
        }
        if plan_row(&r_eff, prev, cfg, &default_engine_cfg, &image_out, db_img).is_some() {
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

        assert!(plan_row(&row, Some(&prev), &cfg, &eng_cfg, &img_out, Some("hero_gen.png")).is_none());

        row.promt_tao_video = "prompt video MOI".into();
        let p_vid = plan_row(&row, Some(&prev), &cfg, &eng_cfg, &img_out, Some("hero_gen.png")).unwrap();
        assert_eq!(p_vid.steps, vec![Step::B3]);
        assert!(!p_vid.force_new_image);
        assert!(p_vid.force_new_video);
        assert_eq!(p_vid.carried_attempts, 0);

        row.promt_tao_image = "prompt anh MOI".into();
        let p_img = plan_row(&row, Some(&prev), &cfg, &eng_cfg, &img_out, Some("hero_gen.png")).unwrap();
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
        assert!(plan_row(&row, Some(&prev_skipped), &cfg, &single_or_batch_cfg, &img_out, None).is_none());

        let prev_maxed = LatestJobInfo {
            status: "loi_tao_image".into(),
            attempts_count: 3,
            ..prev_skipped.clone()
        };
        // Start đơn lẻ (run_row) hoặc batch da_chon đều KHÔNG vượt rào 3/3
        assert!(plan_row(&row, Some(&prev_maxed), &cfg, &single_or_batch_cfg, &img_out, None).is_none());

        // rerun_error_rows (reset_error_attempts: true) mở khóa các dòng lỗi 3/3 của lượt trước và reset attempts = 0!
        let rerun_err_cfg = EngineConfig {
            kind: "lam_lai",
            row_keys: vec!["item.png".into()],
            force_step: None,
            reset_error_attempts: true,
        };
        let unlocked = plan_row(&row, Some(&prev_maxed), &cfg, &rerun_err_cfg, &img_out, None)
            .expect("rerun_error_rows must unlock 3/3 error rows");
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
        assert_eq!(b3_res.db_status, "loi_tao_video");
        assert_eq!(b3_res.image_status, "xong");
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

        // 2) POLL_TIMEOUT -> attempts = 1
        trigger_stop_on_poll.store(false, Ordering::SeqCst);
        stop_flag.store(false, Ordering::SeqCst);
        let _ = run_with_emitter(&noop_emit, &cfg, &dir, EngineConfig::default(), &stop_flag).unwrap();
        {
            let conn = crate::state_db::open(&dir).unwrap();
            assert_eq!(latest_job_info(&conn, &bag_key).unwrap().attempts_count, 1);
        }

        // 3) Poll gặp HTTP 502 liên tiếp khi đang carried_attempts = 1 -> phải dừng đúng ở 3/3 (KHÔNG bao giờ lên 4/3 hay 5/3!)
        phase.store(99, Ordering::SeqCst);
        let _ = run_with_emitter(&noop_emit, &cfg, &dir, EngineConfig::default(), &stop_flag).unwrap();
        {
            let conn = crate::state_db::open(&dir).unwrap();
            assert_eq!(
                latest_job_info(&conn, &bag_key).unwrap().attempts_count,
                3,
                "row-wide attempts_count must be capped strictly at max_attempts (3/3)!"
            );
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
}
