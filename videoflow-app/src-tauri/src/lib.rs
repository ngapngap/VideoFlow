//! VideoFlow Desktop - lõi Rust (Lớp 2 + Lớp 3 theo mục 3).
//! Giao diện Svelte chỉ gọi qua các command/sự kiện ở đây (13.2).

pub mod adapters;
pub mod commands;
pub mod csv_writer;
pub mod engine;
pub mod error;
pub mod ffmpeg;
pub mod muse_sidecar;
pub mod plugins;
pub mod project;
pub mod prompts_csv;
pub mod proxies;
pub mod runtime_services;
pub mod settings;
pub mod state_db;

use commands::AppState;
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let state = AppState::default();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(state)
        .setup(|app| {
            let resource_dir = app.path().resource_dir()?;
            let app_data_dir = settings::app_data_dir();
            plugins::host::initialize(resource_dir, app_data_dir);
            runtime_services::init_tray_and_services(app.handle())?;
            Ok(())
        })
        .on_window_event(runtime_services::handle_window_event)
        .invoke_handler(tauri::generate_handler![
            commands::list_recent_projects,
            commands::remove_recent_project,
            commands::probe_folder,
            commands::load_project,
            commands::check_structure,
            commands::pick_folder,
            commands::create_project,
            commands::scan_untracked_input_images,
            commands::import_input_images,
            commands::get_rows,
            commands::get_row_detail,
            commands::recover_row_media,
            commands::set_current_file_version,
            commands::start_dry_run,
            commands::start_run,
            commands::run_row,
            commands::rerun_row_step,
            commands::stop_run,
            commands::engine_done,
            commands::is_running,
            commands::rerun_error_rows,
            commands::reset_row_status,
            commands::remap_reference_image,
            commands::pick_image_in_input,
            commands::delete_row,
            commands::skip_row,
            commands::save_row_prompts,
            commands::list_models,
            commands::get_settings,
            commands::save_settings,
            commands::set_api_key_cmd,
            commands::check_ffmpeg,
            commands::pick_ffmpeg_binary,
            commands::hide_to_tray,
            commands::force_quit_app,
            commands::test_connection,
            commands::get_history,
            commands::get_run_detail,
            commands::pick_save_csv,
            commands::pick_save_log,
            commands::export_history_csv,
            commands::get_logs,
            commands::export_logs_txt,
            commands::open_path_in_explorer,
            commands::emit_structure_changed,
            commands::muse_sidecar_status,
            commands::muse_start_sidecar,
            commands::muse_stop_sidecar,
            commands::muse_list_accounts,
            commands::muse_add_cookie,
            commands::muse_update_account,
            commands::muse_delete_account,
            commands::muse_renew_account,
            commands::muse_reset_account,
            commands::muse_open_login,
            commands::muse_open_view,
            commands::muse_account_quota,
            commands::muse_download_extension,
            commands::proxy_list,
            commands::proxy_create,
            commands::proxy_update,
            commands::proxy_delete,
            commands::proxy_test,
            commands::proxy_test_all,
            commands::app_version,
            commands::check_app_update,
            commands::install_app_update,
            plugins::commands::plugin_list,
            plugins::commands::plugin_add_instance,
            plugins::commands::plugin_remove_instance,
            plugins::commands::plugin_set_trust,
            plugins::commands::plugin_set_enabled,
            plugins::commands::plugin_start,
            plugins::commands::plugin_stop,
            plugins::commands::plugin_status,
            plugins::commands::plugin_capabilities,
            plugins::commands::plugin_quota,
            plugins::commands::plugin_save_config,
            plugins::commands::plugin_action,
        ])
        .build(tauri::generate_context!())
        .expect("VideoFlow Desktop khởi tạo thất bại")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                use tauri::Manager;
                let st = app.state::<AppState>();
                if !st.exit_requested.load(std::sync::atomic::Ordering::SeqCst) {
                    commands::shutdown_app_state(&st, true);
                } else {
                    muse_sidecar::stop_for_quit();
                }
                plugins::host::stop_all();
            }
        });
}
