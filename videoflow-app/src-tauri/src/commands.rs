//! Tầng command Tauri theo hợp đồng 13.2 - giao diện gọi lõi qua đây, không
//! bao giờ gọi API ngoài hay đọc/ghi file trực tiếp. Tên command và payload
//! khớp 100% với `src/lib/api.ts`.

use crate::engine::{self, EngineConfig, Step};
use crate::error::{AppError, AppResult};
use crate::project;
use crate::prompts_csv;
use crate::settings::{self, Settings, SettingsStore};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;

const STATUS_FILTER_KEYS: [&str; 8] = [
    "Tất cả",
    "Đang chờ",
    "Đã tạo image",
    "Đã tạo video",
    "Lỗi tạo image",
    "Lỗi tạo video",
    "Lỗi tạm thời",
    "Bỏ qua",
];

/// Trạng thái toàn cục của app
pub struct AppState {
    pub settings: SettingsStore,
    pub settings_cache: parking_lot::RwLock<Settings>,
    pub project: parking_lot::RwLock<Option<PathBuf>>,
    pub last_probed_dir: parking_lot::RwLock<Option<PathBuf>>,
    pub approved_export_paths: parking_lot::RwLock<HashSet<String>>,
    pub running: AtomicBool,
    pub exit_requested: AtomicBool,
    pub active_run_id: AtomicI64,
    pub stop_flag: parking_lot::RwLock<Option<Arc<AtomicBool>>>,
    pub engine_thread: parking_lot::Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Default for AppState {
    fn default() -> Self {
        let store = SettingsStore::new();
        let cache = store.load();
        Self {
            settings: store,
            settings_cache: parking_lot::RwLock::new(cache),
            project: parking_lot::RwLock::new(None),
            last_probed_dir: parking_lot::RwLock::new(None),
            approved_export_paths: parking_lot::RwLock::new(HashSet::new()),
            running: AtomicBool::new(false),
            exit_requested: AtomicBool::new(false),
            active_run_id: AtomicI64::new(0),
            stop_flag: parking_lot::RwLock::new(None),
            engine_thread: parking_lot::Mutex::new(None),
        }
    }
}

impl AppState {
    pub fn new() -> Self {
        Self::default()
    }
}

pub(crate) fn project_path(state: &AppState) -> AppResult<PathBuf> {
    state
        .project
        .read()
        .clone()
        .ok_or_else(|| AppError::new("NO_PROJECT", "Chưa chọn thư mục dự án nào."))
}

fn ensure_not_running(state: &AppState) -> AppResult<()> {
    if state.running.load(Ordering::SeqCst) {
        return Err(AppError::new(
            "RUN_IN_PROGRESS",
            "Lượt chạy đang thực hiện - Stop hoặc chờ chạy xong (6.12).",
        ));
    }
    Ok(())
}

fn normalize_win_path_key(p: &str) -> String {
    let replaced = p.trim().replace('/', "\\");
    let stripped = replaced
        .strip_prefix(r"\\?\UNC\")
        .map(|u| format!(r"\\{u}"))
        .or_else(|| replaced.strip_prefix(r"\\?\").map(ToOwned::to_owned))
        .unwrap_or(replaced);
    let trimmed = if stripped.len() <= 3 && stripped.as_bytes().get(1) == Some(&b':') {
        stripped
    } else {
        stripped.trim_end_matches('\\').to_string()
    };
    trimmed.to_lowercase()
}

pub(crate) fn strip_verbatim_prefix(raw: String) -> String {
    if let Some(rest) = raw.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = raw.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        raw
    }
}

pub(crate) fn clean_abs_path_string(path: &Path) -> String {
    project::normalize_project_path(path)
        .to_string_lossy()
        .into_owned()
}

fn compute_duration_ms(started_at: &str, finished_at: Option<&str>) -> Option<i64> {
    let fin = finished_at?;
    let s = chrono::DateTime::parse_from_rfc3339(started_at).ok()?;
    let f = chrono::DateTime::parse_from_rfc3339(fin).ok()?;
    Some((f - s).num_milliseconds().max(0))
}

pub mod history;
pub mod muse;
pub mod proxies;
pub mod research;
pub use history::*;
pub use muse::*;
pub use proxies::*;

// ━━ Kiểu trả về theo hợp đồng IPC ━━

pub use crate::project::ProjectInfo;

#[derive(Serialize, Clone)]
pub struct RecentProject {
    pub path: String,
    pub name: String,
    pub row_count: usize,
    pub exists: bool,
    pub is_current: bool,
}

#[derive(Serialize, Clone)]
pub struct UntrackedImageItem {
    pub file_name: String,
    pub abs_path: String,
    pub size_bytes: u64,
}

#[derive(Serialize, Clone)]
pub struct RowSummary {
    pub row_key: String,
    /// Tên file ảnh tham chiếu (cột A) — hiển thị cho người dùng.
    /// `row_key` là khóa dòng (`tên ảnh#hash prompt`), có thể khác tên ảnh.
    pub image_name: String,
    pub status: String,
    pub image_file: String,
    pub video_file: String,
    pub attempts_count: i64,
    pub last_error: Option<String>,
    pub row_no: usize,
    pub prompt_modified: bool,
    pub ref_exists: bool,
    pub noi_canh: bool,
}

#[derive(Serialize)]
pub struct RowPage {
    pub rows: Vec<RowSummary>,
    pub total: usize,
    pub page: usize,
    pub page_count: usize,
    pub status_counts: HashMap<String, usize>,
}

#[derive(Serialize, Clone)]
pub struct TimelineEntry {
    pub ts: String,
    pub step: String,
    pub result: String,
    pub error: Option<String>,
    pub run_id: Option<i64>,
    pub attempt_no: Option<i64>,
    pub http_status: Option<i64>,
    pub duration_ms: Option<i64>,
}

#[derive(Serialize, Clone)]
pub struct FileRef {
    pub kind: String,
    pub file_name: String,
    pub file_path: String,
    pub abs_path: String,
    pub version: i64,
    pub is_current: bool,
    pub size_bytes: Option<i64>,
    pub created_at: Option<String>,
}

#[derive(Serialize, Clone)]
pub struct RowDetail {
    pub row: RowSummary,
    pub prompt_image: String,
    pub prompt_video: String,
    pub ref_exists: bool,
    pub ref_abs_path: String,
    pub image_abs_path: Option<String>,
    pub video_abs_path: Option<String>,
    pub timeline: Vec<TimelineEntry>,
    pub files: Vec<FileRef>,
    pub video_duration: Option<f64>,
    pub video_resolution: Option<String>,
    pub account_used: Option<String>,
}

#[derive(Serialize, Clone)]
pub struct RunSummary {
    pub id: i64,
    pub kind: String,
    pub status: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub total_rows: i64,
    pub done_count: i64,
    pub error_count: i64,
    pub duration_ms: Option<i64>,
}

#[derive(Serialize, Clone, Default)]
pub struct HistoryStats7d {
    pub runs_count: i64,
    pub done_rows: i64,
    pub error_rows: i64,
}

#[derive(Serialize)]
pub struct HistoryPage {
    pub runs: Vec<RunSummary>,
    pub total: usize,
    pub page: usize,
    pub page_count: usize,
    pub stats_7d: HistoryStats7d,
}

#[derive(Serialize, Clone)]
pub struct LogItem {
    pub id: i64,
    pub ts: String,
    pub level: String,
    pub run_id: Option<i64>,
    pub row_key: Option<String>,
    pub step: Option<String>,
    pub message: String,
    pub detail: Option<String>,
}

#[derive(Serialize)]
pub struct SettingsView {
    pub settings: Settings,
    pub image_key_preview: Option<String>,
    pub video_key_preview: Option<String>,
    pub prompt_key_preview: Option<String>,
    pub has_prompt_key: bool,
    pub has_image_key: bool,
    pub has_video_key: bool,
    pub muse_total_accounts: usize,
    pub muse_video_accounts: usize,
    pub max_concurrency: u32,
    pub ffmpeg_available: bool,
    pub ffmpeg_resolved_path: Option<String>,
}

#[derive(Deserialize, Default)]
struct HistoryExportFilter {
    status: Option<String>,
    kind: Option<String>,
    from: Option<String>,
    to: Option<String>,
}

// ━━ Commands: Dự án & Cấu trúc thư mục ━━

#[tauri::command]
pub fn list_recent_projects(state: State<'_, AppState>) -> Vec<RecentProject> {
    let s = state.settings_cache.read().clone();
    let cur_key = state
        .project
        .read()
        .as_ref()
        .map(|p| normalize_win_path_key(&p.to_string_lossy()));

    let mut seen = HashSet::new();
    let mut out = Vec::new();

    for p in &s.recent_projects {
        let norm_pb = project::normalize_project_path(Path::new(p));
        let norm_str = norm_pb.to_string_lossy().into_owned();
        let key = normalize_win_path_key(&norm_str);
        if key.is_empty() || !seen.insert(key.clone()) {
            continue;
        }
        let exists = norm_pb.is_dir();
        let row_count = if exists {
            prompts_csv::read_all(&norm_pb.join("prompts.csv"))
                .map(|rows| {
                    rows.iter()
                        .filter(|r| !r.is_example() && !r.is_empty_content())
                        .count()
                })
                .unwrap_or(0)
        } else {
            0
        };
        let name = norm_pb
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| norm_str.clone());
        out.push(RecentProject {
            path: norm_str,
            name,
            row_count,
            exists,
            is_current: cur_key.as_deref() == Some(key.as_str()),
        });
        if out.len() >= 5 {
            break;
        }
    }
    out
}

#[tauri::command]
pub fn remove_recent_project(path: String, state: State<'_, AppState>) -> AppResult<bool> {
    let target_key = normalize_win_path_key(&path);
    let mut s = state.settings_cache.write();
    let before = s.recent_projects.len();
    s.recent_projects
        .retain(|x| normalize_win_path_key(x) != target_key);
    if s.current_project
        .as_deref()
        .is_some_and(|c| normalize_win_path_key(c) == target_key)
        && state.project.read().is_none()
    {
        s.current_project = None;
    }
    state.settings.save(&s)?;
    Ok(s.recent_projects.len() != before)
}

#[tauri::command]
pub fn probe_folder(path: String, state: State<'_, AppState>) -> AppResult<project::ProbeResult> {
    let p = PathBuf::from(path.trim());
    if p.as_os_str().is_empty() || !p.is_dir() {
        return Err(AppError::new(
            "DIR_NOT_FOUND",
            "Thư mục được chọn không tồn tại.",
        ));
    }
    *state.last_probed_dir.write() = Some(project::normalize_project_path(&p));
    Ok(project::probe_project(&p))
}

#[tauri::command]
pub fn load_project(
    path: Option<String>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> AppResult<ProjectInfo> {
    ensure_not_running(&state)?;
    let raw_p: PathBuf = match path.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => {
            let s = state.settings_cache.read().clone();
            s.recent_projects
                .first()
                .map(PathBuf::from)
                .ok_or_else(|| AppError::new("NO_PROJECT", "Chưa có dự án nào đã mở."))?
        }
    };
    if !raw_p.is_dir() {
        return Err(AppError::new(
            "PROJECT_MISSING",
            "Không tìm thấy thư mục dự án tại đường dẫn đã lưu.",
        ));
    }

    let p = project::normalize_project_path(&raw_p);
    let _ = app.asset_protocol_scope().allow_directory(&p, true);

    let cleanup_days = state.settings_cache.read().auto_cleanup_days;
    if let Ok((conn, recovered_corrupt)) = project::open_or_recover_db(&p) {
        if recovered_corrupt {
            settings::log(
                &conn,
                "warn",
                None,
                None,
                None,
                "Phát hiện file state.db cũ bị hỏng — đã sao lưu sang state.db.corrupt_* và khởi tạo DB mới an toàn (6.9c)",
            );
        }
        if let Ok(recovered) = crate::state_db::recover_interrupted_runs(&conn) {
            if recovered > 0 {
                settings::log(
                    &conn,
                    "warn",
                    None,
                    None,
                    None,
                    &format!(
                        "Đã phục hồi {recovered} lượt chạy đang dở trước đó sang trạng thái Gián đoạn"
                    ),
                );
            }
        }
        if cleanup_days > 0 {
            let _ = crate::state_db::cleanup_old_history(&conn, cleanup_days);
        }
    }

    // [P1-4]: Đồng bộ kết quả từ state.db -> prompts.csv ngay khi mở dự án (nếu Excel đã đóng)
    let _ = engine::sync_csv_if_unlocked(&p);

    let info = project::load_project(&p)?;

    {
        let p_str = p.to_string_lossy().into_owned();
        let p_key = normalize_win_path_key(&p_str);
        let mut s = state.settings_cache.write();
        s.recent_projects
            .retain(|x| normalize_win_path_key(x) != p_key);
        let mut deduped = vec![p_str.clone()];
        let mut seen = HashSet::new();
        seen.insert(p_key);
        for old in s.recent_projects.drain(..) {
            let norm_old = project::normalize_project_path(Path::new(&old))
                .to_string_lossy()
                .into_owned();
            let k = normalize_win_path_key(&norm_old);
            if !k.is_empty() && seen.insert(k) {
                deduped.push(norm_old);
            }
            if deduped.len() >= 5 {
                break;
            }
        }
        s.recent_projects = deduped;
        s.current_project = Some(p_str);
        let _ = state.settings.save(&s);
    }
    *state.project.write() = Some(p);

    Ok(info)
}

#[tauri::command]
pub fn check_structure(state: State<'_, AppState>) -> AppResult<Vec<project::ChecklistItem>> {
    let p = project_path(&state)?;
    Ok(project::check_structure(&p))
}

#[tauri::command]
pub async fn pick_folder(app: AppHandle) -> Option<String> {
    tauri::async_runtime::spawn_blocking(move || {
        let (tx, rx) = std::sync::mpsc::channel::<Option<String>>();
        app.dialog()
            .file()
            .set_title("Chọn thư mục dự án")
            .pick_folder(move |f| {
                let chosen = f.map(|fp| {
                    fp.into_path()
                        .map(|pb| clean_abs_path_string(&pb))
                        .unwrap_or_else(|e| strip_verbatim_prefix(e.to_string()))
                });
                let _ = tx.send(chosen);
            });
        rx.recv().ok().flatten()
    })
    .await
    .ok()
    .flatten()
}

#[tauri::command]
pub fn create_project(
    path: String,
    mode: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> AppResult<ProjectInfo> {
    ensure_not_running(&state)?;
    let overwrite = mode == "ghi_de";
    project::create_project(&PathBuf::from(&path), overwrite)?;
    load_project(Some(path), app, state)
}

// ━━ Commands: Quét & Nhập hàng loạt ảnh từ `input/` ━━

pub fn scan_untracked_input_images_at(p: &Path) -> AppResult<Vec<UntrackedImageItem>> {
    let input_dir = p.join("input");
    if !input_dir.is_dir() {
        return Ok(Vec::new());
    }
    let csv_path = p.join("prompts.csv");
    let tracked_rows = if csv_path.is_file() {
        prompts_csv::read_all(&csv_path)?
    } else {
        Vec::new()
    };

    let mut tracked_keys = HashSet::new();
    let mut tracked_stems = HashSet::new();
    for r in &tracked_rows {
        if r.is_example() || r.is_empty_content() {
            continue;
        }
        tracked_keys.insert(r.anh_tham_chieu.to_lowercase());
        if let Some(stem) = Path::new(&r.anh_tham_chieu)
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
        {
            tracked_stems.insert(stem);
        }
    }

    let mut untracked = Vec::new();
    let entries = std::fs::read_dir(&input_dir).map_err(|e| {
        AppError::new(
            "READ_INPUT_FAIL",
            format!("Không đọc được thư mục input/: {e}"),
        )
    })?;

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext_ok = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg" | "webp"));
        if !ext_ok {
            continue;
        }
        let Some(fname) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        if fname.starts_with("EXAMPLE_") {
            continue;
        }
        let fname_low = fname.to_lowercase();
        let stem_low = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if tracked_keys.contains(&fname_low) || tracked_stems.contains(&stem_low) {
            continue;
        }
        let size_bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
        untracked.push(UntrackedImageItem {
            file_name: fname,
            abs_path: clean_abs_path_string(&path),
            size_bytes,
        });
    }

    untracked.sort_by_key(|a| a.file_name.to_lowercase());
    Ok(untracked)
}

#[tauri::command]
pub fn scan_untracked_input_images(state: State<'_, AppState>) -> AppResult<Vec<UntrackedImageItem>> {
    let p = project_path(&state)?;
    scan_untracked_input_images_at(&p)
}

pub fn import_input_images_at(
    p: &Path,
    file_names: &[String],
    default_prompt_image: &str,
    default_prompt_video: &str,
) -> AppResult<usize> {
    if file_names.is_empty() {
        return Ok(0);
    }
    let input_dir = p.join("input");
    let mut pairs: Vec<(String, String, String)> = Vec::new();
    for raw_name in file_names {
        let name = raw_name.trim();
        if name.is_empty() {
            continue;
        }
        if !input_dir.join(name).is_file() {
            return Err(AppError::new(
                "IMAGE_NOT_FOUND",
                format!("Không tìm thấy ảnh '{name}' trong thư mục input/."),
            ));
        }
        pairs.push((
            name.to_string(),
            default_prompt_image.trim().to_string(),
            default_prompt_video.trim().to_string(),
        ));
    }
    prompts_csv::append_rows(&p.join("prompts.csv"), &pairs)
}

#[tauri::command]
pub fn import_input_images(
    file_names: Vec<String>,
    default_prompt_image: String,
    default_prompt_video: String,
    state: State<'_, AppState>,
) -> AppResult<usize> {
    ensure_not_running(&state)?;
    let p = project_path(&state)?;
    import_input_images_at(&p, &file_names, &default_prompt_image, &default_prompt_video)
}

// ━━ Commands: Bảng Dòng việc, Chi tiết dòng & Đổi phiên bản chính ━━

#[tauri::command]
pub fn get_rows(
    status: Option<String>,
    query: Option<String>,
    page: usize,
    page_size: usize,
    state: State<'_, AppState>,
) -> AppResult<RowPage> {
    let p = project_path(&state)?;
    if !state.running.load(Ordering::SeqCst) {
        let _ = engine::sync_csv_if_unlocked(&p);
    }

    let csv = p.join("prompts.csv");
    let rows = prompts_csv::read_all(&csv)?;

    let conn =
        crate::state_db::open(&p).map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    let latest_map = engine::load_latest_jobs_map(&conn);
    let media_map = engine::load_current_media_map(&conn);
    let img_counts = engine::image_counts(&rows);
    let img_dir = p.join("image_out");
    let vid_dir = p.join("video_out");

    let mut status_counts: HashMap<String, usize> = STATUS_FILTER_KEYS
        .iter()
        .map(|k| ((*k).to_string(), 0usize))
        .collect();

    let mut summaries: Vec<RowSummary> = Vec::new();
    for r in &rows {
        if r.is_example() || r.is_empty_content() {
            continue;
        }
        // Khoá tra state.db là khóa dòng (tên ảnh + hash prompt), không phải tên ảnh —
        // nếu dùng tên ảnh thì nhiều dòng cùng ảnh sẽ đọc chung một bản ghi job.
        let row_key = prompts_csv::row_key_of(
            &r.anh_tham_chieu,
            &r.promt_tao_image,
            &r.promt_tao_video,
        );
        let key_low = row_key.to_lowercase();
        let prev_owned = engine::latest_job_for_csv_row(&conn, r, &latest_map, &img_counts);
        let prev = prev_owned.as_ref();
        let media = media_map
            .get(&key_low)
            .or_else(|| prev.and_then(|j| media_map.get(&j.row_key.to_lowercase())));
        let label = engine::effective_status_label(r, prev);
        let prompt_modified = engine::is_prompt_modified(r, prev);
        let attempts_count = prev.map(|j| j.attempts_count).unwrap_or(0);
        let last_error = prev
            .and_then(|j| j.last_error.clone())
            .or_else(|| (!r.ghi_chu_loi.trim().is_empty()).then(|| r.ghi_chu_loi.clone()));

        let image_file = if !r.anh_da_tao.trim().is_empty() {
            r.anh_da_tao.clone()
        } else {
            media
                .and_then(|m| m.image_file.as_ref())
                .filter(|f| img_dir.join(f).is_file())
                .cloned()
                .unwrap_or_default()
        };
        let video_file = if !r.video_da_tao.trim().is_empty() {
            r.video_da_tao.clone()
        } else {
            media
                .and_then(|m| m.video_file.as_ref())
                .filter(|f| vid_dir.join(f).is_file())
                .cloned()
                .unwrap_or_default()
        };

        *status_counts.entry("Tất cả".into()).or_insert(0) += 1;
        *status_counts.entry(label.to_string()).or_insert(0) += 1;

        let ref_exists = p.join("input").join(r.anh_tham_chieu.trim()).is_file();
        summaries.push(RowSummary {
            row_key,
            image_name: r.anh_tham_chieu.clone(),
            status: label.into(),
            image_file,
            video_file,
            attempts_count,
            last_error,
            row_no: r.row_no,
            prompt_modified,
            ref_exists,
            noi_canh: r.noi_canh,
        });
    }

    if let Some(st) = status.as_deref().map(str::trim) {
        if !st.is_empty() && st != "Tất cả" {
            if st == "Lỗi" {
                summaries.retain(|r| {
                    r.status == "Lỗi tạo image"
                        || r.status == "Lỗi tạo video"
                        || r.status == "Lỗi tạm thời"
                });
            } else {
                summaries.retain(|r| r.status == st);
            }
        }
    }

    if let Some(q) = query.as_deref().map(str::trim) {
        if !q.is_empty() {
            let q_low = q.to_lowercase();
            summaries.retain(|r| {
                r.row_key.to_lowercase().contains(&q_low)
                    || r.image_name.to_lowercase().contains(&q_low)
            });
        }
    }

    let total = summaries.len();
    let page_size = page_size.max(1);
    let page_count = total.div_ceil(page_size).max(1);
    let page = page.max(1).min(page_count);
    let slice: Vec<RowSummary> = summaries
        .into_iter()
        .skip((page - 1) * page_size)
        .take(page_size)
        .collect();

    Ok(RowPage {
        rows: slice,
        total,
        page,
        page_count,
        status_counts,
    })
}

/// Đọc nhanh metadata cơ bản (thời lượng giây, độ phân giải width x height) từ file MP4 nếu có.
/// Hoạt động an toàn, fail-safe (trả về (None, None) nếu file không hợp lệ hoặc không phải MP4).
pub fn probe_mp4_metadata(path: &Path) -> (Option<f64>, Option<String>) {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return (None, None);
    };

    let Ok(meta) = f.metadata() else {
        return (None, None);
    };
    let file_len = meta.len();
    if file_len < 8 {
        return (None, None);
    }

    let mut duration_sec: Option<f64> = None;
    let mut resolution: Option<String> = None;

    fn scan_boxes<R: Read + Seek>(
        reader: &mut R,
        start_offset: u64,
        max_len: u64,
        duration_out: &mut Option<f64>,
        res_out: &mut Option<String>,
        depth: usize,
    ) {
        if depth > 5 {
            return;
        }
        let Some(end_offset) = start_offset.checked_add(max_len) else {
            return;
        };
        let mut cur = start_offset;
        while cur.checked_add(8).is_some_and(|next8| next8 <= end_offset) {
            if reader.seek(SeekFrom::Start(cur)).is_err() {
                break;
            }
            let mut header = [0u8; 8];
            if reader.read_exact(&mut header).is_err() {
                break;
            }
            let size32 = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as u64;
            let tag = &header[4..8];

            let (box_size, header_size) = if size32 == 1 {
                let mut ext = [0u8; 8];
                if reader.read_exact(&mut ext).is_err() {
                    break;
                }
                (u64::from_be_bytes(ext), 16u64)
            } else if size32 == 0 {
                (end_offset.saturating_sub(cur), 8u64)
            } else {
                (size32, 8u64)
            };

            let Some(next_cur) = cur.checked_add(box_size) else {
                break;
            };
            if box_size < header_size || next_cur > end_offset {
                break;
            }

            let Some(payload_offset) = cur.checked_add(header_size) else {
                break;
            };
            let payload_len = box_size - header_size;

            if tag == b"moov" || tag == b"trak" {
                scan_boxes(reader, payload_offset, payload_len, duration_out, res_out, depth + 1);
            } else if tag == b"mvhd" && duration_out.is_none() && payload_len >= 20 {
                if reader.seek(SeekFrom::Start(payload_offset)).is_ok() {
                    let mut buf = [0u8; 32];
                    let read_len = buf.len().min(payload_len as usize);
                    if reader.read_exact(&mut buf[..read_len]).is_ok() {
                        let version = buf[0];
                        if version == 0 && read_len >= 20 {
                            let timescale = u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]) as f64;
                            let duration = u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]) as f64;
                            if timescale > 0.0 {
                                *duration_out = Some(duration / timescale);
                            }
                        } else if version == 1 && read_len >= 32 {
                            let timescale = u32::from_be_bytes([buf[20], buf[21], buf[22], buf[23]]) as f64;
                            let duration = u64::from_be_bytes([
                                buf[24], buf[25], buf[26], buf[27], buf[28], buf[29], buf[30], buf[31],
                            ]) as f64;
                            if timescale > 0.0 {
                                *duration_out = Some(duration / timescale);
                            }
                        }
                    }
                }
            } else if tag == b"tkhd"
                && res_out.is_none()
                && payload_len >= 80
                && reader.seek(SeekFrom::Start(payload_offset)).is_ok()
            {
                let mut buf = [0u8; 96];
                let read_len = buf.len().min(payload_len as usize);
                if reader.read_exact(&mut buf[..read_len]).is_ok() {
                    let version = buf[0];
                    let offset = if version == 0 { 76 } else { 88 };
                    if read_len >= offset + 8 {
                        let w = u32::from_be_bytes([buf[offset], buf[offset + 1], buf[offset + 2], buf[offset + 3]]) >> 16;
                        let h = u32::from_be_bytes([buf[offset + 4], buf[offset + 5], buf[offset + 6], buf[offset + 7]]) >> 16;
                        if w > 0 && h > 0 {
                            *res_out = Some(format!("{w}x{h}"));
                        }
                    }
                }
            }

            if duration_out.is_some() && res_out.is_some() {
                break;
            }

            cur = next_cur;
        }
    }

    scan_boxes(&mut f, 0, file_len, &mut duration_sec, &mut resolution, 0);

    (duration_sec, resolution)
}

