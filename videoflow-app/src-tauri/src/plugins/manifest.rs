use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub min_core_version: Option<String>,
    pub plugin_api_version: String,
    pub entry: PluginEntry,
    pub preferred_port: Option<u16>,
    pub health_path: String,
    pub capabilities_path: String,
    pub quota_path: String,
    #[serde(default)]
    pub config_schema: serde_json::Value,
    #[serde(default)]
    pub actions: Vec<PluginAction>,
    #[serde(default)]
    pub permissions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginEntry {
    pub exe: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginAction {
    pub id: String,
    pub label: String,
    #[serde(default = "default_action_scope")]
    pub scope: String,
    pub confirm: Option<serde_json::Value>,
}

fn default_action_scope() -> String {
    "instance".into()
}

impl PluginManifest {
    pub fn read(package: &Path) -> Result<Self, String> {
        let bytes = std::fs::read(package.join("manifest.json"))
            .map_err(|e| format!("manifest.json: {e}"))?;
        let manifest: Self =
            serde_json::from_slice(&bytes).map_err(|e| format!("manifest.json: {e}"))?;
        manifest.validate(package)?;
        Ok(manifest)
    }

    pub fn validate(&self, package: &Path) -> Result<(), String> {
        if !valid_id(&self.id) {
            return Err("Plugin id must match [a-z0-9-]+".into());
        }
        if self.plugin_api_version.split('.').next() != Some("1") {
            return Err("Unsupported plugin API major".into());
        }
        let rel = Path::new(&self.entry.exe);
        if rel.as_os_str().is_empty()
            || rel.is_absolute()
            || rel.components().any(|c| {
                matches!(
                    c,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err("Executable path must stay within its package".into());
        }
        let root = package.canonicalize().map_err(|e| e.to_string())?;
        let exe = package
            .join(rel)
            .canonicalize()
            .map_err(|e| format!("Executable not found: {e}"))?;
        if !exe.starts_with(root) || !exe.is_file() {
            return Err("Executable path escapes the plugin package".into());
        }
        let valid_route = |s: &str| {
            s.starts_with('/') && !s.starts_with("//") && !s.contains("..") && !s.contains('#')
        };
        if !valid_route(&self.health_path)
            || !valid_route(&self.capabilities_path)
            || !valid_route(&self.quota_path)
        {
            return Err("Required plugin routes are missing".into());
        }
        for action in &self.actions {
            if !valid_id(&action.id) || !["instance", "job"].contains(&action.scope.as_str()) {
                return Err(format!("Invalid action '{}': id or scope", action.id));
            }
        }
        Ok(())
    }

    pub fn executable(&self, package: &Path) -> PathBuf {
        package.join(&self.entry.exe)
    }
}

pub fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ids_require_lowercase_ascii() {
        assert!(valid_id("demo-plugin"));
        assert!(!valid_id("Demo"));
        assert!(!valid_id("../demo"));
    }

    #[test]
    fn manifest_rejects_escaping_executable() {
        let dir = std::env::temp_dir().join(format!("vf-plugin-manifest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let m = PluginManifest {
            id: "demo".into(),
            name: "Demo".into(),
            version: "0.1.0".into(),
            min_core_version: None,
            plugin_api_version: "1".into(),
            entry: PluginEntry {
                exe: "../outside.exe".into(),
                args: vec![],
            },
            preferred_port: None,
            health_path: "/healthz".into(),
            capabilities_path: "/caps".into(),
            quota_path: "/quota".into(),
            config_schema: serde_json::json!({"type":"object"}),
            actions: vec![],
            permissions: vec![],
        };
        assert!(m.validate(&dir).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
