//! Danh sách kiểm tra cấu trúc 5 mục (6.6, v1.3 chốt): input/, prompts.csv (kèm tính hợp lệ!),
//! image_out/, video_out/, quyền ghi. state.db KHÔNG nằm trong danh sách - mất/hỏng
//! thì tự sao lưu và xử lý theo 6.7c & 6.9c. Tạo dự án theo 6.7b-c.

use crate::error::{AppError, AppResult};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static WRITE_TEST_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize)]
pub struct ChecklistItem {
    pub key: String, // input | prompts | image_out | video_out | write
    pub ok: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectInfo {
    pub path: String,
    pub name: String,
    pub checklist: Vec<ChecklistItem>,
    pub row_count: usize,
}

/// Chuẩn hóa đường dẫn dự án thống nhất trên Windows:
/// - `canonicalize` nếu tồn tại trên đĩa
/// - Gỡ tiền tố verbatim `\\?\UNC\` -> `\\` và `\\?\` -> `""`
/// - Bỏ dấu phân cách thừa ở cuối (trừ gốc ổ đĩa kiểu `D:\`)
pub fn normalize_project_path(path: &Path) -> PathBuf {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let raw = resolved.to_string_lossy();
    let cleaned = if let Some(unc) = raw.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else if let Some(stripped) = raw.strip_prefix(r"\\?\") {
        stripped.to_string()
    } else {
        raw.into_owned()
    };
    PathBuf::from(trim_non_root_trailing_sep(&cleaned))
}

fn trim_non_root_trailing_sep(s: &str) -> String {
    let trimmed = s.trim();
    // Giữ nguyên gốc ổ đĩa Windows kiểu "D:\" hoặc gốc "/"
    if trimmed.len() == 3 && trimmed.as_bytes()[1] == b':' && matches!(trimmed.as_bytes()[2], b'\\' | b'/') {
        return format!("{}:\\", trimmed.chars().next().unwrap_or('C'));
    }
    if trimmed == "/" || trimmed == "\\" {
        return trimmed.to_string();
    }
    let without_sep = trimmed.trim_end_matches(['\\', '/']);
    if without_sep.len() == 2 && without_sep.as_bytes()[1] == b':' {
        return format!("{without_sep}\\");
    }
    if without_sep.is_empty() {
        trimmed.to_string()
    } else {
        without_sep.to_string()
    }
}

/// Tên hiển thị cho dự án (không bao giờ trả về chuỗi rỗng kể cả khi chọn gốc ổ đĩa `D:\`).
pub fn project_display_name(path: &Path) -> String {
    if let Some(name) = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|s| !s.trim().is_empty())
    {
        return name;
    }
    let raw = path.to_string_lossy();
    let stripped = raw.trim().trim_end_matches(['\\', '/']);
    if !stripped.is_empty() {
        stripped.to_string()
    } else {
        raw.into_owned()
    }
}

fn inspect_structure_internal(project: &Path) -> (Vec<ChecklistItem>, bool, Option<String>, usize) {
    let mut items = Vec::new();

    let input = project.join("input");
    let input_ok = input.is_dir();
    items.push(ChecklistItem {
        key: "input".into(),
        ok: input_ok,
        message: if input_ok {
            "Thư mục input/ có tồn tại".into()
        } else {
            "Thiếu thư mục input/".into()
        },
    });

    let prompts = project.join("prompts.csv");
    let mut prompts_valid = false;
    let mut prompts_error = None;
    let mut row_count = 0usize;

    if prompts.is_file() {
        match crate::prompts_csv::read_all(&prompts) {
            Ok(rows) => {
                prompts_valid = true;
                row_count = rows
                    .iter()
                    .filter(|r| !r.is_example() && !r.is_empty_content())
                    .count();
                items.push(ChecklistItem {
                    key: "prompts".into(),
                    ok: true,
                    message: format!("prompts.csv hợp lệ ({row_count} dòng việc)"),
                });
            }
            Err(e) => {
                prompts_error = Some(e.message.clone());
                items.push(ChecklistItem {
                    key: "prompts".into(),
                    ok: false,
                    message: e.message,
                });
            }
        }
    } else {
        items.push(ChecklistItem {
            key: "prompts".into(),
            ok: false,
            message: "Thiếu file prompts.csv".into(),
        });
    }

    let img_out = project.join("image_out");
    let img_ok = img_out.is_dir();
    items.push(ChecklistItem {
        key: "image_out".into(),
        ok: img_ok,
        message: if img_ok {
            "Thư mục image_out/ có tồn tại".into()
        } else {
            "Thiếu thư mục image_out/".into()
        },
    });

    let vid_out = project.join("video_out");
    let vid_ok = vid_out.is_dir();
    items.push(ChecklistItem {
        key: "video_out".into(),
        ok: vid_ok,
        message: if vid_ok {
            "Thư mục video_out/ có tồn tại".into()
        } else {
            "Thiếu thư mục video_out/".into()
        },
    });

    let (write_ok, write_msg) = write_test(project);
    items.push(ChecklistItem {
        key: "write".into(),
        ok: write_ok,
        message: write_msg,
    });

    (items, prompts_valid, prompts_error, row_count)
}