pub fn build_row_detail_at(p: &Path, row_key: &str) -> AppResult<RowDetail> {
    let rows = prompts_csv::read_all(&p.join("prompts.csv"))?;
    let row = prompts_csv::find_row_by_key(&rows, row_key)
        .ok_or_else(|| AppError::new("ROW_NOT_FOUND", format!("Không tìm thấy dòng '{row_key}'.")))?;

    // Khoá dòng chuẩn hoá (tên ảnh + hash prompt) — dùng cho mọi truy vấn state.db.
    let actual_key = &prompts_csv::row_key_of(
        &row.anh_tham_chieu,
        &row.promt_tao_image,
        &row.promt_tao_video,
    );
    let conn =
        crate::state_db::open(p).map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    // Tra job nhất quán theo actual_key -> file_refs -> legacy (khi ảnh duy nhất).
    let latest_map = engine::load_latest_jobs_map(&conn);
    let img_counts = engine::image_counts(&rows);
    let prev_owned = engine::latest_job_for_csv_row(&conn, row, &latest_map, &img_counts);
    let prev = prev_owned.as_ref();

    let status_label = engine::effective_status_label(row, prev);
    let prompt_modified = engine::is_prompt_modified(row, prev);
    let attempts_count = prev.map(|j| j.attempts_count).unwrap_or(0);
    let last_error = prev
        .and_then(|j| j.last_error.clone())
        .or_else(|| (!row.ghi_chu_loi.trim().is_empty()).then(|| row.ghi_chu_loi.clone()));

    // File tham chiếu nằm ở input/<tên ảnh> — KHÔNG dùng khoá dòng (có '#hash').
    let ref_path = p.join("input").join(row.anh_tham_chieu.trim());
    let ref_exists = ref_path.is_file();
    let ref_abs_path = clean_abs_path_string(&ref_path);

    let media_map = engine::load_current_media_map(&conn);
    let media_cur = media_map
        .get(&actual_key.to_lowercase())
        .or_else(|| prev.and_then(|j| media_map.get(&j.row_key.to_lowercase())));

    let eff_image_file = if let Some(f) = media_cur
        .and_then(|m| m.image_file.as_ref())
        .filter(|f| p.join("image_out").join(f).is_file())
    {
        f.clone()
    } else {
        row.anh_da_tao.clone()
    };

    let eff_video_file = if let Some(f) = media_cur
        .and_then(|m| m.video_file.as_ref())
        .filter(|f| p.join("video_out").join(f).is_file())
    {
        f.clone()
    } else {
        row.video_da_tao.clone()
    };

    let image_abs_path = if !eff_image_file.trim().is_empty() {
        let img_p = p.join("image_out").join(&eff_image_file);
        img_p.is_file().then(|| clean_abs_path_string(&img_p))
    } else {
        None
    };

    let video_abs_path = if !eff_video_file.trim().is_empty() {
        let vid_p = p.join("video_out").join(&eff_video_file);
        vid_p.is_file().then(|| clean_abs_path_string(&vid_p))
    } else {
        None
    };

    // Gom toàn bộ khoá liên quan của dòng (actual_key + khoá job gốc khi sửa prompt / nâng cấp v1.4.0)
    let mut related_keys: Vec<String> = vec![actual_key.to_lowercase()];
    if let Some(j) = prev {
        let jk = j.row_key.to_lowercase();
        if !related_keys.contains(&jk) {
            related_keys.push(jk);
        }
    }
    let legacy_low = row.anh_tham_chieu.trim().to_lowercase();
    if img_counts.get(&legacy_low).copied().unwrap_or(0) == 1 && !related_keys.contains(&legacy_low)
    {
        related_keys.push(legacy_low);
    }

    let mut timeline = Vec::new();
    let mut seen_attempt_ids = std::collections::HashSet::<i64>::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT a.id, a.started_at, a.step, a.result, a.error_message,
                j.run_id, a.attempt_no, a.http_status, j.duration_ms
         FROM attempts a
         INNER JOIN jobs j ON j.id = a.job_id
         WHERE LOWER(j.row_key) = LOWER(?1)
         ORDER BY a.id DESC
         LIMIT 30",
    ) {
        let mut raw_timeline: Vec<(i64, TimelineEntry)> = Vec::new();
        for rk in &related_keys {
            if let Ok(it) = stmt.query_map(rusqlite::params![rk], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    TimelineEntry {
                        ts: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        step: r.get::<_, String>(2)?,
                        result: r.get::<_, String>(3)?,
                        error: r.get::<_, Option<String>>(4)?,
                        run_id: r.get::<_, Option<i64>>(5)?,
                        attempt_no: r.get::<_, Option<i64>>(6)?,
                        http_status: r.get::<_, Option<i64>>(7)?,
                        duration_ms: r.get::<_, Option<i64>>(8)?,
                    },
                ))
            }) {
                for (aid, item) in it.flatten() {
                    if seen_attempt_ids.insert(aid) {
                        raw_timeline.push((aid, item));
                    }
                }
            }
        }
        raw_timeline.sort_by_key(|a| std::cmp::Reverse(a.0));
        raw_timeline.truncate(30);
        timeline = raw_timeline.into_iter().map(|(_, e)| e).collect();
    }

    let mut files = Vec::new();
    let mut seen_files = std::collections::HashSet::<(String, String, i64)>::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT kind, file_name, file_path, version, is_current, size_bytes, created_at
         FROM file_refs
         WHERE LOWER(row_key) = LOWER(?1)
         ORDER BY kind ASC, version DESC",
    ) {
        for rk in &related_keys {
            if let Ok(it) = stmt.query_map(rusqlite::params![rk], |r| {
                let rel_path: String = r.get(2)?;
                let abs = clean_abs_path_string(&p.join(&rel_path));
                Ok(FileRef {
                    kind: r.get(0)?,
                    file_name: r.get(1)?,
                    file_path: rel_path,
                    abs_path: abs,
                    version: r.get(3)?,
                    is_current: r.get::<_, i64>(4)? == 1,
                    size_bytes: r.get(5)?,
                    created_at: r.get(6)?,
                })
            }) {
                for item in it.flatten() {
                    let key = (item.kind.clone(), item.file_name.clone(), item.version);
                    if seen_files.insert(key) {
                        files.push(item);
                    }
                }
            }
        }
        files.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| b.version.cmp(&a.version)));
    }

    if !files.iter().any(|f| f.kind == "anh") {
        if let Some(ref abs) = image_abs_path {
            let size_bytes = std::fs::metadata(abs).map(|m| m.len() as i64).ok();
            files.push(FileRef {
                kind: "anh".into(),
                file_name: eff_image_file.clone(),
                file_path: format!("image_out/{eff_image_file}"),
                abs_path: abs.clone(),
                version: 1,
                is_current: true,
                size_bytes,
                created_at: (!row.ngay_tao_anh.is_empty()).then(|| row.ngay_tao_anh.clone()),
            });
        }
    }
    if !files.iter().any(|f| f.kind == "video") {
        if let Some(ref abs) = video_abs_path {
            let size_bytes = std::fs::metadata(abs).map(|m| m.len() as i64).ok();
            files.push(FileRef {
                kind: "video".into(),
                file_name: eff_video_file.clone(),
                file_path: format!("video_out/{eff_video_file}"),
                abs_path: abs.clone(),
                version: 1,
                is_current: true,
                size_bytes,
                created_at: (!row.ngay_tao_video.is_empty()).then(|| row.ngay_tao_video.clone()),
            });
        }
    }

    // Chỉ trả `account_used` khi có dữ liệu tài khoản được ghi nhận thật cho job;
    // không suy đoán bừa từ accounts.json (M5).
    let account_used: Option<String> = None;

    let (video_duration, video_resolution) = if let Some(ref vpath) = video_abs_path {
        probe_mp4_metadata(Path::new(vpath))
    } else {
        (None, None)
    };

    Ok(RowDetail {
        row: RowSummary {
            row_key: actual_key.clone(),
            image_name: row.anh_tham_chieu.clone(),
            status: status_label.into(),
            image_file: eff_image_file,
            video_file: eff_video_file,
            attempts_count,
            last_error,
            row_no: row.row_no,
            prompt_modified,
            ref_exists,
            noi_canh: row.noi_canh,
        },
        prompt_image: row.promt_tao_image.clone(),
        prompt_video: row.promt_tao_video.clone(),
        ref_exists,
        ref_abs_path,
        image_abs_path,
        video_abs_path,
        timeline,
        files,
        video_duration,
        video_resolution,
        account_used,
    })
}

