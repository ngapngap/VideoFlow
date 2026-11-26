use super::{
    host::{self, PluginStatus},
    registry::{PluginInstance, PluginSummary},
};
use crate::error::{AppError, AppResult};
use serde_json::Value;
use std::collections::HashMap;

#[tauri::command]
pub fn plugin_list() -> AppResult<Vec<PluginSummary>> {
    host::list()
}
#[tauri::command]
pub fn plugin_add_instance(plugin_id: String, instance_id: String) -> AppResult<PluginInstance> {
    host::add_instance(&plugin_id, &instance_id)
}
#[tauri::command]
pub fn plugin_remove_instance(plugin_id: String, instance_id: String) -> AppResult<bool> {
    host::remove_instance(&plugin_id, &instance_id)
}
#[tauri::command]
pub fn plugin_set_trust(
    plugin_id: String,
    instance_id: String,
    trusted: bool,
) -> AppResult<PluginInstance> {
    host::set_trust(&plugin_id, &instance_id, trusted)
}
#[tauri::command]
pub fn plugin_set_enabled(
    plugin_id: String,
    instance_id: String,
    enabled: bool,
) -> AppResult<PluginInstance> {
    host::set_enabled(&plugin_id, &instance_id, enabled)
}
#[tauri::command]
pub async fn plugin_start(plugin_id: String, instance_id: String) -> AppResult<PluginStatus> {
    tauri::async_runtime::spawn_blocking(move || host::start(&plugin_id, &instance_id))
        .await
        .map_err(|e| AppError::new("PLUGIN_TASK", e.to_string()))?
}
#[tauri::command]
pub fn plugin_stop(plugin_id: String, instance_id: String) -> AppResult<PluginStatus> {
    host::stop(&plugin_id, &instance_id)
}
#[tauri::command]
pub fn plugin_status(plugin_id: String, instance_id: String) -> AppResult<PluginStatus> {
    host::status(&plugin_id, &instance_id)
}
#[tauri::command]
pub async fn plugin_capabilities(plugin_id: String, instance_id: String) -> AppResult<Value> {
    tauri::async_runtime::spawn_blocking(move || host::capabilities(&plugin_id, &instance_id))
        .await
        .map_err(|e| AppError::new("PLUGIN_TASK", e.to_string()))?
}
#[tauri::command]
pub async fn plugin_quota(plugin_id: String, instance_id: String) -> AppResult<Value> {
    tauri::async_runtime::spawn_blocking(move || host::quota(&plugin_id, &instance_id))
        .await
        .map_err(|e| AppError::new("PLUGIN_TASK", e.to_string()))?
}
#[tauri::command]
pub fn plugin_save_config(
    plugin_id: String,
    instance_id: String,
    config: Value,
    secrets: Option<HashMap<String, String>>,
) -> AppResult<PluginInstance> {
    host::save_config(
        &plugin_id,
        &instance_id,
        config,
        secrets.unwrap_or_default(),
    )
}
#[tauri::command]
pub async fn plugin_action(
    plugin_id: String,
    instance_id: String,
    action_id: String,
    payload: Option<Value>,
) -> AppResult<Value> {
    tauri::async_runtime::spawn_blocking(move || {
        host::action(
            &plugin_id,
            &instance_id,
            &action_id,
            payload.unwrap_or_else(|| Value::Object(Default::default())),
        )
    })
    .await
    .map_err(|e| AppError::new("PLUGIN_TASK", e.to_string()))?
}
