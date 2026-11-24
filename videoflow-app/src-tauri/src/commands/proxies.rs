//! Tauri commands quản lý Proxy toàn cục (proxies.db) và đồng bộ sang sidecar.

use super::*;

// ━━ Commands: Quản lý Proxy toàn cục (proxies.db — Single Source of Truth) ━━

pub(super) fn check_sidecar_accounts_not_busy(
    session: &crate::muse_sidecar::SidecarSession,
    account_ids: &[String],
) -> AppResult<()> {
    if account_ids.is_empty() {
        return Ok(());
    }
    let res = crate::muse_sidecar::admin_get(session.port, &session.api_key, "/admin/accounts")
        .map_err(|e| {
            AppError::new(
                "SIDECAR_SYNC_FAIL",
                crate::settings::redact_secrets(&format!(
                    "Không kiểm tra được trạng thái tài khoản trên engine trước khi sửa/xoá proxy: {}",
                    e.message
                )),
            )
        })?;
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
    Ok(())
}

fn restore_sidecar_proxy_batch(
    session: &crate::muse_sidecar::SidecarSession,
    ids: &[String],
    old_url: &str,
) -> Option<String> {
    let rollback_body = serde_json::json!({ "proxy": old_url });
    let mut failed = Vec::new();
    for id in ids {
        if let Err(e) = crate::muse_sidecar::admin_send(
            session.port,
            &session.api_key,
            "PATCH",
            &format!("/admin/accounts/{id}"),
            Some(&rollback_body),
        ) {
            failed.push(format!("{id} ({})", e.code));
        }
    }
    if failed.is_empty() {
        None
    } else {
        Some(format!(
            "Không khôi phục được proxy sidecar cho account: {}",
            failed.join(", ")
        ))
    }
}

pub(crate) fn proxy_update_in_with_session(
    conn: &mut rusqlite::Connection,
    proxy_id: &str,
    input: &crate::proxies::ProxyInput,
    session_opt: Option<&crate::muse_sidecar::SidecarSession>,
) -> AppResult<crate::proxies::ProxyItem> {
    let existing = crate::proxies::get_proxy_in(conn, proxy_id)?;
    let norm_kind = input.kind.trim().to_ascii_lowercase();
    let raw_host = input.host.trim();
    let norm_host = raw_host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(raw_host);
    let norm_user = input
        .username
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let endpoint_or_secret_changed = existing.kind != norm_kind
        || existing.host != norm_host
        || existing.port != input.port
        || existing.username.as_deref() != norm_user
        || (input.clear_password == Some(true) && existing.has_password)
        || input.password.as_deref().is_some_and(|s| !s.is_empty());

    if endpoint_or_secret_changed {
        if let Some(session) = session_opt {
            check_sidecar_accounts_not_busy(session, &existing.assigned_account_ids)?;
        }
    }

    let old_snap = crate::proxies::snapshot_proxy_row_in(conn, proxy_id)?;
    let old_url_opt = crate::proxies::resolve_proxy_url_in(conn, proxy_id).ok();
    let updated = crate::proxies::update_proxy_in(conn, proxy_id, input)?;

    if endpoint_or_secret_changed {
        if let Some(session) = session_opt {
            if !updated.assigned_account_ids.is_empty() {
                let new_url = match crate::proxies::resolve_proxy_url_in(conn, proxy_id) {
                    Ok(u) => u,
                    Err(e) => {
                        if let Err(rb_e) = crate::proxies::restore_proxy_update_in(conn, &old_snap) {
                            eprintln!("proxy_update DB rollback failed: {}", rb_e.message);
                        }
                        return Err(e);
                    }
                };
                if old_url_opt.as_deref() != Some(new_url.as_str()) {
                    let mut patched_ids: Vec<String> = Vec::new();
                    for acc_id in &updated.assigned_account_ids {
                        let safe_id = match muse_sanitize_id(acc_id) {
                            Ok(s) => s,
                            Err(e) => {
                                let _ = crate::proxies::restore_proxy_update_in(conn, &old_snap);
                                if let Some(ref old_url) = old_url_opt {
                                    if let Some(warn) =
                                        restore_sidecar_proxy_batch(session, &patched_ids, old_url)
                                    {
                                        eprintln!("{warn}");
                                    }
                                }
                                return Err(e);
                            }
                        };
                        let body = serde_json::json!({ "proxy": &new_url });
                        match crate::muse_sidecar::admin_send(
                            session.port,
                            &session.api_key,
                            "PATCH",
                            &format!("/admin/accounts/{safe_id}"),
                            Some(&body),
                        ) {
                            Ok(_) => patched_ids.push(safe_id),
                            Err(e) if e.code == "MUSE_NOT_FOUND" => {
                                let _ = crate::proxies::remove_account_link_in(conn, acc_id);
                            }
                            Err(e) => {
                                let mut warnings = Vec::new();
                                if let Err(rb_e) =
                                    crate::proxies::restore_proxy_update_in(conn, &old_snap)
                                {
                                    warnings.push(format!("DB rollback lỗi: {}", rb_e.message));
                                }
                                if let Some(ref old_url) = old_url_opt {
                                    if let Some(warn) =
                                        restore_sidecar_proxy_batch(session, &patched_ids, old_url)
                                    {
                                        warnings.push(warn);
                                    }
                                }
                                let mut msg = crate::settings::redact_secrets(&e.message);
                                if !warnings.is_empty() {
                                    msg = format!("{msg} [Rollback warning: {}]", warnings.join("; "));
                                }
                                return Err(AppError::new(&e.code, msg));
                            }
                        }
                    }
                }
            }
        }
    }

    crate::proxies::get_proxy_in(conn, proxy_id)
}