/// Kiểm tra đúng 5 mục theo 6.6 Thẻ 1 (v1.3):
/// Nếu `prompts.csv` tồn tại trên đĩa nhưng sai cấu trúc/mã hóa/trùng khóa thì mục `"prompts"` trả `ok: false` kèm thông báo lỗi cụ thể.
pub fn check_structure(project: &Path) -> Vec<ChecklistItem> {
    let norm = normalize_project_path(project);
    inspect_structure_internal(&norm).0
}

fn write_test(project: &Path) -> (bool, String) {
    if !project.is_dir() {
        return (false, "Thư mục dự án không tồn tại".into());
    }
    let seq = WRITE_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
    let probe = project.join(format!(
        ".videoflow_write_test_{}_{}",
        std::process::id(),
        seq
    ));
    match std::fs::write(&probe, b"videoflow") {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            (true, "Có quyền ghi vào thư mục dự án".into())
        }
        Err(e) => (
            false,
            format!("Không ghi được vào thư mục dự án (kiểm tra quyền ghi và dung lượng đĩa): {e}"),
        ),
    }
}

/// Trạng thái dự án khi thăm dò thư mục trước khi tạo/mở (6.7b bước 3)
#[derive(Debug, Clone, Serialize)]
pub struct ProbeResult {
    pub path: String,
    pub name: String,
    pub exists: bool,
    pub is_empty_dir: bool,
    pub has_full_structure: bool,
    pub has_partial: bool,
    pub has_prompts: bool,
    pub prompts_valid: bool,
    pub prompts_error: Option<String>,
    pub can_write: bool,
    pub row_count: usize,
    pub checklist: Vec<ChecklistItem>,
}

pub type ProjectProbe = ProbeResult;

/// Thăm dò thư mục (không tạo file/thư mục nào!):
/// - `is_empty_dir` chỉ `true` khi KHÔNG có bất kỳ mục nào trong 4 thành phần cấu trúc tồn tại trên đĩa.
/// - `has_full_structure` chỉ `true` khi đủ cả 4 thành phần trên đĩa, có quyền ghi VÀ `prompts.csv` hợp lệ.
pub fn probe_project(project: &Path) -> ProbeResult {
    let norm = normalize_project_path(project);
    let exists = norm.is_dir();
    let (checklist, prompts_valid, prompts_error, row_count) = inspect_structure_internal(&norm);

    let input_exists = norm.join("input").is_dir();
    let has_prompts = norm.join("prompts.csv").is_file();
    let img_exists = norm.join("image_out").is_dir();
    let vid_exists = norm.join("video_out").is_dir();

    let disk_struct_count = [input_exists, has_prompts, img_exists, vid_exists]
        .into_iter()
        .filter(|b| *b)
        .count();

    let can_write = checklist
        .iter()
        .find(|i| i.key == "write")
        .map(|i| i.ok)
        .unwrap_or(false);

    let is_empty_dir = exists && disk_struct_count == 0;
    let has_full_structure = exists && disk_struct_count == 4 && prompts_valid && can_write;
    let has_partial = exists && disk_struct_count > 0 && !has_full_structure;

    ProbeResult {
        path: norm.to_string_lossy().into_owned(),
        name: project_display_name(&norm),
        exists,
        is_empty_dir,
        has_full_structure,
        has_partial,
        has_prompts,
        prompts_valid,
        prompts_error,
        can_write,
        row_count,
        checklist,
    }
}

