//! Tauri commands quản lý lịch sử chạy, nhật ký, xuất CSV/Log và mở thư mục Explorer.

use super::*;

// ━━ Commands: Lịch sử, Chi tiết lượt, Xuất CSV/Log, Nhật ký & Explorer ━━

pub(super) fn validate_iso_date(label: &str, d: &str) -> AppResult<()> {
    if d.len() != 10 || !d.chars().all(|c| c.is_ascii_digit() || c == '-') {
        return Err(AppError::new(
            "BAD_DATE_FILTER",
            format!("{label} phải có định dạng YYYY-MM-DD"),
        ));
    }
    Ok(())
}

pub(super) fn build_history_where_clause(
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

pub(super) fn validate_export_save_path_with_approved(
    approved: &HashSet<String>,
    project_dir: &Path,
    save_path: &str,
    allowed_exts: &[&str],
) -> AppResult<PathBuf> {
    let trimmed = save_path.trim();
    if trimmed.is_empty() {
        return Err(AppError::new("EMPTY_PATH", "Đường dẫn lưu file trống."));
    }
    if trimmed.chars().any(|c| c.is_control()) {
        return Err(AppError::new(
            "FORBIDDEN_EXPORT_PATH",
            "Đường dẫn lưu file chứa ký tự điều khiển không hợp lệ.",
        ));
    }
    let pb = PathBuf::from(trimmed);
    let Some(fname) = pb.file_name().and_then(|n| n.to_str()) else {
        return Err(AppError::new(
            "FORBIDDEN_EXPORT_PATH",
            "Đường dẫn lưu file thiếu tên file hợp lệ.",
        ));
    };
    if fname.contains(':') || fname.chars().any(|c| c.is_control()) {
        return Err(AppError::new(
            "FORBIDDEN_EXPORT_PATH",
            "Tên file xuất không được chứa ':' hoặc ký tự điều khiển.",
        ));
    }
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
    if let Ok(meta) = std::fs::symlink_metadata(&pb) {
        if meta.file_type().is_symlink() {
            return Err(AppError::new(
                "FORBIDDEN_EXPORT_PATH",
                "Không cho phép ghi đè qua symlink.",
            ));
        }
    }

    let proj_canon = std::fs::canonicalize(project_dir).ok();
    let parent_canon = pb.parent().and_then(|p| std::fs::canonicalize(p).ok());
    if let (Some(ref proj_c), Some(ref par_c)) = (&proj_canon, &parent_canon) {
        if par_c.starts_with(proj_c)
            && (fname.eq_ignore_ascii_case("prompts.csv")
                || fname.eq_ignore_ascii_case("state.db")
                || fname.eq_ignore_ascii_case("state.db-wal")
                || fname.eq_ignore_ascii_case("state.db-shm"))
        {
            return Err(AppError::new(
                "FORBIDDEN_EXPORT_PATH",
                "Không cho phép ghi đè file hệ thống của dự án (prompts.csv / state.db).",
            ));
        }
    }

    let key = normalize_win_path_key(trimmed);
    if approved.contains(&key) {
        return Ok(pb);
    }

    if let (Some(ref proj_c), Some(ref par_c)) = (&proj_canon, &parent_canon) {
        if par_c.starts_with(proj_c)
            && (!pb.exists()
                || std::fs::canonicalize(&pb).is_ok_and(|target_c| target_c.starts_with(proj_c)))
        {
            return Ok(pb);
        }
    }
    Err(AppError::new(
        "FORBIDDEN_EXPORT_PATH",
        "Đường dẫn lưu file chưa được chọn qua hộp thoại hệ thống hoặc nằm ngoài thư mục dự án.",
    ))
}

pub(super) fn validate_export_save_path(
    state: &AppState,
    project_dir: &Path,
    save_path: &str,
    allowed_exts: &[&str],
) -> AppResult<PathBuf> {
    let approved = state.approved_export_paths.read();
    validate_export_save_path_with_approved(&approved, project_dir, save_path, allowed_exts)
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
    save_path: impl AsRef<Path>,
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
    std::fs::write(save_path.as_ref(), &out)
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
    let validated = validate_export_save_path(&state, &p, &save_path, &["csv"])?;
    export_history_csv_at(&p, filter, &validated)
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

pub(super) fn format_logs_txt_content(items: &[LogItem]) -> String {
    let mut text = String::from("\u{FEFF}# VideoFlow Desktop — Nhật ký kỹ thuật\r\n");
    for item in items.iter().rev() {
        let rid_str = item
            .run_id
            .map(|id| format!("run#{id}"))
            .unwrap_or_else(|| "-".into());
        let row_str = item.row_key.as_deref().unwrap_or("-");
        let step_str = item.step.as_deref().unwrap_or("-");
        let safe_msg = crate::settings::redact_secrets(&item.message);
        let detail_str = item
            .detail
            .as_deref()
            .filter(|d| !d.is_empty())
            .map(crate::settings::redact_secrets)
            .map(|d| format!(" | {d}"))
            .unwrap_or_default();
        text.push_str(&format!(
            "[{}] [{:<5}] [{}] [{}] [{}] {}{}\r\n",
            item.ts,
            item.level.to_ascii_uppercase(),
            rid_str,
            row_str,
            step_str,
            safe_msg,
            detail_str
        ));
    }
    text
}

#[tauri::command]
pub fn export_logs_txt(
    run_id: Option<i64>,
    level: Option<String>,
    save_path: String,
    state: State<'_, AppState>,
) -> AppResult<bool> {
    let p = project_path(&state)?;
    let validated = validate_export_save_path(&state, &p, &save_path, &["log", "txt"])?;
    let items = query_logs_at(&p, run_id, level.as_deref(), 5000)?;
    let text = format_logs_txt_content(&items);
    std::fs::write(&validated, text.as_bytes())
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
pub(super) fn windows_explorer_binary() -> PathBuf {
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

