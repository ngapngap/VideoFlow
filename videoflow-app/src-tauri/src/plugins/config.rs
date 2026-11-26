use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InstanceConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub trusted: bool,
    #[serde(default)]
    pub approved_fingerprint: Option<String>,
    #[serde(default)]
    pub values: serde_json::Value,
    #[serde(default)]
    pub configured_secrets: Vec<String>,
    #[serde(default)]
    pub restart_required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConfigFile {
    #[serde(default)]
    pub instances: BTreeMap<String, InstanceConfig>,
}

pub fn load(path: &Path) -> ConfigFile {
    fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save_atomic(path: &Path, data: &ConfigFile) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| AppError::new("PLUGIN_CONFIG", e.to_string()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(data)
        .map_err(|e| AppError::new("PLUGIN_CONFIG", e.to_string()))?;
    let mut f =
        fs::File::create(&tmp).map_err(|e| AppError::new("PLUGIN_CONFIG", e.to_string()))?;
    f.write_all(&bytes)
        .and_then(|_| f.sync_all())
        .map_err(|e| AppError::new("PLUGIN_CONFIG", e.to_string()))?;
    fs::rename(&tmp, path).map_err(|e| AppError::new("PLUGIN_CONFIG", e.to_string()))?;
    Ok(())
}

pub fn credential_key(plugin: &str, instance: &str, field: &str) -> String {
    format!("plugin:{plugin}:{instance}:{field}")
}
pub fn put_secret(plugin: &str, instance: &str, field: &str, value: &str) -> AppResult<()> {
    let entry = keyring::Entry::new(
        "VideoFlow Desktop Plugin",
        &credential_key(plugin, instance, field),
    )
    .map_err(|e| AppError::new("PLUGIN_CREDENTIAL", e.to_string()))?;
    if value.is_empty() {
        let _ = entry.delete_credential();
    } else {
        entry
            .set_password(value)
            .map_err(|e| AppError::new("PLUGIN_CREDENTIAL", e.to_string()))?;
    }
    Ok(())
}
pub fn get_secret(plugin: &str, instance: &str, field: &str) -> Option<String> {
    keyring::Entry::new(
        "VideoFlow Desktop Plugin",
        &credential_key(plugin, instance, field),
    )
    .ok()?
    .get_password()
    .ok()
}
pub fn config_path(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join("plugins.json")
}
