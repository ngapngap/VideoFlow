//! Lập kế hoạch chạy cho từng dòng (B1) và chạy thử Dry-run.

use super::*;

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

pub(super) fn finish_job_status(
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
    pub ffmpeg_missing_for_auto_merge: bool,
    pub ffmpeg_missing_for_noi_canh: bool,
    pub ffmpeg_hint: Option<String>,
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
    let mut has_noi_canh = false;
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
            if r.noi_canh {
                has_noi_canh = true;
            }
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
    let ffmpeg_err = if cfg.auto_merge || has_noi_canh {
        crate::ffmpeg::resolve_ffmpeg_binary(&cfg.ffmpeg_path).err()
    } else {
        None
    };
    let ffmpeg_missing = ffmpeg_err.is_some();
    Ok(DryRunResult {
        groups,
        total,
        is_csv_locked,
        ffmpeg_missing_for_auto_merge: cfg.auto_merge && ffmpeg_missing,
        ffmpeg_missing_for_noi_canh: has_noi_canh && ffmpeg_missing,
        ffmpeg_hint: ffmpeg_err,
    })
}

