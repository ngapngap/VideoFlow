//! Dịch vụ nền của ứng dụng Desktop (Full Option):
//! - System Tray (Mở cửa sổ, Dừng an toàn, Thoát hoàn toàn)
//! - Chặn đóng cửa sổ đột ngột khi đang chạy (`CloseRequested`) hoặc thu nhỏ xuống khay
//! - Filesystem Watcher nhẹ (chu kỳ 2s, chỉ đọc metadata) phát `structure_changed` & `external_files_changed`
//! - Bộ lập lịch tự chạy (`schedule_enabled` / `schedule_hours`, phát `schedule_missed` theo TC-42 / TC-43)

use crate::commands::{self, AppState};
use crate::engine::{self, EngineConfig};
use crate::project;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, Window, WindowEvent};

pub fn show_and_focus_main(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

pub fn init_tray_and_services(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    // Đồng bộ cấu hình khởi động cùng Windows theo trạng thái đã lưu
    {
        let st = app.state::<AppState>();
        let start_win = st.settings_cache.read().start_with_windows;
        let _ = crate::settings::apply_windows_autostart(start_win);
    }

    let open_item = MenuItem::with_id(app, "tray_open", "Mở VideoFlow Desktop", true, None::<&str>)?;
    let stop_item = MenuItem::with_id(app, "tray_stop", "Dừng an toàn (STOP)", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "tray_quit", "Thoát hoàn toàn", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open_item, &stop_item, &quit_item])?;

    let mut tray_builder = TrayIconBuilder::new()
        .menu(&menu)
        .show_menu_on_left_click(false)
        .tooltip("VideoFlow Desktop");

    if let Some(icon) = app.default_window_icon().cloned() {
        tray_builder = tray_builder.icon(icon);
    }

    let _tray = tray_builder
        .on_menu_event(|app_handle, event| match event.id().as_ref() {
            "tray_open" => {
                show_and_focus_main(app_handle);
            }
            "tray_stop" => {
                let st = app_handle.state::<AppState>();
                let flag_opt = st.stop_flag.read().clone();
                if let Some(flag) = flag_opt {
                    flag.store(true, Ordering::SeqCst);
                }
            }
            "tray_quit" => {
                let st = app_handle.state::<AppState>();
                st.exit_requested.store(true, Ordering::SeqCst);
                if let Some(flag) = st.stop_flag.read().clone() {
                    flag.store(true, Ordering::SeqCst);
                }
                if let Ok(p) = commands::project_path(&st) {
                    if let Ok(conn) = crate::state_db::open(&p) {
                        let _ = crate::state_db::recover_interrupted_runs(&conn);
                    }
                }
                crate::muse_sidecar::stop_for_quit();
                app_handle.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_and_focus_main(tray.app_handle());
            }
        })
        .build(app)?;

    start_background_loop(app.clone());
    Ok(())
}