#[tauri::command]
pub fn get_row_detail(row_key: String, state: State<'_, AppState>) -> AppResult<RowDetail> {
    let p = project_path(&state)?;
    build_row_detail_at(&p, &row_key)
}

pub fn set_current_file_version_at(
    p: &Path,
    row_key: &str,
    kind: &str,
    version: i64,
) -> AppResult<RowDetail> {
    let (db_kind, csv_kind, sub_dir) = match kind.trim().to_ascii_lowercase().as_str() {
        "image" | "anh" => ("anh", "image", "image_out"),
        "video" => ("video", "video", "video_out"),
        _ => {
            return Err(AppError::new(
                "BAD_MEDIA_KIND",
                "Loại file không hợp lệ (chỉ hỗ trợ 'image'/'anh' hoặc 'video').",
            ));
        }
    };

    let csv_path = p.join("prompts.csv");
    let rows = prompts_csv::read_all(&csv_path)?;
    let row = prompts_csv::find_row_by_key(&rows, row_key)
        .ok_or_else(|| AppError::new("ROW_NOT_FOUND", format!("Không tìm thấy dòng '{row_key}'.")))?;
    let actual_key = prompts_csv::row_key_of(
        &row.anh_tham_chieu,
        &row.promt_tao_image,
        &row.promt_tao_video,
    );

    let mut conn =
        crate::state_db::open(p).map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    let latest_map = engine::load_latest_jobs_map(&conn);
    let img_counts = engine::image_counts(&rows);
    let prev_owned = engine::latest_job_for_csv_row(&conn, row, &latest_map, &img_counts);

    let mut related_keys: Vec<String> = vec![actual_key.to_lowercase()];
    let req_low = row_key.trim().to_lowercase();
    if !req_low.is_empty() && !related_keys.contains(&req_low) {
        related_keys.push(req_low);
    }
    if let Some(ref j) = prev_owned {
        let jk = j.row_key.to_lowercase();
        if !related_keys.contains(&jk) {
            related_keys.push(jk);
        }
    }
    let legacy_low = row.anh_tham_chieu.trim().to_lowercase();
    if img_counts.get(&legacy_low).copied().unwrap_or(0) == 1 && !related_keys.contains(&legacy_low)
    {
        related_keys.push(legacy_low);
    }

    let mut found_ref: Option<(i64, String)> = None;
    for rk in &related_keys {
        if let Ok(hit) = conn.query_row(
            "SELECT id, file_name FROM file_refs
             WHERE LOWER(row_key) = LOWER(?1) AND kind = ?2 AND version = ?3
             ORDER BY id DESC LIMIT 1",
            rusqlite::params![rk, db_kind, version],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        ) {
            found_ref = Some(hit);
            break;
        }
    }
    let (target_ref_id, file_name) = found_ref.ok_or_else(|| {
        AppError::new(
            "VERSION_NOT_FOUND",
            format!("Không tìm thấy phiên bản v{version} của '{row_key}'."),
        )
    })?;

    if !p.join(sub_dir).join(&file_name).is_file() {
        return Err(AppError::new(
            "FILE_MISSING_ON_DISK",
            format!("File '{file_name}' không còn tồn tại trong {sub_dir}/."),
        ));
    }

    prompts_csv::set_active_media_file(&csv_path, &actual_key, csv_kind, &file_name)?;

    let tx = conn
        .transaction()
        .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    for rk in &related_keys {
        tx.execute(
            "UPDATE file_refs SET is_current = 0
             WHERE LOWER(row_key) = LOWER(?1) AND kind = ?2",
            rusqlite::params![rk, db_kind],
        )
        .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    }
    tx.execute(
        "UPDATE file_refs SET is_current = 1 WHERE id = ?1",
        rusqlite::params![target_ref_id],
    )
    .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    for rk in &related_keys {
        crate::state_db::migrate_file_refs_row_key(&tx, rk, &actual_key, Some(db_kind))
            .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    }
    tx.commit()
        .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;

    build_row_detail_at(p, &actual_key)
}

