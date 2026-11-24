//! Tauri commands quản lý engine VideoFlow Muse nội bộ, pool tài khoản/cookie và hạn mức.

use super::*;

// ━━ Commands: VideoFlow Muse nội bộ (engine + pool cookie + hạn mức) ━━

pub(super) fn muse_session_or_err(cfg: &Settings) -> AppResult<(u16, String)> {
    let session = crate::muse_sidecar::ensure_running(cfg)?;
    Ok((session.port, session.api_key))
}

/// `sanitize_url_segment` trả `ApiFail` (kiểu lỗi adapter) — đổi sang `AppError`
/// theo hợp đồng command 13.2.
pub(super) fn muse_sanitize_id(id: &str) -> AppResult<String> {
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
    let muse_dir = settings::app_data_dir().join(crate::muse_sidecar::MUSE_DATA_DIR);
    let (disk_total, disk_capable) = crate::muse_sidecar::count_accounts_from_disk(&muse_dir);
    let muse_total_accounts = st.account_total.map(|n| n as usize).unwrap_or(disk_total);
    let muse_video_accounts = if st.running {
        let avail = st
            .account_available
            .map(|n| n as usize)
            .unwrap_or(disk_capable);
        disk_capable.max(avail).min(muse_total_accounts)
    } else {
        disk_capable
    };
    Ok(serde_json::json!({
        "sidecar": st,
        "video_source": cfg.video_source,
        "chromium_path": chromium,
        "headless": cfg.muse_headless,
        "muse_total_accounts": muse_total_accounts,
        "muse_video_accounts": muse_video_accounts,
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

static PROXY_MUTATION_LOCK: std::sync::OnceLock<parking_lot::Mutex<()>> =
    std::sync::OnceLock::new();

pub(super) fn proxy_mutation_lock() -> &'static parking_lot::Mutex<()> {
    PROXY_MUTATION_LOCK.get_or_init(|| parking_lot::Mutex::new(()))
}

pub(super) fn finalize_interrupted_run_in_db(p: &Path, rid: i64) -> (i64, i64) {
    let Ok(conn) = crate::state_db::open(p) else {
        return (0, 0);
    };
    let ts_now = chrono::Local::now().to_rfc3339();
    let _ = conn.execute(
        "UPDATE runs SET status = 'gian_doan', finished_at = ?1 WHERE id = ?2 AND status = 'dang_chay'",
        rusqlite::params![ts_now, rid],
    );
    let _ = conn.execute(
        "UPDATE jobs
         SET status = CASE WHEN status IN ('dang_cho', 'da_tao_image') THEN 'loi_tam_thoi' ELSE status END,
             image_status = CASE WHEN image_status = 'dang_chay' THEN 'loi' ELSE image_status END,
             video_status = CASE WHEN video_status = 'dang_chay' THEN 'loi' ELSE video_status END,
             finished_at = COALESCE(finished_at, ?1)
         WHERE run_id = ?2 AND (image_status = 'dang_chay' OR video_status = 'dang_chay')",
        rusqlite::params![ts_now, rid],
    );
    conn.query_row(
        "SELECT COALESCE(done_count, 0), COALESCE(error_count, 0) FROM runs WHERE id = ?1",
        rusqlite::params![rid],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
    )
    .unwrap_or((0, 0))
}

#[tauri::command]
pub async fn muse_stop_sidecar() -> AppResult<bool> {
    tauri::async_runtime::spawn_blocking(move || {
        crate::muse_sidecar::stop()?;
        Ok(true)
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAIL", format!("{e}")))?
}

pub(super) fn enrich_account_json_with_proxy(
    acc: &mut serde_json::Value,
    acc_to_proxy: &std::collections::HashMap<String, String>,
    proxy_by_id: &std::collections::HashMap<String, crate::proxies::ProxyItem>,
    proxy_urls: &std::collections::HashMap<String, String>,
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
            let mut synced = current_masked.as_deref() == Some(expected_masked.as_str());
            if !synced {
                match sidecar_session {
                    None => synced = true,
                    Some((port, key)) if !is_busy => {
                        if let (Ok(safe_id), Some(full_url)) =
                            (muse_sanitize_id(&acc_id), proxy_urls.get(&px.id))
                        {
                            let patch_body = serde_json::json!({ "proxy": full_url });
                            synced = crate::muse_sidecar::admin_send(
                                port,
                                key,
                                "PATCH",
                                &format!("/admin/accounts/{safe_id}"),
                                Some(&patch_body),
                            )
                            .is_ok();
                        }
                    }
                    Some(_) => {}
                }
            }
            if synced {
                obj.insert("proxy".into(), serde_json::Value::String(expected_masked));
            }
            return;
        }
    }

    obj.insert("proxy_id".into(), serde_json::Value::Null);
    obj.insert("proxy_name".into(), serde_json::Value::Null);
    obj.insert("proxy_kind".into(), serde_json::Value::Null);
    obj.insert("proxy_exit_ip".into(), serde_json::Value::Null);
    obj.insert("proxy_status".into(), serde_json::Value::Null);
    obj.insert("proxy_latency_ms".into(), serde_json::Value::Null);
    if current_masked.is_some() {
        if !is_busy {
            let cleared_ok = match sidecar_session {
                Some((port, key)) => match muse_sanitize_id(&acc_id) {
                    Ok(safe_id) => {
                        let patch_body = serde_json::json!({ "proxy": "" });
                        crate::muse_sidecar::admin_send(
                            port,
                            key,
                            "PATCH",
                            &format!("/admin/accounts/{safe_id}"),
                            Some(&patch_body),
                        )
                        .is_ok()
                    }
                    Err(_) => false,
                },
                None => true,
            };
            if cleared_ok {
                obj.insert("proxy".into(), serde_json::Value::Null);
            }
        }
    } else {
        obj.insert("proxy".into(), serde_json::Value::Null);
    }
}

fn enrich_accounts_slice_from_db(
    conn: &rusqlite::Connection,
    items: &mut [serde_json::Value],
    sidecar_session: Option<(u16, &str)>,
) {
    let Ok(proxies) = crate::proxies::list_proxies_in(conn) else {
        return;
    };
    let Ok(acc_to_proxy) = crate::proxies::load_account_to_proxy_map(conn) else {
        return;
    };
    let proxy_urls = crate::proxies::resolve_proxy_urls_map_best_effort_in(conn)
        .map(|(m, _)| m)
        .unwrap_or_default();
    let proxy_by_id: std::collections::HashMap<String, crate::proxies::ProxyItem> =
        proxies.into_iter().map(|p| (p.id.clone(), p)).collect();
    for item in items.iter_mut() {
        enrich_account_json_with_proxy(
            item,
            &acc_to_proxy,
            &proxy_by_id,
            &proxy_urls,
            sidecar_session,
        );
    }
}

pub(super) fn muse_list_accounts_with_session_and_db<FOpen>(
    session: &crate::muse_sidecar::SidecarSession,
    open_db: FOpen,
) -> AppResult<serde_json::Value>
where
    FOpen: FnOnce() -> AppResult<rusqlite::Connection>,
{
    let _mut_guard = proxy_mutation_lock().lock();
    let mut res =
        crate::muse_sidecar::admin_get(session.port, &session.api_key, "/admin/accounts")?;
    if let Ok(conn) = open_db() {
        if let Some(arr) = res.get_mut("data").and_then(|d| d.as_array_mut()) {
            enrich_accounts_slice_from_db(
                &conn,
                arr,
                Some((session.port, &session.api_key)),
            );
        }
    }
    Ok(res)
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
        muse_list_accounts_with_session_and_db(&session, crate::proxies::open_db)
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
        let _mut_guard = proxy_mutation_lock().lock();
        let clean_pid = proxy_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let resolved_proxy_url = if let Some(ref pid) = clean_pid {
            let conn = crate::proxies::open_db()?;
            Some(crate::proxies::resolve_proxy_url_in(&conn, pid)?)
        } else {
            None
        };

        let (port, key) = muse_session_or_err(&cfg)?;
        let mut body = serde_json::json!({
            "label": label.trim(),
            "cookies": cookies,
        });
        if let Some(ref url) = resolved_proxy_url {
            if let Some(obj) = body.as_object_mut() {
                obj.insert("proxy".into(), serde_json::Value::String(url.clone()));
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
                if let Some(ref pid) = clean_pid {
                    let conn = crate::proxies::open_db()?;
                    crate::proxies::set_account_proxy_in(&conn, &acc_id, Some(pid.as_str()))?;
                    enrich_accounts_slice_from_db(
                        &conn,
                        std::slice::from_mut(acc_obj),
                        Some((port, &key)),
                    );
                } else if let Ok(conn) = crate::proxies::open_db() {
                    let _ = crate::proxies::set_account_proxy_in(&conn, &acc_id, None);
                    enrich_accounts_slice_from_db(
                        &conn,
                        std::slice::from_mut(acc_obj),
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

#[allow(clippy::too_many_arguments)]
pub(super) fn muse_update_account_with_session_and_db<FGetSess, FOpenDb>(
    account_id: String,
    enabled: Option<bool>,
    label: Option<String>,
    proxy: Option<String>,
    proxy_id: Option<String>,
    get_session: FGetSess,
    mut open_db: FOpenDb,
) -> AppResult<serde_json::Value>
where
    FGetSess: FnOnce() -> AppResult<(u16, String)>,
    FOpenDb: FnMut() -> AppResult<rusqlite::Connection>,
{
    let _mut_guard = proxy_mutation_lock().lock();
    let (port, key) = get_session()?;
    let safe_id = muse_sanitize_id(&account_id)?;
    let mut body = serde_json::Map::new();
    if let Some(e) = enabled {
        body.insert("enabled".into(), serde_json::Value::Bool(e));
    }
    if let Some(l) = label {
        body.insert("label".into(), serde_json::Value::String(l.trim().to_string()));
    }

    let mut target_proxy_id: Option<Option<String>> = None;
    let has_raw_proxy =
        proxy_id.is_none() && proxy.as_deref().is_some_and(|p| !p.trim().is_empty());
    let mut prev_proxy_url_for_rb: Option<String> = None;

    if let Some(pid_raw) = proxy_id {
        let conn = open_db()?;
        if let Ok(map) = crate::proxies::load_account_to_proxy_map(&conn) {
            prev_proxy_url_for_rb = Some(
                map.get(&safe_id)
                    .and_then(|old_pid| crate::proxies::resolve_proxy_url_in(&conn, old_pid).ok())
                    .unwrap_or_default(),
            );
        }
        let trimmed = pid_raw.trim();
        if trimmed.is_empty() {
            body.insert("proxy".into(), serde_json::Value::String(String::new()));
            target_proxy_id = Some(None);
        } else {
            let px = crate::proxies::get_proxy_in(&conn, trimmed)?;
            let url = crate::proxies::resolve_proxy_url_in(&conn, &px.id)?;
            body.insert("proxy".into(), serde_json::Value::String(url));
            target_proxy_id = Some(Some(px.id));
        }
    } else if let Some(p) = proxy {
        let trimmed = p.trim();
        body.insert("proxy".into(), serde_json::Value::String(trimmed.to_string()));
        if trimmed.is_empty() {
            target_proxy_id = Some(None);
        }
    }

    let path = format!("/admin/accounts/{safe_id}");
    let mut res = crate::muse_sidecar::admin_send(
        port,
        &key,
        "PATCH",
        &path,
        Some(&serde_json::Value::Object(body)),
    )?;

    if let Some(ref pid_opt) = target_proxy_id {
        let conn = match open_db() {
            Ok(c) => c,
            Err(e) => {
                if let Some(ref old_u) = prev_proxy_url_for_rb {
                    let rb_body = serde_json::json!({ "proxy": old_u });
                    let _ = crate::muse_sidecar::admin_send(
                        port,
                        &key,
                        "PATCH",
                        &path,
                        Some(&rb_body),
                    );
                }
                return Err(e);
            }
        };
        if let Err(e) = crate::proxies::set_account_proxy_in(&conn, &safe_id, pid_opt.as_deref()) {
            if let Some(ref old_u) = prev_proxy_url_for_rb {
                let rb_body = serde_json::json!({ "proxy": old_u });
                let _ = crate::muse_sidecar::admin_send(
                    port,
                    &key,
                    "PATCH",
                    &path,
                    Some(&rb_body),
                );
            }
            return Err(e);
        }
        if !has_raw_proxy {
            if let Some(acc_obj) = res.get_mut("account") {
                enrich_accounts_slice_from_db(
                    &conn,
                    std::slice::from_mut(acc_obj),
                    Some((port, &key)),
                );
            }
        }
    } else if !has_raw_proxy {
        if let Ok(conn) = open_db() {
            if let Some(acc_obj) = res.get_mut("account") {
                enrich_accounts_slice_from_db(
                    &conn,
                    std::slice::from_mut(acc_obj),
                    Some((port, &key)),
                );
            }
        }
    }
    Ok(res)
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
        muse_update_account_with_session_and_db(
            account_id,
            enabled,
            label,
            proxy,
            proxy_id,
            || muse_session_or_err(&cfg),
            crate::proxies::open_db,
        )
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
        let _mut_guard = proxy_mutation_lock().lock();
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