pub fn handle_window_event(window: &Window, event: &WindowEvent) {
    if window.label() != "main" {
        return;
    }
    if let WindowEvent::CloseRequested { api, .. } = event {
        let app = window.app_handle();
        let st = app.state::<AppState>();
        if st.exit_requested.load(Ordering::SeqCst) {
            return;
        }
        let minimize_to_tray = st.settings_cache.read().minimize_to_tray;
        if minimize_to_tray {
            api.prevent_close();
            let _ = window.hide();
            return;
        }
        if st.running.load(Ordering::SeqCst) {
            api.prevent_close();
            let _ = app.emit("close_requested_while_running", ());
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectFsFingerprint {
    project_path: PathBuf,
    dirs_mask: u8,
    csv_exists: bool,
    csv_len: u64,
    csv_mtime_ms: u128,
    input_file_count: usize,
    input_max_mtime_ms: u128,
}

fn mtime_ms(meta: &std::fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn compute_fs_fingerprint(proj: &Path) -> ProjectFsFingerprint {
    let input_dir = proj.join("input");
    let img_dir = proj.join("image_out");
    let vid_dir = proj.join("video_out");
    let csv_path = proj.join("prompts.csv");

    let mut dirs_mask = 0u8;
    if proj.is_dir() {
        dirs_mask |= 1;
    }
    if input_dir.is_dir() {
        dirs_mask |= 2;
    }
    if img_dir.is_dir() {
        dirs_mask |= 4;
    }
    if vid_dir.is_dir() {
        dirs_mask |= 8;
    }

    let (csv_exists, csv_len, csv_mtime_ms) = match std::fs::metadata(&csv_path) {
        Ok(m) if m.is_file() => (true, m.len(), mtime_ms(&m)),
        _ => (false, 0, 0),
    };

    let mut input_file_count = 0usize;
    let mut input_max_mtime_ms = 0u128;
    if let Ok(entries) = std::fs::read_dir(&input_dir) {
        for entry in entries.flatten() {
            if let Ok(m) = entry.metadata() {
                if m.is_file() {
                    input_file_count += 1;
                    input_max_mtime_ms = input_max_mtime_ms.max(mtime_ms(&m));
                }
            }
        }
    }

    ProjectFsFingerprint {
        project_path: proj.to_path_buf(),
        dirs_mask,
        csv_exists,
        csv_len,
        csv_mtime_ms,
        input_file_count,
        input_max_mtime_ms,
    }
}

fn start_background_loop(app: AppHandle) {
    std::thread::spawn(move || {
        let mut last_fp: Option<ProjectFsFingerprint> = None;
        let mut next_schedule_due: Option<Instant> = None;
        let mut last_schedule_hours: u64 = 0;

        loop {
            std::thread::sleep(Duration::from_secs(2));
            let st = app.state::<AppState>();
            if st.exit_requested.load(Ordering::SeqCst) {
                break;
            }

            let is_running = st.running.load(Ordering::SeqCst);
            let current_proj = st.project.read().clone();

            // 1) Filesystem Watcher (chỉ quét metadata nhẹ khi không có lượt chạy đang ghi)
            if !is_running {
                if let Some(ref proj) = current_proj {
                    let fp = compute_fs_fingerprint(proj);
                    if let Some(ref prev) = last_fp {
                        if prev.project_path == fp.project_path && prev != &fp {
                            let checklist = project::check_structure(proj);
                            let ok = checklist.iter().all(|i| i.ok);
                            let _ = app.emit(
                                "structure_changed",
                                serde_json::json!({ "checklist": checklist, "ok": ok }),
                            );
                            let _ = app.emit(
                                "external_files_changed",
                                serde_json::json!({ "path": proj.to_string_lossy() }),
                            );
                        }
                    }
                    last_fp = Some(fp);
                } else {
                    last_fp = None;
                }
            } else if let Some(ref proj) = current_proj {
                // Cập nhật baseline trong lúc đang chạy để vừa chạy xong không phát nhầm sự kiện sửa ngoài
                last_fp = Some(compute_fs_fingerprint(proj));
            }

            // 2) Bộ lập lịch tự chạy (`schedule_enabled` + `schedule_hours`)
            let cfg = st.settings_cache.read().clone();
            if !cfg.schedule_enabled {
                next_schedule_due = None;
                last_schedule_hours = 0;
                continue;
            }

            let hours = cfg.schedule_hours.max(1);
            let period = Duration::from_secs(hours.saturating_mul(3600));
            if next_schedule_due.is_none() || last_schedule_hours != hours {
                last_schedule_hours = hours;
                next_schedule_due = Some(Instant::now() + period);
                continue;
            }

            if let Some(due) = next_schedule_due {
                if Instant::now() >= due {
                    next_schedule_due = Some(Instant::now() + period);
                    if st.running.load(Ordering::SeqCst) {
                        if let Some(ref proj) = current_proj {
                            if let Ok(conn) = crate::state_db::open(proj) {
                                crate::settings::log(
                                    &conn,
                                    "warn",
                                    None,
                                    None,
                                    None,
                                    "Bỏ lỡ nhịp tự chạy theo lịch vì lượt chạy trước vẫn chưa kết thúc (TC-43)",
                                );
                            }
                        }
                        let _ = app.emit(
                            "schedule_missed",
                            serde_json::json!({
                                "reason": "Lượt chạy trước vẫn đang thực hiện — bỏ qua nhịp tự chạy theo lịch này."
                            }),
                        );
                    } else if let Some(ref proj) = current_proj {
                        if engine::dry_run(proj, &cfg).is_ok_and(|d| d.total > 0) {
                            let _ = commands::spawn_engine_internal(
                                app.clone(),
                                &st,
                                EngineConfig {
                                    kind: "lich",
                                    row_keys: Vec::new(),
                                    force_step: None,
                                    reset_error_attempts: false,
                                },
                            );
                        }
                    }
                }
            }
        }
    });
}
