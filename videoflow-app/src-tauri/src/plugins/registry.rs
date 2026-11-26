use super::{
    config::{self, ConfigFile, InstanceConfig},
    manifest::{valid_id, PluginManifest},
};
use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginInstance {
    pub plugin_id: String,
    pub instance_id: String,
    pub enabled: bool,
    pub trusted: bool,
    pub bundled: bool,
    pub health: String,
    pub origin: Option<String>,
    pub reason: Option<String>,
    pub restart_required: bool,
    pub config: serde_json::Value,
    pub configured_secrets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginSummary {
    pub manifest: PluginManifest,
    pub bundled: bool,
    pub package_path: String,
    pub instances: Vec<PluginInstance>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Package {
    pub manifest: PluginManifest,
    pub root: PathBuf,
    pub bundled: bool,
    pub fingerprint: String,
}

#[derive(Debug, Clone)]
pub struct Registry {
    pub packages: HashMap<String, Package>,
    pub errors: Vec<String>,
    pub settings: ConfigFile,
}

impl Registry {
    pub fn scan(resource_dir: &Path, app_data_dir: &Path) -> Self {
        let cfg_path = config::config_path(app_data_dir);
        let settings = config::load(&cfg_path);
        let mut out = Self {
            packages: HashMap::new(),
            errors: Vec::new(),
            settings,
        };
        let mut candidates = Vec::new();
        let bundled_root = resource_dir.join("plugins");
        if let Ok(entries) = std::fs::read_dir(&bundled_root) {
            for entry in entries.flatten() {
                candidates.push((entry.path(), true));
            }
        }
        let user_root = app_data_dir.join("plugins");
        if let Ok(entries) = std::fs::read_dir(&user_root) {
            for entry in entries.flatten() {
                candidates.push((entry.path(), false));
            }
        }
        let mut ambiguous = HashSet::new();
        for (root, bundled) in candidates {
            if !root.is_dir() {
                continue;
            }
            if std::fs::symlink_metadata(&root)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(true)
            {
                out.errors.push(format!(
                    "{}: plugin package root is a symlink or cannot be inspected",
                    root.display()
                ));
                continue;
            }
            match PluginManifest::read(&root) {
                Ok(manifest) => {
                    if ambiguous.contains(&manifest.id) {
                        continue;
                    }
                    if out.packages.contains_key(&manifest.id) {
                        out.packages.remove(&manifest.id);
                        ambiguous.insert(manifest.id.clone());
                        out.errors.push(format!(
                            "Duplicate plugin id '{}' disables both packages",
                            manifest.id
                        ));
                        continue;
                    }
                    if let Some(min) = manifest.min_core_version.as_deref() {
                        let Ok(required) = semver::Version::parse(min) else {
                            out.errors.push(format!(
                                "Plugin '{}' declares invalid min_core_version",
                                manifest.id
                            ));
                            continue;
                        };
                        let Ok(current) = semver::Version::parse(env!("CARGO_PKG_VERSION")) else {
                            out.errors.push("Core version is not valid SemVer".into());
                            continue;
                        };
                        if current < required {
                            out.errors.push(format!(
                                "Plugin '{}' requires core {min} or newer",
                                manifest.id
                            ));
                            continue;
                        }
                    }
                    let fingerprint = match fingerprint(&root) {
                        Ok(v) => v,
                        Err(e) => {
                            out.errors.push(format!("{}: {e}", root.display()));
                            continue;
                        }
                    };
                    out.packages.insert(
                        manifest.id.clone(),
                        Package {
                            manifest,
                            root,
                            bundled,
                            fingerprint,
                        },
                    );
                }
                Err(err) => out.errors.push(format!("{}: {err}", root.display())),
            }
        }
        out
    }

    pub fn summaries(&self) -> Vec<PluginSummary> {
        let mut packages: Vec<_> = self.packages.values().collect();
        packages.sort_by(|a, b| a.manifest.id.cmp(&b.manifest.id));
        packages
            .into_iter()
            .map(|p| {
                let id = &p.manifest.id;
                let instance_map: BTreeMap<String, InstanceConfig> = self
                    .settings
                    .instances
                    .iter()
                    .filter_map(|(key, v)| {
                        key.strip_prefix(&format!("{id}:"))
                            .map(|instance| (instance.to_string(), v.clone()))
                    })
                    .collect();
                let instances = if instance_map.is_empty() {
                    vec![make_instance(p, "default", None)]
                } else {
                    instance_map
                        .iter()
                        .map(|(instance, cfg)| make_instance(p, instance, Some(cfg)))
                        .collect()
                };
                PluginSummary {
                    manifest: p.manifest.clone(),
                    bundled: p.bundled,
                    package_path: p.root.to_string_lossy().to_string(),
                    instances,
                    error: None,
                }
            })
            .collect()
    }

    pub fn get_package(&self, id: &str) -> AppResult<&Package> {
        self.packages.get(id).ok_or_else(|| {
            AppError::new("PLUGIN_NOT_FOUND", format!("Plugin '{id}' was not found"))
        })
    }
    pub fn instance(&self, plugin: &str, instance: &str) -> AppResult<InstanceConfig> {
        if !valid_id(plugin) || !valid_id(instance) {
            return Err(AppError::new(
                "PLUGIN_INSTANCE_INVALID",
                "Plugin and instance IDs must match [a-z0-9-]+",
            ));
        }
        Ok(self
            .settings
            .instances
            .get(&format!("{plugin}:{instance}"))
            .cloned()
            .unwrap_or_else(|| InstanceConfig {
                enabled: false,
                trusted: false,
                ..Default::default()
            }))
    }
}

fn make_instance(p: &Package, id: &str, cfg: Option<&InstanceConfig>) -> PluginInstance {
    let c = cfg.cloned().unwrap_or_default();
    PluginInstance {
        plugin_id: p.manifest.id.clone(),
        instance_id: id.into(),
        enabled: c.enabled,
        trusted: p.bundled
            || (c.trusted && c.approved_fingerprint.as_deref() == Some(p.fingerprint.as_str())),
        bundled: p.bundled,
        health: "stopped".into(),
        origin: None,
        reason: None,
        restart_required: c.restart_required,
        config: c.values,
        configured_secrets: c.configured_secrets,
    }
}
pub(crate) fn fingerprint(root: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    use std::fs;
    let mut h = Sha256::new();
    let base = root.canonicalize().map_err(|e| e.to_string())?;
    let mut files = Vec::new();
    fn walk(base: &Path, dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
        for item in fs::read_dir(dir).map_err(|e| e.to_string())? {
            let item = item.map_err(|e| e.to_string())?;
            let path = item.path();
            let meta = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
            if meta.file_type().is_symlink() {
                return Err(format!(
                    "Symlinks are not allowed in plugin packages: {}",
                    path.display()
                ));
            }
            if meta.is_dir() {
                walk(base, &path, files)?;
            } else if meta.is_file() {
                files.push(
                    path.strip_prefix(base)
                        .map_err(|e| e.to_string())?
                        .to_path_buf(),
                );
            } else {
                return Err(format!("Unsupported filesystem entry: {}", path.display()));
            }
        }
        Ok(())
    }
    walk(&base, &base, &mut files)?;
    files.sort();
    for rel in files {
        let path = base.join(&rel);
        h.update(rel.to_string_lossy().as_bytes());
        h.update([0]);
        let bytes = fs::read(path).map_err(|e| e.to_string())?;
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    }
    Ok(format!("{:x}", h.finalize()))
}