#[tauri::command]
pub fn set_current_file_version(
    row_key: String,
    kind: String,
    version: i64,
    state: State<'_, AppState>,
) -> AppResult<RowDetail> {
    ensure_not_running(&state)?;
    let p = project_path(&state)?;
    set_current_file_version_at(&p, &row_key, &kind, version)
}

// ━━ Commands: Điều khiển lượt chạy (Start / Stop / Dry-run / Rerun) ━━

#[tauri::command]
pub fn start_dry_run(state: State<'_, AppState>) -> AppResult<engine::DryRunResult> {
    let p = project_path(&state)?;
    let cfg = state.settings_cache.read().clone();
    engine::dry_run(&p, &cfg)
}

fn app_exiting_error() -> AppError {
    AppError::new(
        "APP_EXITING",
        "Ứng dụng đang thoát hoặc khởi động lại — không thể bắt đầu lượt chạy mới.",
    )
}

// Serialize the short CSV transfer commit with admission to the rendering engine.
pub(super) static PROJECT_WRITE_GATE: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

pub(crate) fn try_begin_run(state: &AppState) -> AppResult<()> {
    let _gate = PROJECT_WRITE_GATE.lock();
    if state.exit_requested.load(Ordering::SeqCst) {
        return Err(app_exiting_error());
    }
    state
        .running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .map_err(|_| {
            AppError::new(
                "RUN_IN_PROGRESS",
                "Lượt chạy đang thực hiện - Stop hoặc chờ chạy xong (6.12).",
            )
        })?;
    if state.exit_requested.load(Ordering::SeqCst) {
        state.running.store(false, Ordering::SeqCst);
        return Err(app_exiting_error());
    }
    Ok(())
}

pub(crate) fn begin_app_update_restart(state: &AppState) -> AppResult<()> {
    if state.exit_requested.swap(true, Ordering::SeqCst) {
        return Err(app_exiting_error());
    }
    if state.running.load(Ordering::SeqCst) {
        state.exit_requested.store(false, Ordering::SeqCst);
        return Err(AppError::new(
            "RUN_IN_PROGRESS",
            "Pipeline vừa bắt đầu chạy trong lúc tải bản cập nhật — hãy dừng (STOP) rồi khởi động lại app.",
        ));
    }
    Ok(())
}

pub(crate) fn finish_app_update_restart_with<FStop, FRestart>(
    state: &AppState,
    stop_sidecar: FStop,
    restart_app: FRestart,
) -> AppResult<bool>
where
    FStop: FnOnce(),
    FRestart: FnOnce(),
{
    begin_app_update_restart(state)?;
    stop_sidecar();
    restart_app();
    Ok(true)
}

pub(crate) fn spawn_engine_internal(
    app: AppHandle,
    state: &AppState,
    engine_cfg: EngineConfig,
) -> AppResult<i64> {
    try_begin_run(state)?;

    let p = match project_path(state) {
        Ok(p) => p,
        Err(e) => {
            state.running.store(false, Ordering::SeqCst);
            return Err(e);
        }
    };

    let csv = p.join("prompts.csv");
    if engine::is_csv_write_locked(&csv) {
        if let Ok(conn) = crate::state_db::open(&p) {
            settings::log(
                &conn,
                "warn",
                None,
                None,
                Some("ghi_sheet"),
                "prompts.csv đang bị khóa ghi (ví dụ đang mở trong Excel) - trạng thái vẫn lưu đầy đủ vào state.db",
            );
        }
    }

    let cfg = state.settings_cache.read().clone();
    let stop = Arc::new(AtomicBool::new(false));
    *state.stop_flag.write() = Some(stop.clone());
    state.active_run_id.store(0, Ordering::SeqCst);

    let (rid_tx, rid_rx) = std::sync::mpsc::sync_channel::<i64>(1);
    let created_run_id = Arc::new(AtomicI64::new(0));
    let created_rid_bg = created_run_id.clone();

    let mut thr_guard = state.engine_thread.lock();
    if state.exit_requested.load(Ordering::SeqCst) {
        *state.stop_flag.write() = None;
        state.running.store(false, Ordering::SeqCst);
        return Err(app_exiting_error());
    }

    let app2 = app.clone();
    let handle = std::thread::spawn(move || {
        let start_inst = Instant::now();
        let notify_enabled = cfg.notify_on_finish;
        let emit_cb = |name: &str, payload: serde_json::Value| {
            let _ = app2.emit(name, payload);
        };
        let rid_cb = |rid: i64| {
            created_rid_bg.store(rid, Ordering::SeqCst);
            app2.state::<AppState>()
                .active_run_id
                .store(rid, Ordering::SeqCst);
            let _ = rid_tx.try_send(rid);
        };

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            engine::run_with_emitter_and_hook(
                &emit_cb,
                Some(&rid_cb),
                &cfg,
                &p,
                engine_cfg,
                &stop,
            )
        }));

        // Dọn stop_flag & running = false TRƯỚC khi phát run_finished cho mọi nhánh
        // (hoàn tất, dừng, lỗi, panic) để UI nhận sự kiện run_finished có thể khởi động
        // lượt mới ngay mà không bị RUN_IN_PROGRESS (H7).
        let deferred_finish = match result {
            Ok(Ok(outcome)) => {
                if notify_enabled {
                    if let Some(win) = app2.get_webview_window("main") {
                        let _ = win.request_user_attention(Some(
                            tauri::UserAttentionType::Informational,
                        ));
                    }
                    let summary = if outcome.stopped {
                        format!(
                            "Lượt chạy #{} đã dừng an toàn ({} xong, {} lỗi).",
                            outcome.run_id, outcome.done, outcome.errors
                        )
                    } else {
                        format!(
                            "Lượt chạy #{} hoàn tất: {} dòng thành công, {} dòng lỗi.",
                            outcome.run_id, outcome.done, outcome.errors
                        )
                    };
                    let _ = app2.emit(
                        "desktop_notify",
                        serde_json::json!({
                            "title": "VideoFlow Desktop",
                            "body": summary
                        }),
                    );
                }
                crate::engine::build_run_finished_payload(&outcome)
            }
            Ok(Err(e)) => {
                let rid = created_rid_bg.load(Ordering::SeqCst);
                let (done_cnt, err_cnt) = if rid > 0 {
                    finalize_interrupted_run_in_db(&p, rid)
                } else {
                    (0, 0)
                };
                let safe_msg = crate::settings::redact_secrets(&e.message);
                let _ = app2.emit(
                    "run_error",
                    serde_json::json!({
                        "run_id": (rid > 0).then_some(rid), "row_key": null, "step": null,
                        "code": e.code, "message": safe_msg
                    }),
                );
                serde_json::json!({
                    "run_id": rid,
                    "status": "gian_doan",
                    "done_count": done_cnt,
                    "error_count": err_cnt,
                    "duration_ms": start_inst.elapsed().as_millis() as i64
                })
            }
            Err(panic_err) => {
                let rid = created_rid_bg.load(Ordering::SeqCst);
                let (done_cnt, err_cnt) = if rid > 0 {
                    finalize_interrupted_run_in_db(&p, rid)
                } else {
                    (0, 0)
                };
                let msg = panic_err
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic_err.downcast_ref::<&str>().map(|s| (*s).to_string()))
                    .unwrap_or_else(|| "Engine gặp lỗi nội bộ ngoài dự kiến".to_string());
                let safe_msg = crate::settings::redact_secrets(&msg);
                let _ = app2.emit(
                    "run_error",
                    serde_json::json!({
                        "run_id": (rid > 0).then_some(rid), "row_key": null, "step": null,
                        "code": "ENGINE_PANIC", "message": safe_msg
                    }),
                );
                serde_json::json!({
                    "run_id": rid,
                    "status": "gian_doan",
                    "done_count": done_cnt,
                    "error_count": err_cnt,
                    "duration_ms": start_inst.elapsed().as_millis() as i64
                })
            }
        };

        let st = app2.state::<AppState>();
        *st.stop_flag.write() = None;
        st.active_run_id.store(0, Ordering::SeqCst);
        st.running.store(false, Ordering::SeqCst);
        let _ = app2.emit("run_finished", deferred_finish);
    });

    *thr_guard = Some(handle);
    drop(thr_guard);
    let actual_run_id = rid_rx
        .recv_timeout(Duration::from_millis(400))
        .unwrap_or(0);
    Ok(actual_run_id)
}

