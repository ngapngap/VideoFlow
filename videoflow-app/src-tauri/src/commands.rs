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
    pub has_image_key: bool,
    pub has_video_key: bool,
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

    // Đọc thông tin tài khoản đã sử dụng (UI-3)
    let mut account_used = None;
    let acc_file = crate::settings::app_data_dir()
        .join(crate::muse_sidecar::MUSE_DATA_DIR)
        .join("accounts.json");
    if let Ok(content) = std::fs::read_to_string(&acc_file) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(arr) = val.as_array() {
                if let Some(acc) = arr
                    .iter()
                    .find(|a| a.get("status").and_then(|s| s.as_str()) == Some("active"))
                    .or_else(|| arr.first())
                {
                    let id = acc.get("id").and_then(|s| s.as_str()).unwrap_or("");
                    let label = acc.get("label").and_then(|s| s.as_str()).unwrap_or("");
                    if !label.is_empty() && !id.is_empty() && label != id {
                        account_used = Some(format!("{label} ({id})"));
                    } else if !label.is_empty() {
                        account_used = Some(label.to_string());
                    } else if !id.is_empty() {
                        account_used = Some(id.to_string());
                    }
                }
            }
        }
    }

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

    let tx = conn
        .transaction()
        .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    for rk in &related_keys {
        tx.execute(
            "UPDATE file_refs SET is_current = 0, row_key = ?1
             WHERE LOWER(row_key) = LOWER(?2) AND kind = ?3",
            rusqlite::params![actual_key, rk, db_kind],
        )
        .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    }
    tx.execute(
        "UPDATE file_refs SET is_current = 1, row_key = ?1 WHERE id = ?2",
        rusqlite::params![actual_key, target_ref_id],
    )
    .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    tx.commit()
        .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;

    prompts_csv::set_active_media_file(&csv_path, &actual_key, csv_kind, &file_name)?;
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

pub(crate) fn spawn_engine_internal(
    app: AppHandle,
    state: &AppState,
    engine_cfg: EngineConfig,
) -> AppResult<i64> {
    state
        .running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .map_err(|_| {
            AppError::new(
                "RUN_IN_PROGRESS",
                "Lượt chạy đang thực hiện - Stop hoặc chờ chạy xong (6.12).",
            )
        })?;

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

        // Dọn stop_flag & running = false TRƯỚC khi phát run_finished cho nhánh lỗi/panic
        // để UI nhận sự kiện run_finished có thể khởi động lượt mới ngay mà không bị RUN_IN_PROGRESS.
        let mut deferred_finish: Option<serde_json::Value> = None;

        match result {
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
            }
            Ok(Err(e)) => {
                let rid = created_rid_bg.load(Ordering::SeqCst);
                if rid > 0 {
                    if let Ok(conn) = crate::state_db::open(&p) {
                        let ts_now = chrono::Local::now().to_rfc3339();
                        let _ = conn.execute(
                            "UPDATE runs SET status = 'gian_doan', finished_at = ?1 WHERE id = ?2 AND status = 'dang_chay'",
                            rusqlite::params![ts_now, rid],
                        );
                    }
                }
                let _ = app2.emit(
                    "run_error",
                    serde_json::json!({
                        "run_id": (rid > 0).then_some(rid), "row_key": null, "step": null,
                        "code": e.code, "message": e.message
                    }),
                );
                deferred_finish = Some(serde_json::json!({
                    "run_id": rid,
                    "status": "gian_doan",
                    "done_count": 0,
                    "error_count": 0,
                    "duration_ms": start_inst.elapsed().as_millis() as i64
                }));
            }
            Err(panic_err) => {
                let rid = created_rid_bg.load(Ordering::SeqCst);
                if rid > 0 {
                    if let Ok(conn) = crate::state_db::open(&p) {
                        let ts_now = chrono::Local::now().to_rfc3339();
                        let _ = conn.execute(
                            "UPDATE runs SET status = 'gian_doan', finished_at = ?1 WHERE id = ?2 AND status = 'dang_chay'",
                            rusqlite::params![ts_now, rid],
                        );
                    }
                }
                let msg = panic_err
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic_err.downcast_ref::<&str>().map(|s| (*s).to_string()))
                    .unwrap_or_else(|| "Engine gặp lỗi nội bộ ngoài dự kiến".to_string());
                let _ = app2.emit(
                    "run_error",
                    serde_json::json!({
                        "run_id": (rid > 0).then_some(rid), "row_key": null, "step": null,
                        "code": "ENGINE_PANIC", "message": msg
                    }),
                );
                deferred_finish = Some(serde_json::json!({
                    "run_id": rid,
                    "status": "gian_doan",
                    "done_count": 0,
                    "error_count": 0,
                    "duration_ms": start_inst.elapsed().as_millis() as i64
                }));
            }
        }

        let st = app2.state::<AppState>();
        *st.stop_flag.write() = None;
        st.active_run_id.store(0, Ordering::SeqCst);
        st.running.store(false, Ordering::SeqCst);
        if let Some(payload) = deferred_finish {
            let _ = app2.emit("run_finished", payload);
        }
    });

    *state.engine_thread.lock() = Some(handle);
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
    let conn =
        crate::state_db::open(p).map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    let mut updated = conn
        .execute(
            "UPDATE jobs
             SET status = 'dang_cho', image_status = 'chua_chay', video_status = 'chua_chay',
                 image_prompt_hash = NULL, video_prompt_hash = NULL,
                 video_task_id = NULL, video_download_url = NULL,
                 attempts_count = 0, last_error = NULL
             WHERE id = (SELECT id FROM jobs WHERE LOWER(row_key) = LOWER(?1) ORDER BY id DESC LIMIT 1)",
            rusqlite::params![row_key],
        )
        .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;

    if updated == 0 {
        if let Ok(rows) = prompts_csv::read_all(&p.join("prompts.csv")) {
            if let Some(row) = prompts_csv::find_row_by_key(&rows, row_key) {
                let latest_map = engine::load_latest_jobs_map(&conn);
                let img_counts = engine::image_counts(&rows);
                if let Some(linked) =
                    engine::latest_job_for_csv_row(&conn, row, &latest_map, &img_counts)
                {
                    updated = conn
                        .execute(
                            "UPDATE jobs
                             SET status = 'dang_cho', image_status = 'chua_chay', video_status = 'chua_chay',
                                 image_prompt_hash = NULL, video_prompt_hash = NULL,
                                 video_task_id = NULL, video_download_url = NULL,
                                 attempts_count = 0, last_error = NULL
                             WHERE id = ?1",
                            rusqlite::params![linked.id],
                        )
                        .unwrap_or(0);
                }
            }
        }
    }
    let _ = updated;

    prompts_csv::update_row(&p.join("prompts.csv"), row_key, |r| {
        r.trang_thai = "Đang chờ".into();
        r.ghi_chu_loi.clear();
    })?;
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

    let conn =
        crate::state_db::open(p).map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    let note = "Bị bỏ qua bởi người dùng";

    let mut updated = conn
        .execute(
            "UPDATE jobs
             SET status = 'bo_qua', last_error = ?1
             WHERE id = (SELECT id FROM jobs WHERE LOWER(row_key) = LOWER(?2) ORDER BY id DESC LIMIT 1)",
            rusqlite::params![note, actual_key],
        )
        .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;

    if updated == 0 {
        let latest_map = engine::load_latest_jobs_map(&conn);
        let img_counts = engine::image_counts(&rows);
        if let Some(linked) = engine::latest_job_for_csv_row(&conn, row, &latest_map, &img_counts) {
            updated = conn
                .execute(
                    "UPDATE OR IGNORE jobs
                     SET status = 'bo_qua', last_error = ?1, row_key = ?2
                     WHERE id = ?3",
                    rusqlite::params![note, actual_key, linked.id],
                )
                .unwrap_or(0);
            if updated == 0 {
                updated = conn
                    .execute(
                        "UPDATE jobs
                         SET status = 'bo_qua', last_error = ?1
                         WHERE id = ?2",
                        rusqlite::params![note, linked.id],
                    )
                    .unwrap_or(0);
            }
        }
    }

    if updated == 0 {
        let seed_run_id = ensure_seed_run_id(&conn)?;
        let ts = chrono::Local::now().to_rfc3339();
        conn.execute(
            "INSERT INTO jobs(run_id, row_key, row_no, status, image_status, video_status,
                              image_prompt_hash, video_prompt_hash, attempts_count, last_error, started_at, finished_at)
             VALUES (?1, ?2, ?3, 'bo_qua', 'chua_chay', 'chua_chay', ?4, ?5, 0, ?6, ?7, ?7)
             ON CONFLICT(run_id, row_key) DO UPDATE SET status = 'bo_qua', last_error = excluded.last_error",
            rusqlite::params![seed_run_id, actual_key, row_no, img_hash, vid_hash, note, ts],
        )
        .map_err(|e| AppError::new("DB_FAIL", format!("Không lưu được trạng thái bỏ qua: {e}")))?;
    }

    prompts_csv::update_row(&csv_path, &actual_key, |r| {
        r.trang_thai = "Đang chờ".into();
        r.ghi_chu_loi = "Bị bỏ qua bởi người dùng (Bỏ qua dòng)".into();
    })?;

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

    prompts_csv::remap_key(&csv_path, row_key, trimmed_new)?;

    if !old_key.eq_ignore_ascii_case(&new_key) {
        let mut conn =
            crate::state_db::open(p).map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
        let tx = conn
            .transaction()
            .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;

        // Chỉ cập nhật từ old_key -> new_key (không xóa bản ghi của dòng anh em dùng chung ảnh mới).
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

        tx.execute(
            "UPDATE jobs SET row_key = ?1 WHERE LOWER(row_key) = LOWER(?2) OR LOWER(row_key) = LOWER(?3)",
            rusqlite::params![new_key, old_key, row_key],
        )
        .map_err(|e| AppError::new("DB_FAIL", format!("Không đổi được row_key trong jobs: {e}")))?;

        tx.execute(
            "UPDATE file_refs SET row_key = ?1 WHERE LOWER(row_key) = LOWER(?2) OR LOWER(row_key) = LOWER(?3)",
            rusqlite::params![new_key, old_key, row_key],
        )
        .map_err(|e| {
            AppError::new(
                "DB_FAIL",
                format!("Không đổi được row_key trong file_refs: {e}"),
            )
        })?;

        tx.execute(
            "UPDATE logs SET row_key = ?1 WHERE LOWER(row_key) = LOWER(?2) OR LOWER(row_key) = LOWER(?3)",
            rusqlite::params![new_key, old_key, row_key],
        )
        .ok();

        tx.commit()
            .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    } else if let Ok(conn) = crate::state_db::open(p) {
        let _ = conn.execute(
            "UPDATE jobs SET row_key = ?1 WHERE LOWER(row_key) = LOWER(?2)",
            rusqlite::params![new_key, old_key],
        );
        let _ = conn.execute(
            "UPDATE file_refs SET row_key = ?1 WHERE LOWER(row_key) = LOWER(?2)",
            rusqlite::params![new_key, old_key],
        );
    }

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

