//! Điều phối hàng đợi pipeline 2 tầng song song (B2 tạo ảnh / B3 tạo video) và quản lý worker.

use super::*;

// ━━ Engine chính (Pipeline 2 tầng song song, `concurrency = 1..=MAX_CONCURRENCY`) ━━

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

pub(super) struct WorkerRowCtx<'a> {
    pub(super) emitter: EventEmitter<'a>,
    pub(super) cfg: &'a Settings,
    pub(super) conn: &'a Connection,
    pub(super) csv_path: &'a Path,
    pub(super) image_out: &'a Path,
    pub(super) video_out: &'a Path,
    pub(super) stop_flag: &'a AtomicBool,
    pub(super) run_id: i64,
    pub(super) job_id: i64,
    pub(super) total: usize,
    pub(super) done_count: &'a AtomicUsize,
    pub(super) active_rows: &'a parking_lot::Mutex<Vec<ActiveRowProgress>>,
}

impl WorkerRowCtx<'_> {
    pub(super) fn emit_step_progress(&self, row_key: &str, step: &str, detail: String) {
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

    pub(super) fn clear_active_row(&self, row_key: &str) {
        self.active_rows
            .lock()
            .retain(|item| !item.row_key.eq_ignore_ascii_case(row_key));
    }

    /// Phát sự kiện `progress` phản ánh đúng danh sách dòng còn đang chạy sau khi một bước kết thúc.
    pub(super) fn emit_active_snapshot(&self, fallback_row_key: &str) {
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
pub(super) const MUSE_INTERNAL_SLOTS_PER_ACCOUNT: usize = 4;

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

pub(super) struct ImageWorkItem {
    pub(super) seq_idx: usize,
    pub(super) row: prompts_csv::Row,
    pub(super) plan: RowPlan,
}

pub(super) struct VideoWorkItem {
    pub(super) seq_idx: usize,
    pub(super) row: prompts_csv::Row,
    pub(super) plan: RowPlan,
    pub(super) job_id: Option<i64>,
    pub(super) b2_duration_ms: i64,
    pub(super) b2_completed: bool,
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

pub(super) struct VideoPipelineQueue {
    state: parking_lot::Mutex<VideoQueueInner>,
    cv: parking_lot::Condvar,
}

impl VideoPipelineQueue {
    pub(super) fn new(ordered: bool, total_slots: usize, active_image_workers: usize) -> Self {
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

    pub(super) fn push_ready(&self, item: VideoWorkItem) {
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

    pub(super) fn mark_skipped(&self, seq_idx: usize) {
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

    pub(super) fn request_stop(&self) {
        let mut st = self.state.lock();
        st.stop_requested = true;
        self.cv.notify_all();
    }

    pub(super) fn pop_next(
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

pub(super) struct ImageWorkerDropGuard<'a>(pub(super) &'a VideoPipelineQueue);

impl Drop for ImageWorkerDropGuard<'_> {
    fn drop(&mut self) {
        self.0.finish_image_worker();
    }
}

pub(super) struct PipelineShared<'a> {
    pub(super) emitter: EventEmitter<'a>,
    pub(super) cfg: &'a Settings,
    pub(super) project_path: &'a Path,
    pub(super) csv_path: &'a Path,
    pub(super) image_out: &'a Path,
    pub(super) video_out: &'a Path,
    pub(super) stop_flag: &'a AtomicBool,
    pub(super) run_id: i64,
    pub(super) total: usize,
    pub(super) done_count: &'a AtomicUsize,
    pub(super) error_count: &'a AtomicUsize,
    pub(super) stopped: &'a AtomicBool,
    pub(super) active_rows: &'a parking_lot::Mutex<Vec<ActiveRowProgress>>,
    pub(super) fatal_err: &'a parking_lot::Mutex<Option<AppError>>,
    pub(super) video_queue: &'a VideoPipelineQueue,
}

pub(super) fn update_runs_progress(
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

pub(super) fn insert_job_for_row(
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

pub(super) struct StepFailInput<'a> {
    pub(super) step: Step,
    pub(super) fail: &'a ApiFail,
    pub(super) row_key: &'a str,
    pub(super) plan: &'a RowPlan,
    pub(super) b2_completed_in_this_job: bool,
    pub(super) duration_ms: i64,
}

pub(super) fn handle_step_failure(
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

pub(super) fn initial_job_image_state(
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