#[tauri::command]
pub fn start_run(
    app: AppHandle,
    scope: String,
    row_keys: Option<Vec<String>>,
    state: State<'_, AppState>,
) -> AppResult<i64> {
    let _ = scope;
    let keys = row_keys.unwrap_or_default();
    spawn_engine_internal(
        app,
        &state,
        EngineConfig {
            kind: "chung",
            row_keys: keys,
            force_step: None,
            reset_error_attempts: false,
        },
    )
}

#[tauri::command]
pub fn run_row(row_key: String, app: AppHandle, state: State<'_, AppState>) -> AppResult<i64> {
    ensure_not_running(&state)?;
    let p = project_path(&state)?;
    let _cfg = state.settings_cache.read().clone();
    let rows = prompts_csv::read_all(&p.join("prompts.csv"))?;
    let row = prompts_csv::find_row_by_key(&rows, &row_key)
        .ok_or_else(|| AppError::new("ROW_NOT_FOUND", format!("Không tìm thấy dòng '{row_key}'.")))?;
    let key_of_row = prompts_csv::row_key_of(
        &row.anh_tham_chieu,
        &row.promt_tao_image,
        &row.promt_tao_video,
    );

    if let Ok(conn) = crate::state_db::open(&p) {
        if let Some(prev) = engine::latest_job_info(&conn, &key_of_row) {
            if prev.status == "bo_qua" {
                return Err(AppError::new(
                    "ROW_SKIPPED",
                    "Dòng này đang ở trạng thái Bỏ qua — hãy bấm 'Đặt lại trạng thái về Đang chờ' trước khi chạy.",
                ));
            }
        }
    }

    spawn_engine_internal(
        app,
        &state,
        EngineConfig {
            kind: "rieng",
            row_keys: vec![key_of_row],
            force_step: None,
            reset_error_attempts: false,
        },
    )
}

#[tauri::command]
pub fn rerun_row_step(
    row_key: String,
    step: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> AppResult<i64> {
    ensure_not_running(&state)?;
    let forced = match step.trim() {
        "anh" => Step::B2,
        "video" => Step::B3,
        _ => {
            return Err(AppError::new(
                "BAD_STEP",
                "Bước chạy lại không hợp lệ (chỉ chấp nhận 'anh' hoặc 'video').",
            ));
        }
    };

    let p = project_path(&state)?;
    let rows = prompts_csv::read_all(&p.join("prompts.csv"))?;
    let row = prompts_csv::find_row_by_key(&rows, &row_key)
        .ok_or_else(|| AppError::new("ROW_NOT_FOUND", format!("Không tìm thấy dòng '{row_key}'.")))?;
    let key_of_row = prompts_csv::row_key_of(
        &row.anh_tham_chieu,
        &row.promt_tao_image,
        &row.promt_tao_video,
    );

    if forced == Step::B3 {
        // [P0-2] / #22: Chặn chạy riêng bước Video nếu prompt tạo ảnh đã bị sửa đổi
        // (tra qua latest_job_for_csv_row để nhận diện cả khi đã bấm Lưu prompt làm đổi hash row_key).
        if let Ok(conn) = crate::state_db::open(&p) {
            let latest_map = engine::load_latest_jobs_map(&conn);
            let img_counts = engine::image_counts(&rows);
            let prev = engine::latest_job_for_csv_row(&conn, row, &latest_map, &img_counts);
            let (img_changed, _) = engine::detect_prompt_change(row, prev.as_ref());
            if img_changed {
                return Err(AppError::new(
                    "IMAGE_PROMPT_MODIFIED",
                    "Prompt tạo ảnh đã thay đổi — hãy chọn Tạo lại từ Ảnh + Video.",
                ));
            }
        }

        let detail = build_row_detail_at(&p, &key_of_row)?;
        if detail.row.image_file.trim().is_empty() {
            return Err(AppError::new(
                "NO_IMAGE",
                "Dòng này chưa có ảnh sản phẩm để chạy lại riêng bước video.",
            ));
        }
        if detail.image_abs_path.is_none() {
            return Err(AppError::new(
                "IMAGE_MISSING",
                format!(
                    "Không tìm thấy file ảnh '{}' trong thư mục image_out/.",
                    detail.row.image_file
                ),
            ));
        }
    }

    spawn_engine_internal(
        app,
        &state,
        EngineConfig {
            kind: "lam_lai",
            row_keys: vec![key_of_row],
            force_step: Some(forced),
            reset_error_attempts: false,
        },
    )
}

#[tauri::command]
pub fn stop_run(state: State<'_, AppState>) -> AppResult<bool> {
    if let Some(flag) = state.stop_flag.read().clone() {
        flag.store(true, Ordering::SeqCst);
        Ok(true)
    } else {
        Ok(false)
    }
}

#[tauri::command]
pub fn engine_done() {}

#[tauri::command]
pub fn is_running(state: State<'_, AppState>) -> bool {
    state.running.load(Ordering::SeqCst)
}

#[tauri::command]
pub fn rerun_error_rows(
    run_id: i64,
    app: AppHandle,
    state: State<'_, AppState>,
) -> AppResult<i64> {
    ensure_not_running(&state)?;
    let p = project_path(&state)?;
    let conn =
        crate::state_db::open(&p).map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    let mut keys: Vec<String> = Vec::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT row_key FROM jobs WHERE run_id = ?1 AND status LIKE 'loi_%' ORDER BY row_no, id",
    ) {
        if let Ok(it) = stmt.query_map(rusqlite::params![run_id], |r| r.get::<_, String>(0)) {
            for k in it.flatten() {
                keys.push(k);
            }
        }
    }
    if keys.is_empty() {
        return Err(AppError::new(
            "NO_ERROR_ROWS",
            "Lượt này không có dòng lỗi nào.",
        ));
    }

    // [P0-3] TC-45: Bật `reset_error_attempts: true` để các dòng lỗi 3/3 của lượt `run_id` được phép chạy lại!
    spawn_engine_internal(
        app,
        &state,
        EngineConfig {
            kind: "lam_lai",
            row_keys: keys,
            force_step: None,
            reset_error_attempts: true,
        },
    )
}

// ━━ Commands: Sửa dòng, Bỏ qua, Đặt lại, Đổi ảnh & Xóa dòng ━━

fn ensure_seed_run_id(conn: &rusqlite::Connection) -> AppResult<i64> {
    if let Ok(existing_id) = conn.query_row(
        "SELECT id FROM runs WHERE total_rows = 0 ORDER BY id ASC LIMIT 1",
        [],
        |r| r.get::<_, i64>(0),
    ) {
        return Ok(existing_id);
    }
    let ts = chrono::Local::now().to_rfc3339();
    conn.execute(
        "INSERT INTO runs(kind, status, started_at, finished_at, total_rows, done_count, error_count)
         VALUES ('rieng', 'hoan_tat', ?1, ?1, 0, 0, 0)",
        rusqlite::params![ts],
    )
    .map_err(|e| AppError::new("DB_FAIL", format!("Không khởi tạo được container trạng thái: {e}")))?;
    Ok(conn.last_insert_rowid())
}

pub fn reset_row_status_at(p: &Path, row_key: &str) -> AppResult<RowDetail> {
    let csv_path = p.join("prompts.csv");
    let rows = prompts_csv::read_all(&csv_path)?;
    let mut conn =
        crate::state_db::open(p).map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    let target_job_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM jobs WHERE LOWER(row_key) = LOWER(?1) ORDER BY id DESC LIMIT 1",
            rusqlite::params![row_key],
            |r| r.get(0),
        )
        .ok()
        .or_else(|| {
            let row = prompts_csv::find_row_by_key(&rows, row_key)?;
            let latest_map = engine::load_latest_jobs_map(&conn);
            let img_counts = engine::image_counts(&rows);
            engine::latest_job_for_csv_row(&conn, row, &latest_map, &img_counts).map(|j| j.id)
        });

    prompts_csv::update_row(&csv_path, row_key, |r| {
        r.trang_thai = "Đang chờ".into();
        r.ghi_chu_loi.clear();
    })?;

    if let Some(jid) = target_job_id {
        let tx = conn
            .transaction()
            .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
        tx.execute(
            "UPDATE jobs
             SET status = 'dang_cho', image_status = 'chua_chay', video_status = 'chua_chay',
                 image_prompt_hash = NULL, video_prompt_hash = NULL,
                 video_task_id = NULL, video_download_url = NULL,
                 attempts_count = 0, last_error = NULL
             WHERE id = ?1",
            rusqlite::params![jid],
        )
        .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
        tx.commit()
            .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    }

    build_row_detail_at(p, row_key)
}

#[tauri::command]
pub fn reset_row_status(row_key: String, state: State<'_, AppState>) -> AppResult<RowDetail> {
    ensure_not_running(&state)?;
    let p = project_path(&state)?;
    reset_row_status_at(&p, &row_key)
}