/// Mở `state.db`, nếu file `state.db` bị hỏng (corrupt) thì tự sao lưu sang `state.db.corrupt_<ts>` và khởi tạo lại sạch sẽ (6.9c).
pub fn open_or_recover_db(project: &Path) -> AppResult<(rusqlite::Connection, bool)> {
    match crate::state_db::open(project) {
        Ok(conn) => Ok((conn, false)),
        Err(first_err) => {
            let db_file = project.join("state.db");
            if db_file.exists() {
                let ts = chrono::Local::now().format("%Y%m%d_%H%M%S_%3f");
                let corrupt_backup = project.join(format!("state.db.corrupt_{ts}"));
                let _ = std::fs::rename(&db_file, &corrupt_backup);
                for suffix in ["state.db-wal", "state.db-shm"] {
                    let aux = project.join(suffix);
                    if aux.exists() {
                        let _ = std::fs::remove_file(&aux);
                    }
                }
                let conn = crate::state_db::open(project).map_err(|e| {
                    AppError::new(
                        "DB_INIT_FAIL",
                        format!("state.db bị hỏng ({first_err}) và không thể tái tạo mới: {e}"),
                    )
                })?;
                Ok((conn, true))
            } else {
                Err(AppError::new(
                    "DB_INIT_FAIL",
                    format!("Không khởi tạo được state.db: {first_err}"),
                ))
            }
        }
    }
}

/// Đọc thông tin dự án tại đường dẫn đã tồn tại (không ghi thêm thư mục mẫu).
pub fn load_project(project: &Path) -> AppResult<ProjectInfo> {
    let norm = normalize_project_path(project);
    if !norm.is_dir() {
        return Err(AppError::new(
            "PROJECT_MISSING",
            "Không tìm thấy thư mục dự án tại đường dẫn đã lưu.",
        ));
    }
    let (checklist, _prompts_valid, _prompts_error, row_count) = inspect_structure_internal(&norm);

    Ok(ProjectInfo {
        path: norm.to_string_lossy().into_owned(),
        name: project_display_name(&norm),
        checklist,
        row_count,
    })
}