pub fn delete_row_at(p: &Path, row_key: &str) -> AppResult<bool> {
    prompts_csv::delete_row(&p.join("prompts.csv"), row_key)?;

    let mut conn =
        crate::state_db::open(p).map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    let tx = conn
        .transaction()
        .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;

    tx.execute(
        "DELETE FROM attempts WHERE job_id IN (SELECT id FROM jobs WHERE LOWER(row_key) = LOWER(?1))",
        rusqlite::params![row_key],
    )
    .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;

    tx.execute(
        "DELETE FROM file_refs WHERE LOWER(row_key) = LOWER(?1) OR job_id IN (SELECT id FROM jobs WHERE LOWER(row_key) = LOWER(?1))",
        rusqlite::params![row_key],
    )
    .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;

    tx.execute(
        "DELETE FROM jobs WHERE LOWER(row_key) = LOWER(?1)",
        rusqlite::params![row_key],
    )
    .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;

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
    let conn =
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

    // Di trú bản ghi job "chưa sinh sản phẩm" của CHÍNH dòng này sang khoá mới.
    // Chỉ khớp chính xác `old_key` (và fallback tên ảnh trần khi `same_image == 1`);
    // TUYỆT ĐỐI không dùng `LIKE image_low || '#%'` để tránh cướp/xóa dòng anh em dùng chung ảnh.
    if same_image == 1 {
        conn.execute(
            "UPDATE jobs
             SET row_key = ?1, image_prompt_hash = ?2, video_prompt_hash = ?3
             WHERE (LOWER(row_key) = LOWER(?4) OR LOWER(row_key) = LOWER(?5))
               AND status IN ('dang_cho', 'bo_qua')
               AND attempts_count = 0
               AND image_status = 'chua_chay'
               AND video_status = 'chua_chay'",
            rusqlite::params![
                new_key,
                engine::sha2hex(trimmed_img),
                engine::sha2hex(trimmed_vid),
                old_key,
                image_low
            ],
        )
        .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    } else {
        conn.execute(
            "UPDATE jobs
             SET row_key = ?1, image_prompt_hash = ?2, video_prompt_hash = ?3
             WHERE LOWER(row_key) = LOWER(?4)
               AND status IN ('dang_cho', 'bo_qua')
               AND attempts_count = 0
               AND image_status = 'chua_chay'
               AND video_status = 'chua_chay'",
            rusqlite::params![
                new_key,
                engine::sha2hex(trimmed_img),
                engine::sha2hex(trimmed_vid),
                old_key
            ],
        )
        .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    }

    // Với các bản ghi job đã từng chạy và file_refs của chính dòng này: cập nhật `row_key` sang `new_key`
    // nhưng GIỮ NGUYÊN `image_prompt_hash` / `video_prompt_hash` lịch sử để cờ `prompt_modified`
    // tiếp tục phát hiện chính xác prompt đã sửa.
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
            let _ = conn.execute(
                "UPDATE OR IGNORE jobs SET row_key = ?1 WHERE LOWER(row_key) = LOWER(?2)",
                rusqlite::params![new_key, mk],
            );
            let _ = conn.execute(
                "UPDATE file_refs SET row_key = ?1 WHERE LOWER(row_key) = LOWER(?2)",
                rusqlite::params![new_key, mk],
            );
        }
    }
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

#[tauri::command]
pub fn get_settings(state: State<'_, AppState>) -> SettingsView {
    let s = state.settings_cache.read().clone();
    SettingsView {
        settings: s,
        image_key_preview: settings::key_preview("anh"),
        video_key_preview: settings::key_preview("video"),
        has_image_key: settings::has_api_key("anh"),
        has_video_key: settings::has_api_key("video"),
    }
}