pub fn skip_row_at(p: &Path, row_key: &str) -> AppResult<RowDetail> {
    let csv_path = p.join("prompts.csv");
    let rows = prompts_csv::read_all(&csv_path)?;
    let row = prompts_csv::find_row_by_key(&rows, row_key)
        .ok_or_else(|| AppError::new("ROW_NOT_FOUND", format!("Không tìm thấy dòng '{row_key}'.")))?;
    let actual_key = prompts_csv::row_key_of(
        &row.anh_tham_chieu,
        &row.promt_tao_image,
        &row.promt_tao_video,
    );
    let img_hash = engine::sha2hex(row.promt_tao_image.trim());
    let vid_hash = engine::sha2hex(row.promt_tao_video.trim());
    let row_no = row.row_no as i64;

    let mut conn =
        crate::state_db::open(p).map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    let direct_job_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM jobs WHERE LOWER(row_key) = LOWER(?1) ORDER BY id DESC LIMIT 1",
            rusqlite::params![actual_key],
            |r| r.get(0),
        )
        .ok();
    let linked_job_id: Option<i64> = if direct_job_id.is_none() {
        let latest_map = engine::load_latest_jobs_map(&conn);
        let img_counts = engine::image_counts(&rows);
        engine::latest_job_for_csv_row(&conn, row, &latest_map, &img_counts).map(|j| j.id)
    } else {
        None
    };

    prompts_csv::update_row(&csv_path, &actual_key, |r| {
        r.trang_thai = "Đang chờ".into();
        r.ghi_chu_loi = "Bị bỏ qua bởi người dùng (Bỏ qua dòng)".into();
    })?;

    let note = "Bị bỏ qua bởi người dùng";
    let tx = conn
        .transaction()
        .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;

    let mut updated = if let Some(jid) = direct_job_id {
        tx.execute(
            "UPDATE jobs SET status = 'bo_qua', last_error = ?1 WHERE id = ?2",
            rusqlite::params![note, jid],
        )
        .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?
    } else {
        0
    };

    if updated == 0 {
        if let Some(linked_id) = linked_job_id {
            updated = tx
                .execute(
                    "UPDATE OR IGNORE jobs
                     SET status = 'bo_qua', last_error = ?1, row_key = ?2
                     WHERE id = ?3",
                    rusqlite::params![note, actual_key, linked_id],
                )
                .unwrap_or(0);
            if updated == 0 {
                updated = tx
                    .execute(
                        "UPDATE jobs
                         SET status = 'bo_qua', last_error = ?1
                         WHERE id = ?2",
                        rusqlite::params![note, linked_id],
                    )
                    .unwrap_or(0);
            }
        }
    }

    if updated == 0 {
        let seed_run_id = ensure_seed_run_id(&tx)?;
        let ts = chrono::Local::now().to_rfc3339();
        tx.execute(
            "INSERT INTO jobs(run_id, row_key, row_no, status, image_status, video_status,
                              image_prompt_hash, video_prompt_hash, attempts_count, last_error, started_at, finished_at)
             VALUES (?1, ?2, ?3, 'bo_qua', 'chua_chay', 'chua_chay', ?4, ?5, 0, ?6, ?7, ?7)
             ON CONFLICT(run_id, row_key) DO UPDATE SET status = 'bo_qua', last_error = excluded.last_error",
            rusqlite::params![seed_run_id, actual_key, row_no, img_hash, vid_hash, note, ts],
        )
        .map_err(|e| AppError::new("DB_FAIL", format!("Không lưu được trạng thái bỏ qua: {e}")))?;
    }

    tx.commit()
        .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;

    build_row_detail_at(p, &actual_key)
}

#[tauri::command]
pub fn skip_row(row_key: String, state: State<'_, AppState>) -> AppResult<RowDetail> {
    ensure_not_running(&state)?;
    let p = project_path(&state)?;
    skip_row_at(&p, &row_key)
}

pub fn remap_reference_image_at(
    p: &Path,
    row_key: &str,
    new_file_name: &str,
) -> AppResult<RowDetail> {
    let trimmed_new = new_file_name.trim();
    if trimmed_new.is_empty()
        || trimmed_new.contains("..")
        || trimmed_new.contains('/')
        || trimmed_new.contains('\\')
        || trimmed_new.contains(':')
    {
        return Err(AppError::new(
            "BAD_FILE_NAME",
            "Tên file ảnh tham chiếu không hợp lệ.",
        ));
    }

    let new_path = p.join("input").join(trimmed_new);
    if !new_path.is_file() {
        return Err(AppError::new(
            "IMAGE_NOT_FOUND",
            format!("Không tìm thấy file '{trimmed_new}' trong input/."),
        ));
    }

    let csv_path = p.join("prompts.csv");
    let before_rows = prompts_csv::read_all(&csv_path)?;
    let old_row = prompts_csv::find_row_by_key(&before_rows, row_key)
        .ok_or_else(|| AppError::new("ROW_NOT_FOUND", format!("Không tìm thấy dòng '{row_key}'.")))?
        .clone();
    let old_key = prompts_csv::row_key_of(
        &old_row.anh_tham_chieu,
        &old_row.promt_tao_image,
        &old_row.promt_tao_video,
    );
    let new_key = prompts_csv::row_key_of(
        trimmed_new,
        &old_row.promt_tao_image,
        &old_row.promt_tao_video,
    );
    let old_img_low = old_row.anh_tham_chieu.trim().to_lowercase();
    let img_counts = engine::image_counts(&before_rows);
    let same_image = img_counts.get(&old_img_low).copied().unwrap_or(0);

    let mut conn =
        crate::state_db::open(p).map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    let latest_map = engine::load_latest_jobs_map(&conn);
    let prev_job = engine::latest_job_for_csv_row(&conn, &old_row, &latest_map, &img_counts);

    let mut migrate_keys: Vec<String> = vec![old_key.clone()];
    let req_trimmed = row_key.trim();
    if !req_trimmed.is_empty()
        && !migrate_keys.iter().any(|k| k.eq_ignore_ascii_case(req_trimmed))
    {
        migrate_keys.push(req_trimmed.to_string());
    }
    if let Some(ref pj) = prev_job {
        if !pj.row_key.trim().is_empty()
            && !migrate_keys.iter().any(|k| k.eq_ignore_ascii_case(&pj.row_key))
        {
            migrate_keys.push(pj.row_key.clone());
        }
    }
    if same_image == 1
        && !old_img_low.is_empty()
        && !migrate_keys.iter().any(|k| k.eq_ignore_ascii_case(&old_img_low))
    {
        migrate_keys.push(old_img_low.clone());
    }

    prompts_csv::remap_key(&csv_path, row_key, trimmed_new)?;

    let tx = conn
        .transaction()
        .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;

    if !old_key.eq_ignore_ascii_case(&new_key) {
        let keep_existing_new_key = prev_job
            .as_ref()
            .is_some_and(|pj| pj.row_key.eq_ignore_ascii_case(&new_key));
        if !keep_existing_new_key {
            // Chỉ cập nhật từ các key của chính dòng cũ -> new_key (không xóa bản ghi của dòng anh em dùng chung ảnh mới).
            // Dọn bản ghi cũ trùng đúng new_key nếu có để tránh đụng UNIQUE(run_id, row_key).
            tx.execute(
                "DELETE FROM attempts WHERE job_id IN (SELECT id FROM jobs WHERE LOWER(row_key) = LOWER(?1))",
                rusqlite::params![new_key],
            )
            .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;

            tx.execute(
                "DELETE FROM file_refs WHERE LOWER(row_key) = LOWER(?1)
                    OR job_id IN (SELECT id FROM jobs WHERE LOWER(row_key) = LOWER(?1))",
                rusqlite::params![new_key],
            )
            .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;

            tx.execute(
                "DELETE FROM jobs WHERE LOWER(row_key) = LOWER(?1)",
                rusqlite::params![new_key],
            )
            .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
        }
    }

    for mk in &migrate_keys {
        crate::state_db::migrate_jobs_row_key(&tx, mk, &new_key)
            .map_err(|e| AppError::new("DB_FAIL", format!("Không đổi được row_key trong jobs: {e}")))?;
        crate::state_db::migrate_file_refs_row_key(&tx, mk, &new_key, None).map_err(|e| {
            AppError::new(
                "DB_FAIL",
                format!("Không đổi được row_key trong file_refs: {e}"),
            )
        })?;
        tx.execute(
            "UPDATE logs SET row_key = ?1 WHERE LOWER(row_key) = LOWER(?2) AND row_key != ?1",
            rusqlite::params![new_key, mk],
        )
        .ok();
    }

    tx.commit()
        .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;

    build_row_detail_at(p, &new_key)
}

#[tauri::command]
pub fn remap_reference_image(
    row_key: String,
    new_file_name: String,
    state: State<'_, AppState>,
) -> AppResult<RowDetail> {
    ensure_not_running(&state)?;
    let p = project_path(&state)?;
    remap_reference_image_at(&p, &row_key, &new_file_name)
}

pub fn copy_picked_image_into_input(input_dir: &Path, chosen: &Path) -> AppResult<String> {
    let ext_ok = chosen
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|s| matches!(s.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg" | "webp"));
    if !ext_ok {
        return Err(AppError::new(
            "BAD_IMAGE_EXT",
            "Chỉ hỗ trợ định dạng ảnh .png, .jpg, .jpeg, .webp.",
        ));
    }

    let file_name = chosen
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| AppError::new("BAD_FILE_NAME", "Không xác định được tên file ảnh."))?;

    let dest = input_dir.join(&file_name);
    let same_file = match (std::fs::canonicalize(chosen), std::fs::canonicalize(&dest)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };

    if !same_file {
        // [P0-4]: Tuyệt đối KHÔNG ghi đè file cùng tên đã có sẵn trong `input/`!
        if dest.exists() {
            return Err(AppError::new(
                "INPUT_FILE_EXISTS",
                "Trong input/ đã có sẵn file trùng tên — hãy đổi tên ảnh trước khi chọn để không ghi đè dòng khác.",
            ));
        }
        std::fs::copy(chosen, &dest).map_err(|e| {
            AppError::new(
                "COPY_IMAGE_FAIL",
                format!("Không sao chép được ảnh vào thư mục input/: {e}"),
            )
        })?;
    }

    Ok(file_name)
}