/// Tạo dự án: tự tạo đúng cấu trúc bên trong thư mục gốc người dùng chọn (6.7b).
/// `overwrite` = true (`"ghi_de"`) đổi tên `prompts.csv` cũ sang `prompts.backup_YYYYMMDD_HHMMSS_mmm.csv`,
/// xóa `state.db` (`-wal`, `-shm`) cũ rồi khởi tạo mới, KHÔNG đụng tới file ảnh/video trong `input/`, `image_out/`, `video_out/`.
pub fn create_project(project: &Path, overwrite: bool) -> AppResult<Vec<ChecklistItem>> {
    let norm = normalize_project_path(project);
    if !norm.is_dir() {
        return Err(AppError::new("DIR_NOT_FOUND", "Thư mục gốc không tồn tại."));
    }

    let probe = write_test(&norm);
    if !probe.0 {
        return Err(AppError::new("NO_WRITE_PERMISSION", probe.1));
    }

    for dir in ["input", "image_out", "video_out"] {
        std::fs::create_dir_all(norm.join(dir)).map_err(|e| {
            AppError::new(
                "CREATE_DIR_FAIL",
                format!("Không tạo được thư mục {dir}/: {e}"),
            )
        })?;
    }

    let csv_path = norm.join("prompts.csv");
    if overwrite {
        if csv_path.exists() {
            let ts = chrono::Local::now().format("%Y%m%d_%H%M%S_%3f");
            let mut backup = norm.join(format!("prompts.backup_{ts}.csv"));
            let mut suffix = 1u32;
            while backup.exists() {
                backup = norm.join(format!("prompts.backup_{ts}_{suffix}.csv"));
                suffix += 1;
            }
            std::fs::rename(&csv_path, &backup).map_err(|e| {
                AppError::new(
                    "BACKUP_FAIL",
                    format!(
                        "Không sao lưu (đổi tên) được prompts.csv cũ (kiểm tra xem file có đang mở trong Excel không): {e}"
                    ),
                )
            })?;
        }

        for db_name in ["state.db", "state.db-wal", "state.db-shm"] {
            let db_file = norm.join(db_name);
            if db_file.exists() {
                std::fs::remove_file(&db_file).map_err(|e| {
                    AppError::new(
                        "DB_RESET_FAIL",
                        format!("Không làm mới được {db_name}: {e}"),
                    )
                })?;
            }
        }
    }

    if !csv_path.exists() {
        crate::prompts_csv::create_sample(&csv_path)?;
    }

    let db_existed = norm.join("state.db").exists();
    let (conn, recovered_corrupt) = open_or_recover_db(&norm)?;
    let _ = crate::state_db::recover_interrupted_runs(&conn);
    drop(conn);

    let mut checklist = check_structure(&norm);
    if recovered_corrupt {
        checklist.push(ChecklistItem {
            key: "state_db_recovered".into(),
            ok: true,
            message: "state.db cũ bị hỏng đã được sao lưu sang state.db.corrupt_* và tạo mới sạch sẽ".into(),
        });
    } else if !db_existed {
        checklist.push(ChecklistItem {
            key: "state_db_new".into(),
            ok: true,
            message: "state.db được tạo mới - toàn bộ dòng được coi là chưa chạy (6.7c)".into(),
        });
    }
    Ok(checklist)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompts_csv::{self, Row};
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir(name: &str) -> PathBuf {
        let idx = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "videoflow_project_test_{}_{}_{}",
            name,
            std::process::id(),
            idx
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp project dir");
        dir
    }

    #[test]
    fn test_normalize_path_and_drive_root_display_name() {
        assert_eq!(project_display_name(Path::new(r"D:\")), "D:");
        let cleaned = trim_non_root_trailing_sep(r"D:\Projects\Demo\");
        assert_eq!(cleaned, r"D:\Projects\Demo");
        assert_eq!(trim_non_root_trailing_sep(r"D:\"), r"D:\");
    }

    #[test]
    fn test_probe_project_empty_partial_invalid_csv_and_complete() {
        let dir = temp_dir("probe");

        // 1) Thư mục rỗng
        let p_empty = probe_project(&dir);
        assert!(p_empty.exists);
        assert!(p_empty.can_write);
        assert!(p_empty.is_empty_dir);
        assert!(!p_empty.has_partial);
        assert!(!p_empty.has_full_structure);
        assert!(!p_empty.prompts_valid);
        assert!(p_empty.prompts_error.is_none());

        // 2) Đủ 4 thành phần trên đĩa nhưng prompts.csv thiếu cột -> has_full_structure = false, prompts_valid = false!
        for d in ["input", "image_out", "video_out"] {
            fs::create_dir_all(dir.join(d)).expect("mkdir");
        }
        let csv = dir.join("prompts.csv");
        fs::write(&csv, b"cot_sai_1,cot_sai_2\r\na.png,b\r\n").expect("write invalid csv");

        let p_bad_csv = probe_project(&dir);
        assert!(!p_bad_csv.is_empty_dir, "files exist on disk so not empty");
        assert!(p_bad_csv.has_partial);
        assert!(!p_bad_csv.has_full_structure);
        assert!(!p_bad_csv.prompts_valid);
        assert!(p_bad_csv.prompts_error.is_some());
        let prompts_item = p_bad_csv
            .checklist
            .iter()
            .find(|i| i.key == "prompts")
            .unwrap();
        assert!(!prompts_item.ok);

        // 3) Ghi lại prompts.csv chuẩn -> has_full_structure = true, prompts_valid = true
        fs::remove_file(&csv).unwrap();
        prompts_csv::write_all(
            &csv,
            &[
                Row {
                    row_no: 2,
                    anh_tham_chieu: "EXAMPLE_mau.png".into(),
                    promt_tao_image: "ex".into(),
                    ..Default::default()
                },
                Row {
                    row_no: 3,
                    anh_tham_chieu: "a.png".into(),
                    promt_tao_image: "p1".into(),
                    ..Default::default()
                },
            ],
        )
        .expect("write valid prompts");

        let p_full = probe_project(&dir);
        assert!(!p_full.is_empty_dir);
        assert!(!p_full.has_partial);
        assert!(p_full.has_full_structure);
        assert!(p_full.prompts_valid);
        assert!(p_full.prompts_error.is_none());
        assert_eq!(p_full.row_count, 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_create_project_modes_and_corrupt_state_db_recovery() {
        let dir = temp_dir("create_modes");

        create_project(&dir, false).expect("initial create_project");
        fs::write(dir.join("input").join("user_photo.png"), b"fake_png").expect("write user photo");
        let csv_path = dir.join("prompts.csv");
        prompts_csv::write_all(
            &csv_path,
            &[Row {
                row_no: 2,
                anh_tham_chieu: "user_photo.png".into(),
                promt_tao_image: "giữ lại dòng này".into(),
                promt_tao_video: "video".into(),
                ..Default::default()
            }],
        )
        .expect("write real row");

        // Giả lập state.db bị hỏng nặng (corrupt bytes) rồi gọi create_project("giu_lai")
        fs::write(dir.join("state.db"), b"NOT_A_VALID_SQLITE_HEADER_CORRUPT_BYTES")
            .expect("corrupt state.db");
        create_project(&dir, false).expect("create_project recovers corrupt state.db");

        let corrupt_backups: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("state.db.corrupt_"))
            .collect();
        assert_eq!(corrupt_backups.len(), 1, "must backup corrupt state.db");

        // Chế độ "ghi_de" tạo prompts.backup_YYYYMMDD_HHMMSS_mmm.csv có mili-giây
        create_project(&dir, true).expect("create_project ghi_de");
        assert!(dir.join("input").join("user_photo.png").is_file());

        let backups: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("prompts.backup_") && name.ends_with(".csv"))
            .collect();
        assert_eq!(backups.len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }
}