#[tauri::command]
pub fn save_settings(settings: Settings, state: State<'_, AppState>) -> AppResult<bool> {
    let start_with_windows = {
        let mut cur = state.settings_cache.write();
        let recent = cur.recent_projects.clone();
        let current = cur.current_project.clone();
        let mut s = settings;
        s.recent_projects = recent;
        s.current_project = current;
        state.settings.save(&s)?;
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

#[tauri::command]
pub fn force_quit_app(
    stop_first: bool,
    app: AppHandle,
    state: State<'_, AppState>,
) -> AppResult<()> {
    state.exit_requested.store(true, Ordering::SeqCst);
    if stop_first {
        if let Some(flag) = state.stop_flag.read().clone() {
            flag.store(true, Ordering::SeqCst);
        }
    }
    if let Ok(p) = project_path(&state) {
        if let Ok(conn) = crate::state_db::open(&p) {
            let _ = crate::state_db::recover_interrupted_runs(&conn);
        }
    }
    crate::muse_sidecar::stop_for_quit();
    app.exit(0);
    Ok(())
}

#[tauri::command]
pub async fn test_connection(
    which: String,
    base_url: Option<String>,
    api_key: Option<String>,
    state: State<'_, AppState>,
) -> AppResult<(bool, u64, String)> {
    let cfg = state.settings_cache.read().clone();
    tauri::async_runtime::spawn_blocking(move || {
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
        crate::adapters::list_models(&cfg, &which, base_url.as_deref(), api_key.as_deref())
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAIL", format!("{e}")))?
}

// ━━ Commands: VideoFlow Muse nội bộ (engine + pool cookie + hạn mức) ━━

fn muse_session_or_err(cfg: &Settings) -> AppResult<(u16, String)> {
    let session = crate::muse_sidecar::ensure_running(cfg)?;
    Ok((session.port, session.api_key))
}

/// `sanitize_url_segment` trả `ApiFail` (kiểu lỗi adapter) — đổi sang `AppError`
/// theo hợp đồng command 13.2.
fn muse_sanitize_id(id: &str) -> AppResult<String> {
    crate::adapters::sanitize_url_segment(id, false)
        .map_err(|e| AppError::new(&e.code, e.message))
}

#[tauri::command]
pub async fn muse_sidecar_status(state: State<'_, AppState>) -> AppResult<serde_json::Value> {
    // Clone MỘT lần cho cả status lẫn phần mô tả: trước đây clone 2 lần nên nếu người
    // dùng lưu Cài đặt đúng lúc request đang chạy, response sẽ trộn cấu hình cũ/mới.
    let cfg = state.settings_cache.read().clone();
    let cfg_for_status = cfg.clone();
    let st = tauri::async_runtime::spawn_blocking(move || {
        crate::muse_sidecar::status(&cfg_for_status)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAIL", format!("{e}")))??;
    let chromium = if cfg.muse_chromium_path.trim().is_empty() {
        crate::muse_sidecar::detect_chromium("")
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default()
    } else {
        cfg.muse_chromium_path.clone()
    };
    Ok(serde_json::json!({
        "sidecar": st,
        "video_source": cfg.video_source,
        "chromium_path": chromium,
        "headless": cfg.muse_headless,
    }))
}

#[tauri::command]
pub async fn muse_start_sidecar(state: State<'_, AppState>) -> AppResult<serde_json::Value> {
    let cfg = state.settings_cache.read().clone();
    let st = tauri::async_runtime::spawn_blocking(move || {
        crate::muse_sidecar::start(&cfg)?;
        crate::muse_sidecar::status(&cfg)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAIL", format!("{e}")))??;
    // `SidecarStatus` đã derive Serialize — serialize thẳng để KHÔNG bỏ sót field mới
    // (cfg_port/cfg_chromium/cfg_headless/restart_required) như bản dựng tay trước đây.
    serde_json::to_value(&st)
        .map_err(|e| AppError::new("SERIALIZE_FAIL", format!("{e}")))
}

#[tauri::command]
pub fn muse_stop_sidecar() -> AppResult<bool> {
    crate::muse_sidecar::stop()?;
    Ok(true)
}

fn enrich_account_json_with_proxy(
    acc: &mut serde_json::Value,
    acc_to_proxy: &std::collections::HashMap<String, String>,
    proxy_by_id: &std::collections::HashMap<String, crate::proxies::ProxyItem>,
    sidecar_session: Option<(u16, &str)>,
) {
    let Some(obj) = acc.as_object_mut() else {
        return;
    };
    let acc_id = obj
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if acc_id.is_empty() {
        return;
    }
    let is_busy = obj.get("busy").and_then(|v| v.as_bool()).unwrap_or(false);
    let current_masked = obj
        .get("proxy")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    if let Some(pid) = acc_to_proxy.get(&acc_id) {
        if let Some(px) = proxy_by_id.get(pid) {
            obj.insert("proxy_id".into(), serde_json::Value::String(px.id.clone()));
            obj.insert("proxy_name".into(), serde_json::Value::String(px.name.clone()));
            obj.insert("proxy_kind".into(), serde_json::Value::String(px.kind.clone()));
            obj.insert(
                "proxy_exit_ip".into(),
                px.exit_ip
                    .as_ref()
                    .map(|s| serde_json::Value::String(s.clone()))
                    .unwrap_or(serde_json::Value::Null),
            );
            obj.insert(
                "proxy_status".into(),
                serde_json::Value::String(px.last_status.clone()),
            );
            obj.insert(
                "proxy_latency_ms".into(),
                px.latency_ms
                    .map(serde_json::Value::from)
                    .unwrap_or(serde_json::Value::Null),
            );
            let expected_masked = px.to_masked_url();
            if current_masked.as_deref() != Some(expected_masked.as_str()) && !is_busy {
                if let Some((port, key)) = sidecar_session {
                    if let Ok(safe_id) = muse_sanitize_id(&acc_id) {
                        let patch_body = serde_json::json!({ "proxy": px.to_proxy_url() });
                        let _ = crate::muse_sidecar::admin_send(
                            port,
                            key,
                            "PATCH",
                            &format!("/admin/accounts/{safe_id}"),
                            Some(&patch_body),
                        );
                    }
                }
            }
            obj.insert("proxy".into(), serde_json::Value::String(expected_masked));
            return;
        }
    }

    obj.insert("proxy_id".into(), serde_json::Value::Null);
    obj.insert("proxy_name".into(), serde_json::Value::Null);
    obj.insert("proxy_kind".into(), serde_json::Value::Null);
    obj.insert("proxy_exit_ip".into(), serde_json::Value::Null);
    obj.insert("proxy_status".into(), serde_json::Value::Null);
    obj.insert("proxy_latency_ms".into(), serde_json::Value::Null);
}

#[tauri::command]
pub async fn muse_list_accounts() -> AppResult<serde_json::Value> {
    tauri::async_runtime::spawn_blocking(move || {
        let session = crate::muse_sidecar::current_session().ok_or_else(|| {
            AppError::new(
                "MUSE_SIDECAR_STOPPED",
                "Engine VideoFlow Muse đang dừng — hãy khởi động engine trước khi tải danh sách tài khoản.",
            )
        })?;
        let mut res =
            crate::muse_sidecar::admin_get(session.port, &session.api_key, "/admin/accounts")?;
        if let Ok(conn) = crate::proxies::open_db() {
            let proxies = crate::proxies::list_proxies_in(&conn).unwrap_or_default();
            let acc_to_proxy = crate::proxies::load_account_to_proxy_map(&conn).unwrap_or_default();
            let proxy_by_id: std::collections::HashMap<String, crate::proxies::ProxyItem> =
                proxies.into_iter().map(|p| (p.id.clone(), p)).collect();
            if let Some(arr) = res.get_mut("data").and_then(|d| d.as_array_mut()) {
                for item in arr.iter_mut() {
                    enrich_account_json_with_proxy(
                        item,
                        &acc_to_proxy,
                        &proxy_by_id,
                        Some((session.port, &session.api_key)),
                    );
                }
            }
        }
        Ok(res)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAIL", format!("{e}")))?
}

/// Nhận cookie ở 3 format (dict JSON / chuỗi header raw / list EditThisCookie) —
/// muse2api tự parse; kèm `?allow_duplicate_session` khi user chọn bỏ qua cảnh báo trùng.
#[tauri::command]
pub async fn muse_add_cookie(
    label: String,
    cookies: serde_json::Value,
    allow_duplicate: bool,
    proxy_id: Option<String>,
    state: State<'_, AppState>,
) -> AppResult<serde_json::Value> {
    let cfg = state.settings_cache.read().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let clean_pid = proxy_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let resolved_proxy = if let Some(ref pid) = clean_pid {
            let conn = crate::proxies::open_db()?;
            let px = crate::proxies::get_proxy_in(&conn, pid)?;
            Some(px)
        } else {
            None
        };

        let (port, key) = muse_session_or_err(&cfg)?;
        let mut body = serde_json::json!({
            "label": label.trim(),
            "cookies": cookies,
        });
        if let Some(ref px) = resolved_proxy {
            if let Some(obj) = body.as_object_mut() {
                obj.insert("proxy".into(), serde_json::Value::String(px.to_proxy_url()));
            }
        }
        let dup_flag = if allow_duplicate { "?allow_duplicate_session=true" } else { "" };
        let mut res = crate::muse_sidecar::admin_send(
            port,
            &key,
            "POST",
            &format!("/admin/accounts{dup_flag}"),
            Some(&body),
        )?;

        if let Some(acc_obj) = res.get_mut("account") {
            let acc_id = acc_obj
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !acc_id.is_empty() {
                if let Ok(conn) = crate::proxies::open_db() {
                    let _ = crate::proxies::set_account_proxy_in(
                        &conn,
                        &acc_id,
                        clean_pid.as_deref(),
                    );
                    let proxies = crate::proxies::list_proxies_in(&conn).unwrap_or_default();
                    let acc_to_proxy =
                        crate::proxies::load_account_to_proxy_map(&conn).unwrap_or_default();
                    let proxy_by_id: std::collections::HashMap<String, crate::proxies::ProxyItem> =
                        proxies.into_iter().map(|p| (p.id.clone(), p)).collect();
                    enrich_account_json_with_proxy(
                        acc_obj,
                        &acc_to_proxy,
                        &proxy_by_id,
                        Some((port, &key)),
                    );
                }
            }
        }
        Ok(res)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAIL", format!("{e}")))?
}

#[tauri::command]
pub async fn muse_update_account(
    account_id: String,
    enabled: Option<bool>,
    label: Option<String>,
    proxy: Option<String>,
    proxy_id: Option<String>,
    state: State<'_, AppState>,
) -> AppResult<serde_json::Value> {
    if proxy_id.is_some() || proxy.is_some() {
        ensure_not_running(&state)?;
    }
    let cfg = state.settings_cache.read().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (port, key) = muse_session_or_err(&cfg)?;
        let mut body = serde_json::Map::new();
        if let Some(e) = enabled {
            body.insert("enabled".into(), serde_json::Value::Bool(e));
        }
        if let Some(l) = label {
            body.insert("label".into(), serde_json::Value::String(l.trim().to_string()));
        }

        let mut target_proxy_id: Option<Option<String>> = None;
        if let Some(pid_raw) = proxy_id {
            let trimmed = pid_raw.trim();
            if trimmed.is_empty() {
                body.insert("proxy".into(), serde_json::Value::String(String::new()));
                target_proxy_id = Some(None);
            } else {
                let conn = crate::proxies::open_db()?;
                let px = crate::proxies::get_proxy_in(&conn, trimmed)?;
                body.insert("proxy".into(), serde_json::Value::String(px.to_proxy_url()));
                target_proxy_id = Some(Some(px.id));
            }
        } else if let Some(p) = proxy {
            let trimmed = p.trim();
            body.insert("proxy".into(), serde_json::Value::String(trimmed.to_string()));
            if trimmed.is_empty() {
                target_proxy_id = Some(None);
            }
        }

        let safe_id = muse_sanitize_id(&account_id)?;
        let path = format!("/admin/accounts/{safe_id}");
        let mut res = crate::muse_sidecar::admin_send(
            port,
            &key,
            "PATCH",
            &path,
            Some(&serde_json::Value::Object(body)),
        )?;

        if let Ok(conn) = crate::proxies::open_db() {
            if let Some(ref pid_opt) = target_proxy_id {
                crate::proxies::set_account_proxy_in(&conn, &safe_id, pid_opt.as_deref())?;
            }
            if let Some(acc_obj) = res.get_mut("account") {
                let proxies = crate::proxies::list_proxies_in(&conn).unwrap_or_default();
                let acc_to_proxy =
                    crate::proxies::load_account_to_proxy_map(&conn).unwrap_or_default();
                let proxy_by_id: std::collections::HashMap<String, crate::proxies::ProxyItem> =
                    proxies.into_iter().map(|p| (p.id.clone(), p)).collect();
                enrich_account_json_with_proxy(
                    acc_obj,
                    &acc_to_proxy,
                    &proxy_by_id,
                    Some((port, &key)),
                );
            }
        }
        Ok(res)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAIL", format!("{e}")))?
}

#[tauri::command]
pub async fn muse_open_login(
    account_id: String,
    state: State<'_, AppState>,
) -> AppResult<serde_json::Value> {
    let cfg = state.settings_cache.read().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (port, key) = muse_session_or_err(&cfg)?;
        let safe_id = muse_sanitize_id(&account_id)?;
        crate::muse_sidecar::open_login(port, &key, &safe_id)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAIL", format!("{e}")))?
}

#[tauri::command]
pub async fn muse_open_view(
    account_id: String,
    state: State<'_, AppState>,
) -> AppResult<serde_json::Value> {
    let cfg = state.settings_cache.read().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (port, key) = muse_session_or_err(&cfg)?;
        let safe_id = muse_sanitize_id(&account_id)?;
        crate::muse_sidecar::open_view(port, &key, &safe_id)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAIL", format!("{e}")))?
}

#[tauri::command]
pub async fn muse_delete_account(
    account_id: String,
    state: State<'_, AppState>,
) -> AppResult<serde_json::Value> {
    let cfg = state.settings_cache.read().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (port, key) = muse_session_or_err(&cfg)?;
        let safe_id = muse_sanitize_id(&account_id)?;
        let path = format!("/admin/accounts/{safe_id}");
        let res = crate::muse_sidecar::admin_send(port, &key, "DELETE", &path, None)?;
        if let Ok(conn) = crate::proxies::open_db() {
            let _ = crate::proxies::remove_account_link_in(&conn, &safe_id);
        }
        Ok(res)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAIL", format!("{e}")))?
}

#[tauri::command]
pub async fn muse_renew_account(
    account_id: String,
    state: State<'_, AppState>,
) -> AppResult<serde_json::Value> {
    let cfg = state.settings_cache.read().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (port, key) = muse_session_or_err(&cfg)?;
        let path = format!(
            "/admin/accounts/{}/renew",
            muse_sanitize_id(&account_id)?
        );
        // renew_session có thể poll VM wake + proxy chậm → dùng 90s.
        crate::muse_sidecar::admin_send_timeout(port, &key, "POST", &path, None, 90)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAIL", format!("{e}")))?
}

#[tauri::command]
pub async fn muse_reset_account(
    account_id: String,
    state: State<'_, AppState>,
) -> AppResult<serde_json::Value> {
    let cfg = state.settings_cache.read().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (port, key) = muse_session_or_err(&cfg)?;
        let path = format!(
            "/admin/accounts/{}/reset",
            muse_sanitize_id(&account_id)?
        );
        crate::muse_sidecar::admin_send(port, &key, "POST", &path, None)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAIL", format!("{e}")))?
}

/// Đọc hạn mức của một account: dùng cache 30 phút của sidecar khi `refresh=false`,
/// force đọc lại từ trang Settings → Usage của muse.ai khi `refresh=true` (thao tác
/// browser thật — có thể bị từ chối khi account đang phục vụ request).
#[tauri::command]
pub async fn muse_account_quota(
    account_id: String,
    refresh: bool,
    state: State<'_, AppState>,
) -> AppResult<serde_json::Value> {
    let cfg = state.settings_cache.read().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (port, key) = muse_session_or_err(&cfg)?;
        let id = muse_sanitize_id(&account_id)?;
        let flag = if refresh { "?refresh=1" } else { "" };
        let path = format!("/admin/accounts/{id}/quota{flag}");
        // Đọc quota bằng browser (cold-start Edge riêng theo account + mở muse.ai qua proxy) → dùng 90s.
        crate::muse_sidecar::admin_get_timeout(port, &key, &path, 90)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAIL", format!("{e}")))?
}

/// Tải Chrome extension import cookie (GET /admin/extension.zip) về thư mục dữ liệu app
/// rồi mở Explorer tại đó cho user cài vào trình duyệt.
#[tauri::command]
pub async fn muse_download_extension(state: State<'_, AppState>) -> AppResult<String> {
    let cfg = state.settings_cache.read().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (port, key) = muse_session_or_err(&cfg)?;
        let dir = crate::settings::app_data_dir().join(crate::muse_sidecar::MUSE_DATA_DIR);
        std::fs::create_dir_all(&dir)
            .map_err(|e| AppError::new("MUSE_EXT_FAIL", format!("Không tạo được thư mục: {e}")))?;
        let dest = dir.join("videoflow-muse-extension.zip");
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|e| AppError::new("MUSE_EXT_FAIL", format!("{e}")))?;
        let resp = client
            .get(format!("http://127.0.0.1:{port}/admin/extension.zip"))
            .bearer_auth(&key)
            .send()
            .map_err(|e| AppError::new("MUSE_EXT_FAIL", format!("Tải extension thất bại: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(AppError::new(
                "MUSE_EXT_FAIL",
                format!("Sidecar trả lỗi {status} khi tải extension.zip"),
            ));
        }
        let bytes = resp
            .bytes()
            .map_err(|e| AppError::new("MUSE_EXT_FAIL", format!("Đọc nội dung thất bại: {e}")))?;
        std::fs::write(&dest, &bytes)
            .map_err(|e| AppError::new("MUSE_EXT_FAIL", format!("Không ghi được file: {e}")))?;
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            let _ = std::process::Command::new(windows_explorer_binary())
                .raw_arg(format!("\"{}\"", dir.to_string_lossy()))
                .creation_flags(CREATE_NO_WINDOW)
                .spawn();
        }
        Ok(dest.to_string_lossy().to_string())
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAIL", format!("{e}")))?
}

// ━━ Commands: Quản lý Proxy toàn cục (proxies.db — Single Source of Truth) ━━

fn check_sidecar_accounts_not_busy(
    session: &crate::muse_sidecar::SidecarSession,
    account_ids: &[String],
) -> AppResult<()> {
    if account_ids.is_empty() {
        return Ok(());
    }
    if let Ok(res) = crate::muse_sidecar::admin_get(session.port, &session.api_key, "/admin/accounts")
    {
        if let Some(arr) = res.get("data").and_then(|d| d.as_array()) {
            for acc in arr {
                let id = acc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let busy = acc.get("busy").and_then(|v| v.as_bool()).unwrap_or(false);
                if busy && account_ids.iter().any(|a| a == id) {
                    let label = acc
                        .get("label")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .unwrap_or(id);
                    return Err(AppError::new(
                        "ACCOUNT_BUSY",
                        format!(
                            "Tài khoản '{label}' đang bận thực thi hoặc đang mở trình duyệt đăng nhập, không thể sửa/xoá proxy lúc này."
                        ),
                    ));
                }
            }
        }
    }
    Ok(())
}

#[tauri::command]
pub fn proxy_list() -> AppResult<Vec<crate::proxies::ProxyItem>> {
    let conn = crate::proxies::open_db()?;
    crate::proxies::list_proxies_in(&conn)
}

#[tauri::command]
pub fn proxy_create(input: crate::proxies::ProxyInput) -> AppResult<crate::proxies::ProxyItem> {
    let conn = crate::proxies::open_db()?;
    crate::proxies::create_proxy_in(&conn, &input)
}

#[tauri::command]
pub async fn proxy_update(
    proxy_id: String,
    input: crate::proxies::ProxyInput,
    state: State<'_, AppState>,
) -> AppResult<crate::proxies::ProxyItem> {
    ensure_not_running(&state)?;
    tauri::async_runtime::spawn_blocking(move || {
        let conn = crate::proxies::open_db()?;
        let existing = crate::proxies::get_proxy_in(&conn, &proxy_id)?;
        let session_opt = crate::muse_sidecar::current_session();

        if let Some(ref session) = session_opt {
            check_sidecar_accounts_not_busy(session, &existing.assigned_account_ids)?;
        }

        let updated = crate::proxies::update_proxy_in(&conn, &proxy_id, &input)?;

        if let Some(ref session) = session_opt {
            let new_url = updated.to_proxy_url();
            for acc_id in &updated.assigned_account_ids {
                if let Ok(safe_id) = muse_sanitize_id(acc_id) {
                    let body = serde_json::json!({ "proxy": new_url });
                    let _ = crate::muse_sidecar::admin_send(
                        session.port,
                        &session.api_key,
                        "PATCH",
                        &format!("/admin/accounts/{safe_id}"),
                        Some(&body),
                    );
                }
            }
        }

        Ok(updated)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAIL", format!("{e}")))?
}

#[tauri::command]
pub async fn proxy_delete(
    proxy_id: String,
    state: State<'_, AppState>,
) -> AppResult<serde_json::Value> {
    ensure_not_running(&state)?;
    tauri::async_runtime::spawn_blocking(move || {
        let conn = crate::proxies::open_db()?;
        let existing = crate::proxies::get_proxy_in(&conn, &proxy_id)?;
        let session_opt = crate::muse_sidecar::current_session();

        if let Some(ref session) = session_opt {
            check_sidecar_accounts_not_busy(session, &existing.assigned_account_ids)?;
            for acc_id in &existing.assigned_account_ids {
                if let Ok(safe_id) = muse_sanitize_id(acc_id) {
                    let body = serde_json::json!({ "proxy": "" });
                    let _ = crate::muse_sidecar::admin_send(
                        session.port,
                        &session.api_key,
                        "PATCH",
                        &format!("/admin/accounts/{safe_id}"),
                        Some(&body),
                    );
                }
            }
        }

        let affected = crate::proxies::delete_proxy_in(&conn, &proxy_id)?;
        Ok(serde_json::json!({
            "deleted": proxy_id,
            "unassigned_accounts": affected,
        }))
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAIL", format!("{e}")))?
}

#[tauri::command]
pub async fn proxy_test(proxy_id: String) -> AppResult<crate::proxies::ProxyItem> {
    tauri::async_runtime::spawn_blocking(move || {
        let conn = crate::proxies::open_db()?;
        crate::proxies::test_proxy_connection(&conn, &proxy_id)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAIL", format!("{e}")))?
}

#[tauri::command]
pub async fn proxy_test_all() -> AppResult<Vec<crate::proxies::ProxyItem>> {
    tauri::async_runtime::spawn_blocking(move || {
        let conn = crate::proxies::open_db()?;
        let list = crate::proxies::list_proxies_in(&conn)?;
        let mut results = Vec::with_capacity(list.len());
        for p in list {
            match crate::proxies::test_proxy_connection(&conn, &p.id) {
                Ok(updated) => results.push(updated),
                Err(_) => results.push(p),
            }
        }
        Ok(results)
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

    if state.running.load(Ordering::SeqCst) {
        return Err(AppError::new(
            "RUN_IN_PROGRESS",
            "Pipeline vừa bắt đầu chạy trong lúc tải bản cập nhật — hãy dừng (STOP) rồi khởi động lại app.",
        ));
    }
    log_info_app("Đã cài bản cập nhật — khởi động lại ứng dụng");
    // Dừng engine trước khi thoát để không để lại Edge mồ côi giữ profile.
    crate::muse_sidecar::stop_for_quit();
    app.restart();
}

fn log_info_app(msg: &str) {
    crate::muse_sidecar::log_app_event(msg);
}

// ━━ Commands: Lịch sử, Chi tiết lượt, Xuất CSV/Log, Nhật ký & Explorer ━━

fn validate_iso_date(label: &str, d: &str) -> AppResult<()> {
    if d.len() != 10 || !d.chars().all(|c| c.is_ascii_digit() || c == '-') {
        return Err(AppError::new(
            "BAD_DATE_FILTER",
            format!("{label} phải có định dạng YYYY-MM-DD"),
        ));
    }
    Ok(())
}

fn build_history_where_clause(
    status: Option<&str>,
    kind: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
) -> AppResult<(String, Vec<String>)> {
    let mut where_clauses: Vec<String> = vec!["total_rows > 0".into()];
    let mut params: Vec<String> = Vec::new();

    if let Some(st_raw) = status.map(str::trim) {
        let mapped = match st_raw {
            "" | "Tất cả" | "all" => None,
            "Hoàn tất" | "hoan_tat" => Some("hoan_tat"),
            "Đã dừng" | "da_dung" => Some("da_dung"),
            "Gián đoạn" | "gian_doan" => Some("gian_doan"),
            "Đang chạy" | "dang_chay" => Some("dang_chay"),
            other => Some(other),
        };
        if let Some(st) = mapped {
            where_clauses.push("status = ?".into());
            params.push(st.to_string());
        }
    }

    if let Some(k_raw) = kind.map(str::trim) {
        let mapped_k = match k_raw {
            "" | "Tất cả" | "all" => None,
            "Chạy chung" | "chung" => Some("chung"),
            "Chạy riêng" | "rieng" => Some("rieng"),
            "Lịch" | "lich" => Some("lich"),
            "Làm lại" | "lam_lai" => Some("lam_lai"),
            other => Some(other),
        };
        if let Some(k) = mapped_k {
            where_clauses.push("kind = ?".into());
            params.push(k.to_string());
        }
    }

    if let Some(f) = from.map(str::trim).filter(|s| !s.is_empty()) {
        validate_iso_date("Từ ngày", f)?;
        where_clauses.push("date(started_at, 'localtime') >= ?".into());
        params.push(f.to_string());
    }

    if let Some(t) = to.map(str::trim).filter(|s| !s.is_empty()) {
        validate_iso_date("Đến ngày", t)?;
        where_clauses.push("date(started_at, 'localtime') <= ?".into());
        params.push(t.to_string());
    }

    Ok((format!("WHERE {}", where_clauses.join(" AND ")), params))
}

#[tauri::command]
pub fn get_history(
    page: Option<usize>,
    page_size: Option<usize>,
    status: Option<String>,
    kind: Option<String>,
    from: Option<String>,
    to: Option<String>,
    state: State<'_, AppState>,
) -> AppResult<HistoryPage> {
    let p = project_path(&state)?;
    let conn =
        crate::state_db::open(&p).map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;

    let cutoff_7d = (chrono::Local::now() - chrono::Duration::days(7)).to_rfc3339();
    let stats_7d = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(done_count), 0), COALESCE(SUM(error_count), 0)
             FROM runs
             WHERE total_rows > 0 AND datetime(started_at) >= datetime(?1)",
            rusqlite::params![cutoff_7d],
            |r| {
                Ok(HistoryStats7d {
                    runs_count: r.get(0)?,
                    done_rows: r.get(1)?,
                    error_rows: r.get(2)?,
                })
            },
        )
        .unwrap_or_default();

    let (where_sql, params) = build_history_where_clause(
        status.as_deref(),
        kind.as_deref(),
        from.as_deref(),
        to.as_deref(),
    )?;

    let total: i64 = conn
        .query_row(
            &format!("SELECT COUNT(*) FROM runs {where_sql}"),
            rusqlite::params_from_iter(params.iter()),
            |r| r.get(0),
        )
        .unwrap_or(0);

    let page_size = page_size.unwrap_or(25).max(1);
    let page_count = (total.max(0) as usize).div_ceil(page_size).max(1);
    let page = page.unwrap_or(1).max(1).min(page_count);
    let sql = format!(
        "SELECT id, kind, status, started_at, finished_at, total_rows, done_count, error_count
         FROM runs {where_sql}
         ORDER BY id DESC
         LIMIT {page_size} OFFSET {}",
        (page - 1) * page_size
    );

    let mut runs = Vec::new();
    if let Ok(mut stmt) = conn.prepare(&sql) {
        if let Ok(it) = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
            let started_at: String = r.get(3)?;
            let finished_at: Option<String> = r.get(4)?;
            let duration_ms = compute_duration_ms(&started_at, finished_at.as_deref());
            Ok(RunSummary {
                id: r.get(0)?,
                kind: r.get(1)?,
                status: r.get(2)?,
                started_at,
                finished_at,
                total_rows: r.get(5)?,
                done_count: r.get(6)?,
                error_count: r.get(7)?,
                duration_ms,
            })
        }) {
            for item in it.flatten() {
                runs.push(item);
            }
        }
    }

    Ok(HistoryPage {
        runs,
        total: total.max(0) as usize,
        page,
        page_count,
        stats_7d,
    })
}

pub fn get_run_detail_at(p: &Path, run_id: i64) -> AppResult<(RunSummary, Vec<RowSummary>)> {
    let conn =
        crate::state_db::open(p).map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    let (id, kind, status, started_at, finished_at, total_rows, done_count, error_count): (
        i64,
        String,
        String,
        String,
        Option<String>,
        i64,
        i64,
        i64,
    ) = conn
        .query_row(
            "SELECT id, kind, status, started_at, finished_at, total_rows, done_count, error_count
             FROM runs WHERE id = ?1",
            rusqlite::params![run_id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            },
        )
        .map_err(|_| AppError::new("RUN_NOT_FOUND", format!("Không tìm thấy lượt #{run_id}.")))?;

    let duration_ms = compute_duration_ms(&started_at, finished_at.as_deref());

    let mut rows = Vec::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT
            j.row_key,
            j.status,
            j.last_error,
            j.attempts_count,
            COALESCE(j.row_no, 0),
            COALESCE(
                (SELECT f.file_name FROM file_refs f WHERE f.job_id = j.id AND f.kind = 'anh' ORDER BY f.version DESC LIMIT 1),
                CASE WHEN j.image_status = 'xong' THEN
                    (SELECT f.file_name FROM file_refs f WHERE LOWER(f.row_key) = LOWER(j.row_key) AND f.kind = 'anh' ORDER BY f.is_current DESC, f.version DESC LIMIT 1)
                ELSE '' END,
                ''
            ) AS img_file,
            COALESCE(
                (SELECT f.file_name FROM file_refs f WHERE f.job_id = j.id AND f.kind = 'video' ORDER BY f.version DESC LIMIT 1),
                CASE WHEN j.video_status = 'xong' THEN
                    (SELECT f.file_name FROM file_refs f WHERE LOWER(f.row_key) = LOWER(j.row_key) AND f.kind = 'video' ORDER BY f.is_current DESC, f.version DESC LIMIT 1)
                ELSE '' END,
                ''
            ) AS vid_file
         FROM jobs j
         WHERE j.run_id = ?1
         ORDER BY j.row_no, j.id",
    ) {
        if let Ok(it) = stmt.query_map(rusqlite::params![run_id], |r| {
            let db_status: String = r.get(1)?;
            Ok(RowSummary {
                row_key: {
                    let k: String = r.get(0)?;
                    k
                },
                image_name: {
                    // Bảng jobs chỉ lưu khóa dòng → hiển thị tên ảnh bằng cách bỏ hậu tố hash.
                    let k: String = r.get(0)?;
                    crate::prompts_csv::row_key_image_name(&k).to_string()
                },
                status: engine::db_status_to_label(&db_status).into(),
                last_error: r.get(2)?,
                attempts_count: r.get(3)?,
                row_no: r.get::<_, i64>(4)?.max(0) as usize,
                image_file: r.get(5)?,
                video_file: r.get(6)?,
                prompt_modified: false,
                ref_exists: true,
                noi_canh: false,
            })
        }) {
            for item in it.flatten() {
                rows.push(item);
            }
        }
    }

    Ok((
        RunSummary {
            id,
            kind,
            status,
            started_at,
            finished_at,
            total_rows,
            done_count,
            error_count,
            duration_ms,
        },
        rows,
    ))
}

#[tauri::command]
pub fn get_run_detail(
    run_id: i64,
    state: State<'_, AppState>,
) -> AppResult<(RunSummary, Vec<RowSummary>)> {
    let p = project_path(&state)?;
    get_run_detail_at(&p, run_id)
}

fn validate_export_save_path(
    state: &AppState,
    project_dir: &Path,
    save_path: &str,
    allowed_exts: &[&str],
) -> AppResult<()> {
    let trimmed = save_path.trim();
    if trimmed.is_empty() {
        return Err(AppError::new("EMPTY_PATH", "Đường dẫn lưu file trống."));
    }
    let key = normalize_win_path_key(trimmed);
    if state.approved_export_paths.read().contains(&key) {
        return Ok(());
    }
    let pb = PathBuf::from(trimmed);
    let ext_ok = pb
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| allowed_exts.iter().any(|a| ext.eq_ignore_ascii_case(a)));
    if !ext_ok {
        return Err(AppError::new(
            "FORBIDDEN_EXPORT_PATH",
            format!(
                "Đuôi file xuất không hợp lệ (chỉ hỗ trợ {}).",
                allowed_exts.join(", ")
            ),
        ));
    }
    if pb
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(AppError::new(
            "FORBIDDEN_EXPORT_PATH",
            "Đường dẫn xuất file không được chứa '..'.",
        ));
    }
    if let (Ok(proj_canon), Some(parent)) = (std::fs::canonicalize(project_dir), pb.parent()) {
        if let Ok(parent_canon) = std::fs::canonicalize(parent) {
            if parent_canon.starts_with(&proj_canon) {
                return Ok(());
            }
        }
    }
    Err(AppError::new(
        "FORBIDDEN_EXPORT_PATH",
        "Đường dẫn lưu file chưa được chọn qua hộp thoại hệ thống hoặc nằm ngoài thư mục dự án.",
    ))
}

#[tauri::command]
pub async fn pick_save_csv(
    app: AppHandle,
    state: State<'_, AppState>,
) -> AppResult<Option<String>> {
    let default_name = format!(
        "videoflow_history_{}.csv",
        chrono::Local::now().format("%Y%m%d_%H%M%S")
    );
    let picked = tauri::async_runtime::spawn_blocking(move || {
        let (tx, rx) = std::sync::mpsc::channel::<Option<String>>();
        app.dialog()
            .file()
            .set_title("Lưu file lịch sử CSV")
            .set_file_name(&default_name)
            .add_filter("CSV UTF-8", &["csv"])
            .save_file(move |f| {
                let chosen = f.map(|fp| {
                    let mut s = fp
                        .into_path()
                        .map(|pb| strip_verbatim_prefix(pb.to_string_lossy().into_owned()))
                        .unwrap_or_else(|e| e.to_string());
                    if !s.to_ascii_lowercase().ends_with(".csv") {
                        s.push_str(".csv");
                    }
                    s
                });
                let _ = tx.send(chosen);
            });
        rx.recv().ok().flatten()
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAIL", format!("{e}")))?;
    if let Some(ref p) = picked {
        state
            .approved_export_paths
            .write()
            .insert(normalize_win_path_key(p));
    }
    Ok(picked)
}

#[tauri::command]
pub async fn pick_save_log(
    app: AppHandle,
    state: State<'_, AppState>,
) -> AppResult<Option<String>> {
    let default_name = format!(
        "videoflow_logs_{}.log",
        chrono::Local::now().format("%Y%m%d_%H%M%S")
    );
    let picked = tauri::async_runtime::spawn_blocking(move || {
        let (tx, rx) = std::sync::mpsc::channel::<Option<String>>();
        app.dialog()
            .file()
            .set_title("Xuất nhật ký kỹ thuật (.log)")
            .set_file_name(&default_name)
            .add_filter("Log / Text", &["log", "txt"])
            .save_file(move |f| {
                let chosen = f.map(|fp| {
                    let mut s = fp
                        .into_path()
                        .map(|pb| strip_verbatim_prefix(pb.to_string_lossy().into_owned()))
                        .unwrap_or_else(|e| e.to_string());
                    let low = s.to_ascii_lowercase();
                    if !low.ends_with(".log") && !low.ends_with(".txt") {
                        s.push_str(".log");
                    }
                    s
                });
                let _ = tx.send(chosen);
            });
        rx.recv().ok().flatten()
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAIL", format!("{e}")))?;
    if let Some(ref p) = picked {
        state
            .approved_export_paths
            .write()
            .insert(normalize_win_path_key(p));
    }
    Ok(picked)
}

pub fn export_history_csv_at(
    p: &Path,
    filter: Option<serde_json::Value>,
    save_path: &str,
) -> AppResult<bool> {
    let parsed: HistoryExportFilter = match filter {
        Some(serde_json::Value::String(s)) => HistoryExportFilter {
            status: Some(s),
            ..Default::default()
        },
        Some(v @ serde_json::Value::Object(_)) => serde_json::from_value(v).unwrap_or_default(),
        _ => HistoryExportFilter::default(),
    };

    let (where_sql, params) = build_history_where_clause(
        parsed.status.as_deref(),
        parsed.kind.as_deref(),
        parsed.from.as_deref(),
        parsed.to.as_deref(),
    )?;

    let conn =
        crate::state_db::open(p).map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    let sql = format!(
        "SELECT id, kind, status, started_at, finished_at, total_rows, done_count, error_count
         FROM runs
         {where_sql}
         ORDER BY id DESC"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    let it = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |r| {
            let started_at: String = r.get(3)?;
            let finished_at: Option<String> = r.get(4)?;
            let dur_s = compute_duration_ms(&started_at, finished_at.as_deref())
                .map(|ms| format!("{:.1}s", ms as f64 / 1000.0))
                .unwrap_or_default();
            Ok(vec![
                r.get::<_, i64>(0)?.to_string(),
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                started_at,
                finished_at.unwrap_or_default(),
                dur_s,
                r.get::<_, i64>(5)?.to_string(),
                r.get::<_, i64>(6)?.to_string(),
                r.get::<_, i64>(7)?.to_string(),
            ])
        })
        .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;

    let mut out = Vec::new();
    out.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    {
        let mut records: Vec<Vec<String>> = Vec::new();
        for r in it {
            match r {
                Ok(rec) => records.push(rec),
                Err(e) => return Err(AppError::new("DB_FAIL", format!("{e}"))),
            }
        }
        let mut wtr = csv::WriterBuilder::new()
            .terminator(csv::Terminator::CRLF)
            .from_writer(&mut out);
        wtr.write_record([
            "run_id",
            "loai",
            "ket_qua",
            "bat_dau",
            "ket_thuc",
            "thoi_luong",
            "tong_dong",
            "xong",
            "loi",
        ])
        .map_err(|e| AppError::new("CSV_EXPORT_FAIL", format!("{e}")))?;
        for rec in records {
            wtr.write_record(&rec)
                .map_err(|e| AppError::new("CSV_EXPORT_FAIL", format!("{e}")))?;
        }
        wtr.flush()
            .map_err(|e| AppError::new("CSV_EXPORT_FAIL", format!("{e}")))?;
    }
    std::fs::write(save_path, &out)
        .map_err(|e| AppError::new("CSV_EXPORT_FAIL", format!("Không ghi được file: {e}")))?;
    Ok(true)
}

#[tauri::command]
pub fn export_history_csv(
    filter: Option<serde_json::Value>,
    save_path: String,
    state: State<'_, AppState>,
) -> AppResult<bool> {
    let p = project_path(&state)?;
    validate_export_save_path(&state, &p, &save_path, &["csv"])?;
    export_history_csv_at(&p, filter, &save_path)
}

fn query_logs_at(
    p: &Path,
    run_id: Option<i64>,
    level: Option<&str>,
    limit: usize,
) -> AppResult<Vec<LogItem>> {
    let conn =
        crate::state_db::open(p).map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;

    let norm_level = level
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .filter(|l| matches!(l.as_str(), "info" | "warn" | "error"));

    let max_rows = limit.clamp(1, 5000) as i64;
    let mut clauses: Vec<&str> = Vec::new();
    let mut sql_params: Vec<rusqlite::types::Value> = Vec::new();

    if let Some(rid) = run_id {
        clauses.push("run_id = ?");
        sql_params.push(rusqlite::types::Value::Integer(rid));
    }
    if let Some(lvl) = norm_level {
        clauses.push("level = ?");
        sql_params.push(rusqlite::types::Value::Text(lvl));
    }
    sql_params.push(rusqlite::types::Value::Integer(max_rows));

    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    let sql = format!(
        "SELECT id, ts, level, run_id, row_key, step, message, detail
         FROM logs
         {where_sql}
         ORDER BY id DESC
         LIMIT ?"
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;
    let it = stmt
        .query_map(rusqlite::params_from_iter(sql_params.iter()), |r| {
            Ok(LogItem {
                id: r.get(0)?,
                ts: r.get(1)?,
                level: r.get(2)?,
                run_id: r.get(3)?,
                row_key: r.get(4)?,
                step: r.get(5)?,
                message: r.get(6)?,
                detail: r.get(7)?,
            })
        })
        .map_err(|e| AppError::new("DB_FAIL", format!("{e}")))?;

    let mut out = Vec::new();
    for item in it.flatten() {
        out.push(item);
    }
    Ok(out)
}

#[tauri::command]
pub fn get_logs(
    run_id: Option<i64>,
    level: Option<String>,
    limit: Option<usize>,
    state: State<'_, AppState>,
) -> AppResult<Vec<LogItem>> {
    let p = project_path(&state)?;
    query_logs_at(&p, run_id, level.as_deref(), limit.unwrap_or(200))
}

#[tauri::command]
pub fn export_logs_txt(
    run_id: Option<i64>,
    level: Option<String>,
    save_path: String,
    state: State<'_, AppState>,
) -> AppResult<bool> {
    let p = project_path(&state)?;
    validate_export_save_path(&state, &p, &save_path, &["log", "txt"])?;
    let items = query_logs_at(&p, run_id, level.as_deref(), 5000)?;
    let mut text = String::from("\u{FEFF}# VideoFlow Desktop — Nhật ký kỹ thuật\r\n");
    for item in items.iter().rev() {
        let rid_str = item
            .run_id
            .map(|id| format!("run#{id}"))
            .unwrap_or_else(|| "-".into());
        let row_str = item.row_key.as_deref().unwrap_or("-");
        let step_str = item.step.as_deref().unwrap_or("-");
        let detail_str = item
            .detail
            .as_deref()
            .filter(|d| !d.is_empty())
            .map(|d| format!(" | {d}"))
            .unwrap_or_default();
        text.push_str(&format!(
            "[{}] [{:<5}] [{}] [{}] [{}] {}{}\r\n",
            item.ts,
            item.level.to_ascii_uppercase(),
            rid_str,
            row_str,
            step_str,
            item.message,
            detail_str
        ));
    }
    std::fs::write(&save_path, text.as_bytes())
        .map_err(|e| AppError::new("LOG_EXPORT_FAIL", format!("Không ghi được file log: {e}")))?;
    Ok(true)
}

/// Xác thực đường dẫn đích bắt buộc nằm bên trong thư mục dự án hiện tại (chống path traversal).
pub fn resolve_safe_project_target(project_dir: &Path, target_path: &str) -> AppResult<PathBuf> {
    let trimmed = target_path.trim();
    if trimmed.is_empty() {
        return Err(AppError::new("EMPTY_PATH", "Đường dẫn trống."));
    }
    let proj_canon = std::fs::canonicalize(project_dir).map_err(|e| {
        AppError::new(
            "PROJECT_MISSING",
            format!("Không truy cập được thư mục dự án: {e}"),
        )
    })?;

    let raw = PathBuf::from(trimmed);
    let candidate = if raw.is_absolute() {
        raw
    } else {
        let mut resolved = project_dir.join(&raw);
        if !resolved.exists() {
            for sub in ["image_out", "video_out", "input"] {
                let try_sub = project_dir.join(sub).join(&raw);
                if try_sub.exists() {
                    resolved = try_sub;
                    break;
                }
            }
        }
        resolved
    };

    let target_canon = std::fs::canonicalize(&candidate).map_err(|_| {
        AppError::new(
            "PATH_NOT_FOUND",
            format!("Không tìm thấy file/thư mục: {}", candidate.display()),
        )
    })?;

    if !target_canon.starts_with(&proj_canon) {
        return Err(AppError::new(
            "FORBIDDEN_PATH",
            "Không cho phép mở đường dẫn nằm ngoài thư mục dự án hiện tại.",
        ));
    }

    Ok(target_canon)
}

#[cfg(target_os = "windows")]
fn windows_explorer_binary() -> PathBuf {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    PathBuf::from(root).join("explorer.exe")
}

/// [P1-2]: Luôn mở qua `%SystemRoot%\explorer.exe` với `raw_arg` được bọc ngoặc kép chuẩn:
/// - Mọi file đều dùng `/select,"<path>"` (KHÔNG BAO GIỜ thực thi file qua `ShellExecute`!).
/// - Mọi thư mục dùng `"<path>"` (không bị gãy khi tên thư mục chứa dấu phẩy `,`).
#[tauri::command]
pub fn open_path_in_explorer(
    reveal: Option<bool>,
    target_path: String,
    state: State<'_, AppState>,
) -> AppResult<bool> {
    let _ = reveal;
    let target_canon = match project_path(&state)
        .and_then(|proj| resolve_safe_project_target(&proj, &target_path))
    {
        Ok(canon) => canon,
        Err(orig_err) => {
            // Cho phép mở đúng file `prompts.csv` trong thư mục vừa probe (Probe Modal)
            // kể cả khi chưa mở dự án hoặc đang probe thư mục khác.
            let probed_opt = state.last_probed_dir.read().clone();
            if let Some(probed) = probed_opt {
                if let Ok(canon) = resolve_safe_project_target(&probed, &target_path) {
                    let is_prompts_csv = canon
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.eq_ignore_ascii_case("prompts.csv"));
                    if is_prompts_csv && canon.is_file() {
                        canon
                    } else {
                        return Err(orig_err);
                    }
                } else {
                    return Err(orig_err);
                }
            } else {
                return Err(orig_err);
            }
        }
    };
    let clean_path = strip_verbatim_prefix(target_canon.to_string_lossy().into_owned())
        .replace('"', "");

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let explorer = windows_explorer_binary();
        let raw_argument = if target_canon.is_file() {
            format!("/select,\"{clean_path}\"")
        } else {
            format!("\"{clean_path}\"")
        };
        std::process::Command::new(explorer)
            .raw_arg(raw_argument)
            .spawn()
            .map_err(|e| {
                AppError::new(
                    "EXPLORER_FAIL",
                    format!("Không mở được File Explorer: {e}"),
                )
            })?;
        Ok(true)
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = clean_path;
        Ok(true)
    }
}

#[tauri::command]
pub fn emit_structure_changed(app: AppHandle, state: State<'_, AppState>) -> AppResult<bool> {
    if let Ok(p) = project_path(&state) {
        let checklist = project::check_structure(&p);
        let ok = checklist.iter().all(|i| i.ok);
        let _ = app.emit(
            "structure_changed",
            serde_json::json!({ "checklist": checklist, "ok": ok }),
        );
        return Ok(true);
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompts_csv::Row;
    use std::sync::atomic::AtomicUsize;

    static CMD_TEST_IDX: AtomicUsize = AtomicUsize::new(0);

    fn temp_proj(tag: &str) -> PathBuf {
        let idx = CMD_TEST_IDX.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "videoflow_cmd_test_{}_{}_{}",
            tag,
            std::process::id(),
            idx
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        project::create_project(&dir, false).unwrap();
        dir
    }

    #[test]
    fn test_skip_row_upserts_on_brand_new_row_without_prior_runs() {
        let dir = temp_proj("skip_new");
        prompts_csv::write_all(
            &dir.join("prompts.csv"),
            &[Row {
                row_no: 2,
                anh_tham_chieu: "fresh.png".into(),
                promt_tao_image: "img".into(),
                promt_tao_video: "vid".into(),
                ..Default::default()
            }],
        )
        .unwrap();

        let fresh_key = prompts_csv::row_key_of("fresh.png", "img", "vid");
        let detail = skip_row_at(&dir, &fresh_key).expect("skip brand-new row");
        assert_eq!(detail.row.status, "Bỏ qua");

        let dry = engine::dry_run(&dir, &Settings::default()).unwrap();
        assert_eq!(dry.total, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_remap_conflict_and_delete_row_cleanup_and_run_detail_files() {
        let dir = temp_proj("remap_delete");
        std::fs::write(dir.join("input").join("old.png"), b"png1").unwrap();
        std::fs::write(dir.join("input").join("new.png"), b"png2").unwrap();
        std::fs::write(dir.join("image_out").join("old_gen.png"), b"gen_img").unwrap();
        std::fs::write(dir.join("video_out").join("old_gen.mp4"), b"gen_vid").unwrap();

        prompts_csv::write_all(
            &dir.join("prompts.csv"),
            &[Row {
                row_no: 2,
                anh_tham_chieu: "old.png".into(),
                promt_tao_image: "p1".into(),
                promt_tao_video: "p2".into(),
                anh_da_tao: "old_gen.png".into(),
                video_da_tao: "old_gen.mp4".into(),
                trang_thai: "Đã tạo video".into(),
                ..Default::default()
            }],
        )
        .unwrap();

        let run_id = {
            let conn = crate::state_db::open(&dir).unwrap();
            conn.execute(
                "INSERT INTO runs(kind, status, started_at, finished_at, total_rows, done_count, error_count)
                 VALUES ('chung', 'hoan_tat', '2026-11-18T01:00:00Z', '2026-11-18T01:02:00Z', 1, 1, 0)",
                [],
            )
            .unwrap();
            let rid = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO jobs(run_id, row_key, row_no, status, image_status, video_status)
                 VALUES (?1, 'old.png', 2, 'da_tao_video', 'xong', 'xong')",
                rusqlite::params![rid],
            )
            .unwrap();
            let jid = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
                 VALUES (?1, 'old.png', 'anh', 'old_gen.png', 'image_out/old_gen.png', 1, 1, '2026-11-18T01:00:30Z')",
                rusqlite::params![jid],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
                 VALUES (?1, 'old.png', 'video', 'old_gen.mp4', 'video_out/old_gen.mp4', 1, 1, '2026-11-18T01:01:50Z')",
                rusqlite::params![jid],
            )
            .unwrap();

            // Bản ghi mồ côi của `new.png` ở `version = 2, is_current = 1` ([P2-5])
            conn.execute(
                "INSERT INTO jobs(run_id, row_key, row_no, status, image_status, video_status)
                 VALUES (?1, 'new.png', 99, 'loi_tao_image', 'loi', 'chua_chay')",
                rusqlite::params![rid],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
                 VALUES (NULL, 'new.png', 'anh', 'orphan_v2.png', 'image_out/orphan_v2.png', 2, 1, '2026-11-18T01:00:00Z')",
                [],
            )
            .unwrap();
            rid
        };

        let (summary, run_rows) = get_run_detail_at(&dir, run_id).unwrap();
        assert_eq!(summary.duration_ms, Some(120_000));
        // Bảng jobs trong test ghi khoá legacy ('old.png'); row detail hiển thị tên ảnh
        // suy ra từ khoá nên so qua image_name.
        let old_row = run_rows.iter().find(|r| r.image_name == "old.png").unwrap();
        assert_eq!(old_row.image_file, "old_gen.png");
        assert_eq!(old_row.video_file, "old_gen.mp4");

        // remap: dữ liệu legacy trong CSV nên khoá legacy vẫn tra được (ảnh duy nhất).
        let remapped = remap_reference_image_at(&dir, "old.png", "new.png").expect("remap ok");
        // Sau remap, ảnh cột A đổi nên khoá dòng đổi theo (tên ảnh mới + hash prompt cũ).
        assert_eq!(remapped.row.image_name, "new.png");
        assert!(remapped.row.row_key.starts_with("new.png#"));
        let new_key = remapped.row.row_key.clone();
        assert_eq!(
            remapped.row.image_file, "old_gen.png",
            "orphan file_ref v2 must be purged on remap"
        );

        assert!(resolve_safe_project_target(&dir, "image_out/old_gen.png").is_ok());
        assert!(resolve_safe_project_target(&dir, "../outside.txt").is_err());

        assert!(delete_row_at(&dir, &new_key).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_save_row_prompts_trim_and_unlock_3_of_3() {
        let dir = temp_proj("save_prompts_hash");
        prompts_csv::write_all(
            &dir.join("prompts.csv"),
            &[
                Row {
                    row_no: 2,
                    anh_tham_chieu: "fresh.png".into(),
                    promt_tao_image: "img0".into(),
                    promt_tao_video: "vid0".into(),
                    ..Default::default()
                },
                Row {
                    row_no: 3,
                    anh_tham_chieu: "locked.png".into(),
                    promt_tao_image: "bad_prompt".into(),
                    promt_tao_video: "vid_locked".into(),
                    trang_thai: "Lỗi tạo image".into(),
                    ..Default::default()
                },
            ],
        )
        .unwrap();

        let _ = skip_row_at(&dir, "fresh.png").unwrap();
        // Lưu kèm khoảng trắng ở cuối trên dòng chưa từng chạy -> không được bật cờ prompt_modified sai ([P2-2])
        let d_fresh =
            save_row_prompts_at(&dir, "fresh.png", "  img0_new  \n", " vid0_new ").unwrap();
        assert!(!d_fresh.row.prompt_modified);

        {
            let conn = crate::state_db::open(&dir).unwrap();
            conn.execute(
                "INSERT INTO runs(kind, status, started_at, total_rows) VALUES ('chung', 'hoan_tat', '2026-11-18T01:00:00Z', 1)",
                [],
            )
            .unwrap();
            let rid = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO jobs(run_id, row_key, row_no, status, image_status, video_status, image_prompt_hash, video_prompt_hash, attempts_count)
                 VALUES (?1, 'locked.png', 3, 'loi_tao_image', 'loi', 'chua_chay', ?2, ?3, 3)",
                rusqlite::params![rid, engine::sha2hex("bad_prompt"), engine::sha2hex("vid_locked")],
            )
            .unwrap();
        }

        let d_locked =
            save_row_prompts_at(&dir, "locked.png", "fixed_prompt", "vid_locked").unwrap();
        assert!(d_locked.row.prompt_modified);
        assert_eq!(engine::dry_run(&dir, &Settings::default()).unwrap().total, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_import_set_version_pick_guard_and_history_export_filter() {
        let dir = temp_proj("full_option_cmds");
        let input_dir = dir.join("input");
        std::fs::write(input_dir.join("prod_a.png"), b"png_a").unwrap();
        std::fs::write(input_dir.join("prod_b.webp"), b"webp_b").unwrap();

        // 1) Quét ảnh chưa có trong prompts.csv & nhập hàng loạt
        let untracked = scan_untracked_input_images_at(&dir).unwrap();
        assert_eq!(untracked.len(), 2);

        let imported = import_input_images_at(
            &dir,
            &["prod_a.png".into(), "prod_b.webp".into()],
            "prompt anh mac dinh",
            "prompt video mac dinh",
        )
        .unwrap();
        assert_eq!(imported, 2);
        assert!(scan_untracked_input_images_at(&dir).unwrap().is_empty());

        // 2) [P0-4]: Chặn ghi đè file cùng tên trong input/ khi chọn từ thư mục ngoài
        let outside = dir.join("prod_a.png");
        std::fs::write(&outside, b"malicious_overwrite").unwrap();
        let err = copy_picked_image_into_input(&input_dir, &outside).unwrap_err();
        assert_eq!(err.code, "INPUT_FILE_EXISTS");
        assert_eq!(std::fs::read(input_dir.join("prod_a.png")).unwrap(), b"png_a");

        // 3) Chuyển đổi phiên bản sản phẩm chính (v1 <-> v2)
        std::fs::write(dir.join("image_out").join("prod_a_gen.png"), b"v1").unwrap();
        std::fs::write(dir.join("image_out").join("prod_a_gen_v2.png"), b"v2").unwrap();
        {
            let conn = crate::state_db::open(&dir).unwrap();
            conn.execute(
                "INSERT INTO runs(kind, status, started_at, total_rows) VALUES ('chung', 'hoan_tat', '2026-11-18T08:00:00+07:00', 1)",
                [],
            )
            .unwrap();
            let rid1 = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO runs(kind, status, started_at, total_rows) VALUES ('lam_lai', 'da_dung', '2026-11-18T09:00:00+07:00', 1)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
                 VALUES (NULL, 'prod_a.png', 'anh', 'prod_a_gen.png', 'image_out/prod_a_gen.png', 1, 0, '2026-11-18T08:00:00+07:00')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
                 VALUES (NULL, 'prod_a.png', 'anh', 'prod_a_gen_v2.png', 'image_out/prod_a_gen_v2.png', 2, 1, '2026-11-18T09:00:00+07:00')",
                [],
            )
            .unwrap();
            let _ = rid1;
        }

        let switched = set_current_file_version_at(&dir, "prod_a.png", "image", 1).unwrap();
        assert_eq!(switched.row.image_file, "prod_a_gen.png");
        let csv_rows = prompts_csv::read_all(&dir.join("prompts.csv")).unwrap();
        let row_a = csv_rows
            .iter()
            .find(|r| r.anh_tham_chieu == "prod_a.png")
            .unwrap();
        assert_eq!(row_a.anh_da_tao, "prod_a_gen.png");

        // 4) [P2-4]: Xuất CSV lịch sử tôn trọng bộ lọc status & kind
        let out_csv = dir.join("history_filtered.csv");
        export_history_csv_at(
            &dir,
            Some(serde_json::json!({ "status": "da_dung", "kind": "lam_lai" })),
            &out_csv.to_string_lossy(),
        )
        .unwrap();
        let exported_text = String::from_utf8(std::fs::read(&out_csv).unwrap()).unwrap();
        assert!(exported_text.contains("lam_lai"));
        assert!(!exported_text.contains(",chung,"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_ui3_probe_mp4_metadata_and_row_detail_fields() {
        let dir = temp_proj("ui3_video_detail");
        let non_exist = dir.join("missing.mp4");
        let (d_none, r_none) = probe_mp4_metadata(&non_exist);
        assert!(d_none.is_none());
        assert!(r_none.is_none());

        // Tạo mock MP4 file với mvhd (timescale=1000, duration=5000) và tkhd (720x1280)
        let mut mp4_bytes = Vec::new();
        // 1. ftyp box (20 bytes)
        mp4_bytes.extend_from_slice(&20u32.to_be_bytes());
        mp4_bytes.extend_from_slice(b"ftypisom");
        mp4_bytes.extend_from_slice(&1u32.to_be_bytes());
        mp4_bytes.extend_from_slice(b"isom");

        // 2. mvhd box (108 bytes)
        let mut mvhd_bytes = Vec::new();
        mvhd_bytes.extend_from_slice(&108u32.to_be_bytes());
        mvhd_bytes.extend_from_slice(b"mvhd");
        mvhd_bytes.push(0); // version 0
        mvhd_bytes.extend_from_slice(&[0, 0, 0]); // flags
        mvhd_bytes.extend_from_slice(&0u32.to_be_bytes()); // create_time
        mvhd_bytes.extend_from_slice(&0u32.to_be_bytes()); // mod_time
        mvhd_bytes.extend_from_slice(&1000u32.to_be_bytes()); // timescale = 1000
        mvhd_bytes.extend_from_slice(&5000u32.to_be_bytes()); // duration = 5000 (5.0s)
        mvhd_bytes.resize(108, 0); // pad rest of mvhd

        // 3. tkhd box (92 bytes)
        let mut tkhd_bytes = Vec::new();
        tkhd_bytes.extend_from_slice(&92u32.to_be_bytes());
        tkhd_bytes.extend_from_slice(b"tkhd");
        tkhd_bytes.push(0); // version 0
        tkhd_bytes.extend_from_slice(&[0, 0, 7]); // flags
        tkhd_bytes.resize(84, 0);
        tkhd_bytes.extend_from_slice(&(720u32 << 16).to_be_bytes()); // width = 720
        tkhd_bytes.extend_from_slice(&(1280u32 << 16).to_be_bytes()); // height = 1280
        assert_eq!(tkhd_bytes.len(), 92);

        // 4. trak box wrap tkhd
        let trak_size = (8 + tkhd_bytes.len()) as u32;
        let mut trak_bytes = Vec::new();
        trak_bytes.extend_from_slice(&trak_size.to_be_bytes());
        trak_bytes.extend_from_slice(b"trak");
        trak_bytes.extend_from_slice(&tkhd_bytes);

        // 5. moov box wrap mvhd + trak
        let moov_size = (8 + mvhd_bytes.len() + trak_bytes.len()) as u32;
        let mut moov_bytes = Vec::new();
        moov_bytes.extend_from_slice(&moov_size.to_be_bytes());
        moov_bytes.extend_from_slice(b"moov");
        moov_bytes.extend_from_slice(&mvhd_bytes);
        moov_bytes.extend_from_slice(&trak_bytes);

        mp4_bytes.extend_from_slice(&moov_bytes);

        let video_file = dir.join("video_out").join("test_sample.mp4");
        std::fs::write(&video_file, &mp4_bytes).unwrap();

        let (duration, resolution) = probe_mp4_metadata(&video_file);
        assert_eq!(duration, Some(5.0));
        assert_eq!(resolution, Some("720x1280".to_string()));

        // Thiết lập dự án và gọi build_row_detail_at
        let csv_path = dir.join("prompts.csv");
        std::fs::write(
            &csv_path,
            "anh_tham_chieu,promt_tao_image,promt_tao_video,anh_da_tao,video_da_tao,ngay_tao_idea,ngay_tao_anh,ngay_tao_video,trang_thai,ghi_chu_loi\n\
             test.png,prompt_img,prompt_vid,test_gen.png,test_sample.mp4,2026-11-20,2026-11-20,2026-11-20,Đã tạo video,\n",
        )
        .unwrap();

        let detail = build_row_detail_at(&dir, "test.png").unwrap();
        assert_eq!(detail.row.image_name, "test.png");
        assert_eq!(detail.row.video_file, "test_sample.mp4");
        assert_eq!(detail.prompt_image, "prompt_img");
        assert_eq!(detail.prompt_video, "prompt_vid");
        assert_eq!(detail.video_duration, Some(5.0));
        assert_eq!(detail.video_resolution, Some("720x1280".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_issue_22_multi_row_same_image_save_prompt_remap_and_file_refs_lookup() {
        let dir = std::env::temp_dir().join(format!(
            "vf_issue22_{}_{}",
            std::process::id(),
            chrono::Local::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(dir.join("input")).unwrap();
        std::fs::create_dir_all(dir.join("image_out")).unwrap();
        std::fs::create_dir_all(dir.join("video_out")).unwrap();
        std::fs::write(dir.join("input").join("shared.png"), b"png1").unwrap();
        std::fs::write(dir.join("input").join("other.png"), b"png2").unwrap();
        std::fs::write(dir.join("image_out").join("shared_r1.png"), b"gen1").unwrap();

        let csv_path = dir.join("prompts.csv");
        std::fs::write(
            &csv_path,
            "anh_tham_chieu,promt_tao_image,promt_tao_video,anh_da_tao,video_da_tao,ngay_tao_idea,ngay_tao_anh,ngay_tao_video,trang_thai,ghi_chu_loi\n\
             shared.png,img_p1,vid_p1,shared_r1.png,,,,2026-11-20,Đã tạo image,\n\
             shared.png,img_p2,vid_p2,,,,,,Đang chờ,\n",
        )
        .unwrap();

        let k1 = prompts_csv::row_key_of("shared.png", "img_p1", "vid_p1");
        let k2 = prompts_csv::row_key_of("shared.png", "img_p2", "vid_p2");

        // Bỏ qua dòng 2 và ghi nhận job + file_ref cho dòng 1
        let _ = skip_row_at(&dir, &k2).unwrap();
        {
            let conn = crate::state_db::open(&dir).unwrap();
            let run_id = ensure_seed_run_id(&conn).unwrap();
            conn.execute(
                "INSERT INTO jobs(run_id, row_key, row_no, status, image_status, video_status, image_prompt_hash, video_prompt_hash, attempts_count)
                 VALUES (?1, ?2, 2, 'da_tao_image', 'xong', 'chua_chay', ?3, ?4, 1)",
                rusqlite::params![run_id, k1, engine::sha2hex("img_p1"), engine::sha2hex("vid_p1")],
            )
            .unwrap();
            let jid = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
                 VALUES (?1, ?2, 'anh', 'shared_r1.png', 'image_out/shared_r1.png', 1, 1, '2026-11-20T10:00:00+07:00')",
                rusqlite::params![jid, k1],
            )
            .unwrap();
        }

        // Sửa prompt ảnh dòng 1 -> không được cướp/xóa trạng thái bo_qua của dòng 2
        let d1_after = save_row_prompts_at(&dir, &k1, "img_p1_edited", "vid_p1").unwrap();
        assert!(d1_after.row.prompt_modified, "dòng 1 phải bật prompt_modified sau khi sửa prompt ảnh");
        assert_eq!(d1_after.files.len(), 1, "không được mất file_refs lịch sử sau khi sửa prompt");

        let d2_check = build_row_detail_at(&dir, &k2).unwrap();
        assert_eq!(d2_check.row.status, "Bỏ qua", "dòng 2 dùng chung ảnh không được mất trạng thái Bỏ qua");

        // Remap dòng 2 sang other.png -> key mới có dạng other.png#hash và giữ nguyên dữ liệu dòng 1
        let d2_remapped = remap_reference_image_at(&dir, &k2, "other.png").unwrap();
        assert!(d2_remapped.row.row_key.starts_with("other.png#"));
        assert_eq!(d2_remapped.row.status, "Bỏ qua");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