#[tauri::command]
pub async fn pick_image_in_input(
    app: AppHandle,
    state: State<'_, AppState>,
) -> AppResult<Option<String>> {
    ensure_not_running(&state)?;
    let p = project_path(&state)?;
    let input_dir = p.join("input");
    std::fs::create_dir_all(&input_dir).map_err(|e| {
        AppError::new(
            "CREATE_DIR_FAIL",
            format!("Không tạo được thư mục input/: {e}"),
        )
    })?;

    let input_for_dialog = input_dir.clone();
    let chosen_opt = tauri::async_runtime::spawn_blocking(move || {
        let (tx, rx) = std::sync::mpsc::channel::<Option<PathBuf>>();
        app.dialog()
            .file()
            .set_title("Chọn ảnh tham chiếu (.png, .jpg, .jpeg, .webp)")
            .set_directory(&input_for_dialog)
            .add_filter("Ảnh tham chiếu", &["png", "jpg", "jpeg", "webp"])
            .pick_file(move |f| {
                let pb = f.and_then(|fp| fp.into_path().ok());
                let _ = tx.send(pb);
            });
        rx.recv().ok().flatten()
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAIL", format!("{e}")))?;

    let Some(chosen) = chosen_opt else {
        return Ok(None);
    };

    ensure_not_running(&state)?;
    copy_picked_image_into_input(&input_dir, &chosen).map(Some)
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

pub fn delete_row_at(p: &Path, row_key: &str) -> AppResult<bool> {
    let csv_path = p.join("prompts.csv");
    let rows = prompts_csv::read_all(&csv_path)?;
    let row = prompts_csv::find_row_by_key(&rows, row_key)
        .ok_or_else(|| AppError::new("ROW_NOT_FOUND", format!("Không tìm thấy dòng '{row_key}'.")))?
        .clone();
    let actual_key = prompts_csv::row_key_of(
        &row.anh_tham_chieu,
        &row.promt_tao_image,
        &row.promt_tao_video,
    );
    let img_counts = engine::image_counts(&rows);
    let legacy_low = row.anh_tham_chieu.trim().to_lowercase();
    let same_image = img_counts.get(&legacy_low).copied().unwrap_or(0);

    let mut conn =
        crate::state_db::open(p).map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    let latest_map = engine::load_latest_jobs_map(&conn);
    let prev_job = engine::latest_job_for_csv_row(&conn, &row, &latest_map, &img_counts);

    let mut related_keys: Vec<String> = Vec::new();
    let mut push_key = |k: &str| {
        let trimmed = k.trim();
        if !trimmed.is_empty() && !related_keys.iter().any(|x| x.eq_ignore_ascii_case(trimmed)) {
            related_keys.push(trimmed.to_string());
        }
    };
    push_key(&actual_key);
    push_key(row_key);
    if let Some(ref pj) = prev_job {
        push_key(&pj.row_key);
    }

    for media_name in [row.anh_da_tao.trim(), row.video_da_tao.trim()] {
        if !media_name.is_empty() {
            if let Ok(mut stmt) = conn.prepare(
                "SELECT DISTINCT row_key FROM file_refs WHERE LOWER(file_name) = LOWER(?1)",
            ) {
                if let Ok(iter) = stmt.query_map(rusqlite::params![media_name], |r| r.get::<_, String>(0)) {
                    for rk in iter.flatten() {
                        push_key(&rk);
                    }
                }
            }
        }
    }

    if same_image == 1 && !legacy_low.is_empty() {
        push_key(&legacy_low);
        let prefix_like = format!("{}#%", escape_sqlite_like(&legacy_low));
        for sql in [
            "SELECT DISTINCT row_key FROM jobs WHERE LOWER(row_key) LIKE LOWER(?1) ESCAPE '\\'",
            "SELECT DISTINCT row_key FROM file_refs WHERE LOWER(row_key) LIKE LOWER(?1) ESCAPE '\\'",
        ] {
            if let Ok(mut stmt) = conn.prepare(sql) {
                if let Ok(iter) = stmt.query_map(rusqlite::params![prefix_like], |r| r.get::<_, String>(0)) {
                    for rk in iter.flatten() {
                        push_key(&rk);
                    }
                }
            }
        }
    }

    prompts_csv::delete_row(&csv_path, row_key)?;

    let tx = conn
        .transaction()
        .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;

    let mut job_ids: Vec<i64> = Vec::new();
    {
        let mut stmt = tx
            .prepare("SELECT id FROM jobs WHERE LOWER(row_key) = LOWER(?1)")
            .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
        for rk in &related_keys {
            let iter = stmt
                .query_map(rusqlite::params![rk], |r| r.get::<_, i64>(0))
                .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
            for jid in iter {
                let id = jid.map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
                if !job_ids.contains(&id) {
                    job_ids.push(id);
                }
            }
        }
    }

    for jid in &job_ids {
        tx.execute("DELETE FROM attempts WHERE job_id = ?1", rusqlite::params![jid])
            .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
        tx.execute("DELETE FROM file_refs WHERE job_id = ?1", rusqlite::params![jid])
            .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    }
    for rk in &related_keys {
        tx.execute(
            "DELETE FROM file_refs WHERE LOWER(row_key) = LOWER(?1)",
            rusqlite::params![rk],
        )
        .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    }
    for jid in &job_ids {
        tx.execute("DELETE FROM jobs WHERE id = ?1", rusqlite::params![jid])
            .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    }
    for rk in &related_keys {
        tx.execute(
            "DELETE FROM jobs WHERE LOWER(row_key) = LOWER(?1)",
            rusqlite::params![rk],
        )
        .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    }

    tx.commit()
        .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    Ok(true)
}

#[tauri::command]
pub fn delete_row(row_key: String, state: State<'_, AppState>) -> AppResult<bool> {
    ensure_not_running(&state)?;
    let p = project_path(&state)?;
    delete_row_at(&p, &row_key)
}

pub fn save_row_prompts_at(
    p: &Path,
    row_key: &str,
    prompt_image: &str,
    prompt_video: &str,
) -> AppResult<RowDetail> {
    let trimmed_img = prompt_image.trim();
    let trimmed_vid = prompt_video.trim();
    let csv_path = p.join("prompts.csv");

    // Khoá dòng chứa hash prompt → sửa prompt là khoá đổi. Phải biết tên ảnh TRƯỚC khi
    // ghi để dựng khoá mới và để tra bản ghi job cũ (khoá cũ).
    let before = prompts_csv::read_all(&csv_path)?;
    let old_row = prompts_csv::find_row_by_key(&before, row_key)
        .ok_or_else(|| AppError::new("ROW_NOT_FOUND", format!("Không tìm thấy dòng '{row_key}'.")))?;
    let image_name = old_row.anh_tham_chieu.clone();

    // Trạng thái "Bỏ qua" là quyết định của người dùng về DÒNG, không về prompt →
    // phải mang theo khi khoá đổi vì prompt sửa. Cột trạng thái trong CSV cũng vậy.
    let was_skipped = old_row.trang_thai.trim() == "Bỏ qua";
    let old_trang_thai = old_row.trang_thai.clone();
    let old_note = old_row.ghi_chu_loi.clone();

    prompts_csv::save_prompts(&csv_path, row_key, trimmed_img, trimmed_vid)?;
    if was_skipped {
        // save_prompts chỉ sửa cột B/C nên trạng thái vẫn còn; không cần ghi lại.
        let _ = (&old_trang_thai, &old_note);
    }

    let new_key = prompts_csv::row_key_of(&image_name, trimmed_img, trimmed_vid);
    let mut conn =
        crate::state_db::open(p).map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;

    let old_key = prompts_csv::row_key_of(
        &old_row.anh_tham_chieu,
        &old_row.promt_tao_image,
        &old_row.promt_tao_video,
    );
    let image_low = image_name.trim().to_lowercase();
    let img_counts = engine::image_counts(&before);
    let same_image = img_counts
        .get(&image_low)
        .copied()
        .unwrap_or(0);
    let latest_map = engine::load_latest_jobs_map(&conn);
    let prev_job = engine::latest_job_for_csv_row(&conn, old_row, &latest_map, &img_counts);

    let tx = conn
        .transaction()
        .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;

    // Di trú bản ghi job "chưa sinh sản phẩm" của CHÍNH dòng này sang khoá mới.
    // Cập nhật prompt hashes trước, rồi dùng migrate_jobs_row_key để tránh đụng UNIQUE(run_id, row_key).
    // Chỉ khớp chính xác `old_key` (và fallback tên ảnh trần khi `same_image == 1`);
    // TUYỆT ĐỐI không dùng `LIKE image_low || '#%'` để tránh cướp/xóa dòng anh em dùng chung ảnh.
    let untouched_updated = if same_image == 1 {
        tx.execute(
            "UPDATE jobs
             SET image_prompt_hash = ?1, video_prompt_hash = ?2
             WHERE (LOWER(row_key) = LOWER(?3) OR LOWER(row_key) = LOWER(?4))
               AND status IN ('dang_cho', 'bo_qua')
               AND attempts_count = 0
               AND image_status = 'chua_chay'
               AND video_status = 'chua_chay'",
            rusqlite::params![
                engine::sha2hex(trimmed_img),
                engine::sha2hex(trimmed_vid),
                old_key,
                image_low
            ],
        )
        .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?
    } else {
        tx.execute(
            "UPDATE jobs
             SET image_prompt_hash = ?1, video_prompt_hash = ?2
             WHERE LOWER(row_key) = LOWER(?3)
               AND status IN ('dang_cho', 'bo_qua')
               AND attempts_count = 0
               AND image_status = 'chua_chay'
               AND video_status = 'chua_chay'",
            rusqlite::params![
                engine::sha2hex(trimmed_img),
                engine::sha2hex(trimmed_vid),
                old_key
            ],
        )
        .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?
    };

    // Với các bản ghi job đã từng chạy và file_refs của chính dòng này: cập nhật `row_key` sang `new_key`
    // một cách an toàn khi va chạm UNIQUE, GIỮ NGUYÊN `image_prompt_hash` / `video_prompt_hash` lịch sử
    // để cờ `prompt_modified` tiếp tục phát hiện chính xác prompt đã sửa.
    if !old_key.eq_ignore_ascii_case(&new_key) {
        let mut migrate_keys = vec![old_key.clone()];
        if let Some(ref pj) = prev_job {
            if !migrate_keys.iter().any(|k| k.eq_ignore_ascii_case(&pj.row_key)) {
                migrate_keys.push(pj.row_key.clone());
            }
        }
        if same_image == 1 && !image_low.is_empty() && !migrate_keys.iter().any(|k| k.eq_ignore_ascii_case(&image_low)) {
            migrate_keys.push(image_low.clone());
        }
        for mk in &migrate_keys {
            crate::state_db::migrate_jobs_row_key(&tx, mk, &new_key)
                .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
            crate::state_db::migrate_file_refs_row_key(&tx, mk, &new_key, None)
                .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
        }
    } else if same_image == 1 && untouched_updated > 0 && !image_low.is_empty() && !image_low.eq_ignore_ascii_case(&new_key) {
        crate::state_db::migrate_jobs_row_key(&tx, &image_low, &new_key)
            .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    }

    tx.commit()
        .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    build_row_detail_at(p, &new_key)
}

#[tauri::command]
pub fn save_row_prompts(
    row_key: String,
    prompt_image: String,
    prompt_video: String,
    state: State<'_, AppState>,
) -> AppResult<RowDetail> {
    ensure_not_running(&state)?;
    let p = project_path(&state)?;
    save_row_prompts_at(&p, &row_key, &prompt_image, &prompt_video)
}

// ━━ Commands: Cài đặt, API Key, Khay hệ thống & Đóng ứng dụng ━━

/// Kiểm tra số luồng chạy đồng thời hợp lệ theo nguồn video và số tài khoản Muse khả dụng:
/// - Luôn trong khoảng `1..=MAX_CONCURRENCY`.
/// - Khi `video_source == "internal"`, số luồng KHÔNG được vượt quá số tài khoản Muse khả dụng
///   (tối thiểu 1 để vẫn lưu được cấu hình trước khi thêm tài khoản) nhằm bảo đảm mỗi tài khoản
///   chỉ chạy tối đa 1 luồng video duy nhất.
pub fn validate_settings_concurrency(cfg: &Settings, muse_video_accounts: usize) -> AppResult<()> {
    if cfg.concurrency < 1 || cfg.concurrency > settings::MAX_CONCURRENCY {
        return Err(AppError::new(
            "INVALID_CONCURRENCY",
            format!(
                "Số luồng chạy đồng thời phải từ 1 đến {}.",
                settings::MAX_CONCURRENCY
            ),
        ));
    }
    if cfg.video_source == "internal" {
        let max_allowed = (muse_video_accounts.max(1) as u32).min(settings::MAX_CONCURRENCY);
        if cfg.concurrency > max_allowed {
            let msg = if muse_video_accounts == 0 {
                "Chưa có tài khoản Muse khả dụng — chỉ cho phép 1 luồng cho đến khi thêm tài khoản Muse.".to_string()
            } else {
                format!(
                    "Số luồng chạy đồng thời ({}) không được vượt quá số tài khoản Muse khả dụng ({}) để đảm bảo mỗi tài khoản chỉ chạy tối đa 1 video.",
                    cfg.concurrency, muse_video_accounts
                )
            };
            return Err(AppError::new("CONCURRENCY_EXCEEDS_ACCOUNTS", msg));
        }
    }
    Ok(())
}

#[tauri::command]
pub fn get_settings(state: State<'_, AppState>) -> SettingsView {
    let s = state.settings_cache.read().clone();
    let muse_dir = settings::app_data_dir().join(crate::muse_sidecar::MUSE_DATA_DIR);
    let (muse_total_accounts, muse_video_accounts) =
        crate::muse_sidecar::count_accounts_from_disk(&muse_dir);
    let ffmpeg_resolved = crate::ffmpeg::resolve_ffmpeg_binary(&s.ffmpeg_path).ok();
    let prompt_key = settings::prompt_key(&s.endpoint_prompt_base_url);
    SettingsView {
        has_prompt_key: prompt_key.is_some(),
        prompt_key_preview: prompt_key.as_deref().and_then(settings::mask_api_key),
        settings: s,
        image_key_preview: settings::key_preview("anh"),
        video_key_preview: settings::key_preview("video"),
        has_image_key: settings::has_api_key("anh"),
        has_video_key: settings::has_api_key("video"),
        muse_total_accounts,
        muse_video_accounts,
        max_concurrency: settings::MAX_CONCURRENCY,
        ffmpeg_available: ffmpeg_resolved.is_some(),
        ffmpeg_resolved_path: ffmpeg_resolved.map(|p| p.to_string_lossy().to_string()),
    }
}

#[tauri::command]
pub async fn check_ffmpeg(
    override_path: Option<String>,
    state: State<'_, AppState>,
) -> AppResult<crate::ffmpeg::FfmpegStatus> {
    let path_str = match override_path {
        Some(p) => p,
        None => state.settings_cache.read().ffmpeg_path.clone(),
    };
    let status = tauri::async_runtime::spawn_blocking(move || {
        crate::ffmpeg::check_ffmpeg_status(&path_str)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAIL", format!("Không kiểm tra được FFmpeg: {e}")))?;
    Ok(status)
}

#[tauri::command]
pub async fn pick_ffmpeg_binary(app: AppHandle) -> Option<String> {
    tauri::async_runtime::spawn_blocking(move || {
        let (tx, rx) = std::sync::mpsc::channel::<Option<String>>();
        let mut dlg = app
            .dialog()
            .file()
            .set_title("Chọn file thực thi FFmpeg (ffmpeg.exe)");
        #[cfg(windows)]
        {
            dlg = dlg.add_filter("FFmpeg Executable", &["exe"]);
        }
        dlg.pick_file(move |file| {
            let chosen = file.map(|fp| {
                fp.into_path()
                    .map(|pb| clean_abs_path_string(&pb))
                    .unwrap_or_else(|e| strip_verbatim_prefix(e.to_string()))
            });
            let _ = tx.send(chosen);
        });
        rx.recv().ok().flatten()
    })
    .await
    .ok()
    .flatten()
}

#[tauri::command]
pub fn save_settings(mut settings: Settings, prompt_key: Option<String>, state: State<'_, AppState>) -> AppResult<bool> {
    let muse_dir = settings::app_data_dir().join(crate::muse_sidecar::MUSE_DATA_DIR);
    let (_, muse_video_accounts) = crate::muse_sidecar::count_accounts_from_disk(&muse_dir);
    validate_settings_concurrency(&settings, muse_video_accounts)?;
    settings.endpoint_prompt_base_url = settings.endpoint_prompt_base_url.trim().trim_end_matches('/').into();
    settings.endpoint_prompt_model = settings.endpoint_prompt_model.trim().into();
    settings.reels_country = settings.reels_country.trim().to_ascii_uppercase();
    if !settings.endpoint_prompt_base_url.is_empty() {
        settings::prompt_origin(&settings.endpoint_prompt_base_url)?;
    }
    if !matches!(settings.prompt_source.as_str(), "gateway" | "internal")
        || !(5..=3600).contains(&settings.prompt_timeout_s)
        || [settings.reels_min_likes,settings.reels_min_views].into_iter().flatten().any(|n| n > i32::MAX as u32)
        || !(settings.reels_country.is_empty() || (settings.reels_country.len() == 2 && settings.reels_country.bytes().all(|c| c.is_ascii_uppercase()))) {
        return Err(AppError::new("BAD_RESEARCH_SETTINGS","Kiểm tra cấu hình Reels và endpoint prompt."));
    }

    let start_with_windows = {
        let mut cur = state.settings_cache.write();
        let recent = cur.recent_projects.clone();
        let current = cur.current_project.clone();
        let mut s = settings;
        s.recent_projects = recent;
        s.current_project = current;
        let old_key = prompt_key.as_ref().and_then(|_| settings::prompt_key(&s.endpoint_prompt_base_url));
        if let Some(key) = &prompt_key {
            settings::set_prompt_key(&s.endpoint_prompt_base_url, key)?;
        }
        if let Err(error) = state.settings.save(&s) {
            if prompt_key.is_some() {
                settings::set_prompt_key(&s.endpoint_prompt_base_url, old_key.as_deref().unwrap_or(""))?;
            }
            return Err(error);
        }
        let loaded = state.settings.load();
        let autostart = loaded.start_with_windows;
        *cur = loaded;
        autostart
    };
    // Nhả lock settings_cache trước khi gọi reg.exe để không chặn các command khác.
    if let Err(e) = settings::apply_windows_autostart(start_with_windows) {
        log_info_app(&format!("Cảnh báo cập nhật autostart Windows: {e}"));
    }
    Ok(true)
}

#[tauri::command]
pub fn set_api_key_cmd(which: String, value: String) -> AppResult<bool> {
    settings::set_api_key(&which, &value)?;
    Ok(true)
}

#[tauri::command]
pub fn hide_to_tray(app: AppHandle) -> AppResult<()> {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.hide();
    }
    Ok(())
}

const SHUTDOWN_ENGINE_WAIT_TIMEOUT: Duration = Duration::from_millis(2500);

pub(crate) fn shutdown_app_state_with<FQuit>(
    state: &AppState,
    stop_first: bool,
    wait_timeout: Duration,
    quit_sidecar: FQuit,
) where
    FQuit: FnOnce(),
{
    state.exit_requested.store(true, Ordering::SeqCst);
    if stop_first {
        if let Some(flag) = state.stop_flag.read().clone() {
            flag.store(true, Ordering::SeqCst);
        }
    }
    let handle_opt = state.engine_thread.lock().take();
    if let Some(handle) = handle_opt {
        let deadline = Instant::now() + wait_timeout;
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if handle.is_finished() {
            let _ = handle.join();
        }
    }
    if let Ok(p) = project_path(state) {
        if let Ok(conn) = crate::state_db::open(&p) {
            let _ = crate::state_db::recover_interrupted_runs(&conn);
        }
    }
    quit_sidecar();
}

pub(crate) fn shutdown_app_state(state: &AppState, stop_first: bool) {
    shutdown_app_state_with(
        state,
        stop_first,
        SHUTDOWN_ENGINE_WAIT_TIMEOUT,
        crate::muse_sidecar::stop_for_quit,
    );
}

#[tauri::command]
pub fn force_quit_app(
    stop_first: bool,
    app: AppHandle,
    state: State<'_, AppState>,
) -> AppResult<()> {
    shutdown_app_state(&state, stop_first);
    app.exit(0);
    Ok(())
}

#[tauri::command]
pub async fn test_connection(
    which: String,
    source: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    state: State<'_, AppState>,
) -> AppResult<(bool, u64, String)> {
    let cfg = state.settings_cache.read().clone();
    tauri::async_runtime::spawn_blocking(move || {
        if which == "prompt" {
            return research::test_prompt(&cfg, source.as_deref(), base_url.as_deref(), model.as_deref(), api_key.as_deref());
        }
        crate::adapters::test_connection(&cfg, &which, base_url.as_deref(), api_key.as_deref())
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAIL", format!("{e}")))?
}

#[tauri::command]
pub async fn list_models(
    which: String,
    base_url: Option<String>,
    api_key: Option<String>,
    state: State<'_, AppState>,
) -> AppResult<Vec<String>> {
    let cfg = state.settings_cache.read().clone();
    tauri::async_runtime::spawn_blocking(move || {
        if which == "prompt" {
            settings::prompt_origin(base_url.as_deref().unwrap_or(&cfg.endpoint_prompt_base_url))?;
        }
        crate::adapters::list_models(&cfg, &which, base_url.as_deref(), api_key.as_deref())
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAIL", format!("{e}")))?
}

// ━━ Commands: Cập nhật ứng dụng (auto-update qua GitHub Releases) ━━

/// Phiên bản app hiện tại lấy từ `tauri.conf.json` (package_info) — không hardcode.
#[tauri::command]
pub fn app_version(app: AppHandle) -> String {
    app.package_info().version.to_string()
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateInfo {
    pub available: bool,
    pub current_version: String,
    pub version: Option<String>,
    pub notes: Option<String>,
    pub date: Option<String>,
}

/// Kiểm tra bản mới trên endpoint updater (`latest.json` của GitHub Release).
/// Không báo lỗi chặn UI khi mạng hỏng: trả `available: false` kèm `notes` mô tả lỗi.
#[tauri::command]
pub async fn check_app_update(app: AppHandle) -> AppResult<UpdateInfo> {
    use tauri_plugin_updater::UpdaterExt;

    let current = app.package_info().version.to_string();
    let updater = app
        .updater()
        .map_err(|e| AppError::new("UPDATE_INIT_FAIL", format!("Không khởi tạo updater: {e}")))?;

    match updater.check().await {
        Ok(Some(update)) => Ok(UpdateInfo {
            available: true,
            current_version: current,
            version: Some(update.version.clone()),
            notes: update.body.clone(),
            date: update.date.map(|d| d.to_string()),
        }),
        Ok(None) => Ok(UpdateInfo {
            available: false,
            current_version: current,
            version: None,
            notes: None,
            date: None,
        }),
        Err(e) => Ok(UpdateInfo {
            available: false,
            current_version: current,
            version: None,
            notes: Some(format!("Không kiểm tra được bản cập nhật: {e}")),
            date: None,
        }),
    }
}

/// Tải + cài bản cập nhật, phát tiến độ qua event `update_progress`
/// (`{ downloaded, total, percent }`), rồi khởi động lại app.
#[tauri::command]
pub async fn install_app_update(
    app: AppHandle,
    state: State<'_, AppState>,
) -> AppResult<bool> {
    use tauri_plugin_updater::UpdaterExt;

    if state.exit_requested.load(Ordering::SeqCst) {
        return Err(app_exiting_error());
    }
    if state.running.load(Ordering::SeqCst) {
        return Err(AppError::new(
            "RUN_IN_PROGRESS",
            "Đang chạy pipeline — dừng (STOP) rồi mới cập nhật để không mất tiến độ.",
        ));
    }

    let updater = app
        .updater()
        .map_err(|e| AppError::new("UPDATE_INIT_FAIL", format!("Không khởi tạo updater: {e}")))?;
    let update = updater
        .check()
        .await
        .map_err(|e| AppError::new("UPDATE_CHECK_FAIL", format!("Không kiểm tra được bản mới: {e}")))?
        .ok_or_else(|| {
            AppError::new("UPDATE_NOT_FOUND", "Bạn đang dùng phiên bản mới nhất.")
        })?;

    let emitter = app.clone();
    let mut downloaded: u64 = 0;
    update
        .download_and_install(
            move |chunk_len, total| {
                downloaded += chunk_len as u64;
                let percent = total
                    .filter(|t| *t > 0)
                    .map(|t| (downloaded.saturating_mul(100) / t).min(100) as u32);
                let _ = emitter.emit(
                    "update_progress",
                    serde_json::json!({
                        "downloaded": downloaded,
                        "total": total,
                        "percent": percent,
                    }),
                );
            },
            || {},
        )
        .await
        .map_err(|e| {
            AppError::new("UPDATE_INSTALL_FAIL", format!("Cài bản cập nhật thất bại: {e}"))
        })?;

    finish_app_update_restart_with(
        &state,
        || {
            log_info_app("Đã cài bản cập nhật — khởi động lại ứng dụng");
            // Dừng engine trước khi thoát để không để lại Edge mồ côi giữ profile.
            crate::muse_sidecar::stop_for_quit();
        },
        || app.restart(),
    )
}

fn log_info_app(msg: &str) {
    crate::muse_sidecar::log_app_event(msg);
}

#[cfg(test)]
mod tests;