pub(crate) fn proxy_delete_in_with_session(
    conn: &mut rusqlite::Connection,
    proxy_id: &str,
    session_opt: Option<&crate::muse_sidecar::SidecarSession>,
) -> AppResult<Vec<String>> {
    let existing = crate::proxies::get_proxy_in(conn, proxy_id)?;
    if let Some(session) = session_opt {
        check_sidecar_accounts_not_busy(session, &existing.assigned_account_ids)?;
    }

    let old_snap = crate::proxies::snapshot_proxy_row_in(conn, proxy_id)?;
    let old_url_opt = crate::proxies::resolve_proxy_url_in(conn, proxy_id).ok();
    let affected = crate::proxies::delete_proxy_in(conn, proxy_id)?;

    if let Some(session) = session_opt {
        if !affected.is_empty() {
            let mut cleared_ids: Vec<String> = Vec::new();
            for acc_id in &affected {
                let safe_id = match muse_sanitize_id(acc_id) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = crate::proxies::restore_proxy_delete_in(conn, &old_snap);
                        if let Some(ref old_url) = old_url_opt {
                            if let Some(warn) =
                                restore_sidecar_proxy_batch(session, &cleared_ids, old_url)
                            {
                                eprintln!("{warn}");
                            }
                        }
                        return Err(e);
                    }
                };
                let body = serde_json::json!({ "proxy": "" });
                match crate::muse_sidecar::admin_send(
                    session.port,
                    &session.api_key,
                    "PATCH",
                    &format!("/admin/accounts/{safe_id}"),
                    Some(&body),
                ) {
                    Ok(_) => cleared_ids.push(safe_id),
                    Err(e) if e.code == "MUSE_NOT_FOUND" => {}
                    Err(e) => {
                        let mut warnings = Vec::new();
                        if let Err(rb_e) = crate::proxies::restore_proxy_delete_in(conn, &old_snap)
                        {
                            warnings.push(format!("DB rollback lỗi: {}", rb_e.message));
                        }
                        if let Some(ref old_url) = old_url_opt {
                            if let Some(warn) =
                                restore_sidecar_proxy_batch(session, &cleared_ids, old_url)
                            {
                                warnings.push(warn);
                            }
                        }
                        let mut msg = crate::settings::redact_secrets(&e.message);
                        if !warnings.is_empty() {
                            msg = format!("{msg} [Rollback warning: {}]", warnings.join("; "));
                        }
                        return Err(AppError::new(&e.code, msg));
                    }
                }
            }
        }
    }

    Ok(affected)
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
        let _mut_guard = proxy_mutation_lock().lock();
        let mut conn = crate::proxies::open_db()?;
        let session_opt = crate::muse_sidecar::current_session();
        proxy_update_in_with_session(&mut conn, &proxy_id, &input, session_opt.as_ref())
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
        let _mut_guard = proxy_mutation_lock().lock();
        let mut conn = crate::proxies::open_db()?;
        let session_opt = crate::muse_sidecar::current_session();
        let affected = proxy_delete_in_with_session(&mut conn, &proxy_id, session_opt.as_ref())?;
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

pub(crate) fn proxy_test_all_with_urls_in(
    db_path: &Path,
    target_url: &str,
    ip_urls: &[&str],
) -> AppResult<Vec<crate::proxies::ProxyItem>> {
    let list = {
        let conn = crate::proxies::open_db_at(db_path)?;
        crate::proxies::list_proxies_in(&conn)?
    };
    let total = list.len();
    if total == 0 {
        return Ok(Vec::new());
    }

    let worker_count = total.min(6);
    let queue = parking_lot::Mutex::new(
        list.into_iter()
            .enumerate()
            .map(|(i, p)| (i, p.id))
            .collect::<std::collections::VecDeque<_>>(),
    );
    let results = parking_lot::Mutex::new(vec![None::<crate::proxies::ProxyItem>; total]);
    let first_err = parking_lot::Mutex::new(None::<AppError>);

    std::thread::scope(|s| {
        for _ in 0..worker_count {
            s.spawn(|| {
                let worker_conn = match crate::proxies::open_db_at(db_path) {
                    Ok(c) => c,
                    Err(e) => {
                        let mut slot = first_err.lock();
                        if slot.is_none() {
                            *slot = Some(e);
                        }
                        return;
                    }
                };
                loop {
                    let next = queue.lock().pop_front();
                    let Some((idx, pid)) = next else {
                        break;
                    };
                    let item_opt = match crate::proxies::test_proxy_connection_with_urls(
                        &worker_conn,
                        &pid,
                        target_url,
                        ip_urls,
                    ) {
                        Ok(updated) => Some(updated),
                        Err(e) if e.code == "PROXY_NOT_FOUND" => None,
                        Err(_) => crate::proxies::get_proxy_in(&worker_conn, &pid).ok(),
                    };
                    results.lock()[idx] = item_opt;
                }
            });
        }
    });

    if let Some(err) = first_err.into_inner() {
        return Err(err);
    }

    Ok(results.into_inner().into_iter().flatten().collect())
}

#[tauri::command]
pub async fn proxy_test_all() -> AppResult<Vec<crate::proxies::ProxyItem>> {
    tauri::async_runtime::spawn_blocking(move || {
        proxy_test_all_with_urls_in(
            &crate::proxies::default_db_path(),
            "https://muse.ai",
            &[
                "https://api.ipify.org?format=json",
                "https://icanhazip.com",
                "https://ifconfig.me/ip",
            ],
        )
    })
    .await
    .map_err(|e| AppError::new("JOIN_FAIL", format!("{e}")))?
}

