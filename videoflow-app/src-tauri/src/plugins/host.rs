use super::{
    config::{self, InstanceConfig},
    manifest::{valid_id, PluginManifest},
    registry::{Package, PluginInstance, PluginSummary, Registry},
};
use crate::error::{AppError, AppResult};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, OnceLock},
    thread,
    time::{Duration, Instant},
};

static MANAGER: OnceLock<Mutex<Manager>> = OnceLock::new();
static OWNER_LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
struct Manager {
    resource_dir: PathBuf,
    app_data_dir: PathBuf,
    stopped: std::collections::HashSet<String>,
    children: HashMap<String, ManagedChild>,
    runtime: HashMap<String, Runtime>,
    scan_cache: Option<(Instant, HashMap<String, Package>, Vec<String>)>,
}
struct ManagedChild {
    child: Child,
    _job: ProcessJob,
}

#[cfg(windows)]
struct ProcessJob(windows_sys::Win32::Foundation::HANDLE);
#[cfg(not(windows))]
struct ProcessJob;

#[cfg(windows)]
unsafe impl Send for ProcessJob {}

#[cfg(windows)]
impl Drop for ProcessJob {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

impl ProcessJob {
    fn attach(child: &mut Child) -> AppResult<Self> {
        #[cfg(windows)]
        {
            use std::{ffi::c_void, os::windows::io::AsRawHandle};
            use windows_sys::Win32::System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
                SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            };
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(AppError::new(
                    "PLUGIN_JOB",
                    "Could not create plugin process job",
                ));
            }
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let configured = unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as *const c_void,
                    std::mem::size_of_val(&limits) as u32,
                )
            } != 0;
            let assigned = configured
                && unsafe {
                    AssignProcessToJobObject(
                        handle,
                        child.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
                    )
                } != 0;
            if !assigned {
                unsafe {
                    windows_sys::Win32::Foundation::CloseHandle(handle);
                };
                return Err(AppError::new(
                    "PLUGIN_JOB",
                    "Could not attach plugin process to its job",
                ));
            }
            Ok(ProcessJob(handle))
        }
        #[cfg(not(windows))]
        {
            let _ = child;
            Ok(ProcessJob)
        }
    }
}
#[derive(Clone)]
struct Runtime {
    config: serde_json::Value,
    origin: String,
    token: String,
    health: Health,
    pid: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Health {
    pub status: String,
    #[serde(default)]
    pub checks: serde_json::Value,
    #[serde(default)]
    pub message: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginStatus {
    pub plugin_id: String,
    pub instance_id: String,
    pub running: bool,
    pub health: String,
    pub origin: Option<String>,
    pub reason: Option<String>,
    pub pid: Option<u32>,
    pub checks: Option<serde_json::Value>,
}
#[derive(Debug, Clone)]
pub struct Session {
    pub origin: String,
    pub token: String,
    pub config: serde_json::Value,
    pub health_path: String,
    pub capabilities_path: String,
    pub quota_path: String,
}

pub fn initialize(resource_dir: PathBuf, app_data_dir: PathBuf) {
    let _ = std::fs::create_dir_all(&app_data_dir);
    let m = Manager {
        resource_dir,
        app_data_dir,
        stopped: Default::default(),
        children: HashMap::new(),
        runtime: HashMap::new(),
        scan_cache: None,
    };
    if let Some(lock) = MANAGER.get() {
        *lock.lock() = m;
    } else {
        let _ = MANAGER.set(Mutex::new(m));
    }
}
fn with_manager<T>(f: impl FnOnce(&mut Manager) -> AppResult<T>) -> AppResult<T> {
    let lock = MANAGER.get().ok_or_else(|| {
        AppError::new(
            "PLUGIN_NOT_INITIALIZED",
            "Plugin host has not been initialized",
        )
    })?;
    f(&mut lock.lock())
}
fn key(plugin: &str, instance: &str) -> String {
    format!("{plugin}:{instance}")
}
fn owner_lock(id: &str) -> Arc<Mutex<()>> {
    OWNER_LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .entry(id.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}
fn owner_key(owner: &str) -> AppResult<(String, String)> {
    let parts: Vec<_> = owner.split(':').collect();
    if parts.len() != 3 || parts[0] != "plugin" || !valid_id(parts[1]) || !valid_id(parts[2]) {
        return Err(AppError::new(
            "PLUGIN_OWNER_INVALID",
            "Invalid plugin owner",
        ));
    }
    Ok((parts[1].into(), parts[2].into()))
}
fn registry(m: &mut Manager) -> Registry {
    if m.scan_cache
        .as_ref()
        .map_or(true, |(at, _, _)| at.elapsed() > Duration::from_secs(30))
    {
        let scanned = Registry::scan(&m.resource_dir, &m.app_data_dir);
        m.scan_cache = Some((Instant::now(), scanned.packages, scanned.errors));
    }
    let (_, packages, errors) = m.scan_cache.as_ref().expect("scan cache initialized");
    Registry {
        packages: packages.clone(),
        errors: errors.clone(),
        settings: config::load(&config::config_path(&m.app_data_dir)),
    }
}

pub fn list() -> AppResult<Vec<PluginSummary>> {
    with_manager(|m| {
        m.scan_cache = None;
        Ok(registry(m).summaries())
    })
}
pub fn owners(plugin_id: &str) -> AppResult<Vec<String>> {
    with_manager(|m| {
        let r = registry(m);
        r.get_package(plugin_id)?;
        let summaries = r.summaries();
        let p = summaries
            .into_iter()
            .find(|p| p.manifest.id == plugin_id)
            .unwrap();
        Ok(p.instances
            .into_iter()
            .filter(|i| i.enabled)
            .map(|i| format!("plugin:{plugin_id}:{}", i.instance_id))
            .collect())
    })
}
pub fn status(plugin: &str, instance: &str) -> AppResult<PluginStatus> {
    let (runtime, health_path, dead, cfg_enabled) = with_manager(|m| {
        let id = key(plugin, instance);
        let r = registry(m);
        r.get_package(plugin)?;
        let cfg = r.instance(plugin, instance)?;
        let package = r.get_package(plugin)?;
        let rt = m.runtime.get(&id).cloned();
        let dead = rt.is_some()
            && m.children
                .get_mut(&id)
                .map(|c| c.child.try_wait().ok().flatten().is_some())
                .unwrap_or(true);
        Ok((rt, package.manifest.health_path.clone(), dead, cfg.enabled))
    })?;
    if let Some(mut rt) = runtime {
        if dead {
            let _ = with_manager(|m| {
                m.runtime.remove(&key(plugin, instance));
                m.children.remove(&key(plugin, instance));
                Ok(())
            });
            return Ok(PluginStatus {
                plugin_id: plugin.into(),
                instance_id: instance.into(),
                running: false,
                health: "down".into(),
                origin: Some(rt.origin),
                reason: Some("Plugin process is not running".into()),
                pid: Some(rt.pid),
                checks: None,
            });
        }
        let probe = reqwest::blocking::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(2))
            .build();
        match probe.and_then(|client| {
            client
                .get(format!("{}{health_path}", rt.origin))
                .bearer_auth(&rt.token)
                .send()
        }) {
            Ok(resp) if resp.status().as_u16() == 200 || resp.status().as_u16() == 503 => {
                match resp.json::<Health>() {
                    Ok(h) if ["ok", "degraded", "down"].contains(&h.status.as_str()) => {
                        rt.health = h
                    }
                    _ => {
                        rt.health.status = "down".into();
                        rt.health.message = "Invalid health response".into();
                    }
                }
            }
            Ok(resp) if resp.status().as_u16() == 401 => {
                rt.health.status = "down".into();
                rt.health.message = "Plugin rejected the host session token".into();
            }
            _ => {
                rt.health.status = "down".into();
                rt.health.message = "Plugin health endpoint is unavailable".into();
            }
        }
        let _ = with_manager(|m| {
            m.runtime.insert(key(plugin, instance), rt.clone());
            Ok(())
        });
        return Ok(status_from(plugin, instance, &rt));
    }
    Ok(PluginStatus {
        plugin_id: plugin.into(),
        instance_id: instance.into(),
        running: false,
        health: "stopped".into(),
        origin: None,
        reason: if cfg_enabled {
            None
        } else {
            Some("Instance is disabled".into())
        },
        pid: None,
        checks: None,
    })
}

pub fn start(plugin: &str, instance: &str) -> AppResult<PluginStatus> {
    start_instance(plugin, instance, true)
}

fn start_instance(plugin: &str, instance: &str, explicit: bool) -> AppResult<PluginStatus> {
    if !valid_id(plugin) || !valid_id(instance) {
        return Err(AppError::new(
            "PLUGIN_INSTANCE_INVALID",
            "Plugin and instance IDs must match [a-z0-9-]+",
        ));
    }
    let id = key(plugin, instance);
    let instance_lock = owner_lock(&id);
    let _instance_guard = instance_lock.lock();
    let start_data = with_manager(|m| {
        if explicit {
            m.stopped.remove(&id);
        } else if m.stopped.contains(&id) {
            return Err(AppError::new("PLUGIN_STOPPED", "Instance has been stopped"));
        }
        if let Some(rt) = m.runtime.get(&id).cloned() {
            let exited = m
                .children
                .get_mut(&id)
                .map(|c| c.child.try_wait().ok().flatten().is_some())
                .unwrap_or(true);
            if !exited {
                return Ok(Err(status_from(plugin, instance, &rt)));
            }
            m.runtime.remove(&id);
            m.children.remove(&id);
        }
        let r = registry(m);
        let mut p = r.get_package(plugin)?.clone();
        let cfg = r.instance(plugin, instance)?;
        if !cfg.enabled {
            return Err(AppError::new(
                "PLUGIN_DISABLED",
                "Enable this plugin instance before starting it",
            ));
        }
        if !p.bundled && !cfg.trusted {
            return Err(AppError::new(
                "PLUGIN_TRUST_REQUIRED",
                "Trust this plugin instance before its first start",
            ));
        }
        p.fingerprint = super::registry::fingerprint(&p.root)
            .map_err(|e| AppError::new("PLUGIN_PACKAGE_INVALID", e))?;
        if !p.bundled && cfg.approved_fingerprint.as_deref() != Some(p.fingerprint.as_str()) {
            return Err(AppError::new(
                "PLUGIN_REVIEW_REQUIRED",
                "Plugin package changed; review and trust it again",
            ));
        }
        let configured_port = cfg
            .values
            .get("bridge_port")
            .and_then(|v| v.as_u64())
            .and_then(|n| u16::try_from(n).ok());
        let port = choose_port(configured_port.or(p.manifest.preferred_port).unwrap_or(0))?;
        let data_dir = m
            .app_data_dir
            .join("plugins-data")
            .join(plugin)
            .join(instance);
        std::fs::create_dir_all(&data_dir)
            .map_err(|e| AppError::new("PLUGIN_DATA_DIR", e.to_string()))?;
        let config_file = data_dir.join("config.json");
        let config_bytes = serde_json::to_vec(&cfg.values)
            .map_err(|e| AppError::new("PLUGIN_CONFIG", e.to_string()))?;
        std::fs::write(&config_file, config_bytes)
            .map_err(|e| AppError::new("PLUGIN_CONFIG", e.to_string()))?;
        let token = random_token()?;
        let secret_values = secret_values(plugin, instance, &p.manifest);
        let exe = p.manifest.executable(&p.root);
        let args: Vec<String> = p
            .manifest
            .entry
            .args
            .iter()
            .map(|s| {
                s.replace("{port}", &port.to_string())
                    .replace("{data_dir}", &data_dir.to_string_lossy())
                    .replace("{config_file}", &config_file.to_string_lossy())
            })
            .collect();
        let mut child = Command::new(exe)
            .args(args)
            .env("VF_PLUGIN_TOKEN", &token)
            .env(
                "VF_PLUGIN_SECRETS_JSON",
                serde_json::to_string(&secret_values).unwrap_or_else(|_| "{}".into()),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                AppError::new(
                    "PLUGIN_SPAWN",
                    format!("Could not start plugin process: {e}"),
                )
            })?;
        let pid = child.id();
        let job = match ProcessJob::attach(&mut child) {
            Ok(job) => job,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        };
        m.children
            .insert(id.clone(), ManagedChild { child, _job: job });
        let origin = format!("http://127.0.0.1:{port}");
        Ok(Ok((origin, token, pid, p.manifest.health_path, cfg.values)))
    })?;
    let (origin, token, pid, health_path, config) = match start_data {
        Err(status) => return Ok(status),
        Ok(data) => data,
    };
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|e| AppError::new("PLUGIN_HTTP", e.to_string()))?;
    let started = Instant::now();
    let mut health = None;
    while started.elapsed() < Duration::from_secs(90) {
        let exited = with_manager(|m| {
            Ok(m.children
                .get_mut(&id)
                .map(|c| c.child.try_wait().ok().flatten().is_some())
                .unwrap_or(true))
        })?;
        if exited {
            let _ = with_manager(|m| {
                m.children.remove(&id);
                Ok(())
            });
            return Err(AppError::new(
                "PLUGIN_EXITED",
                "Plugin process exited before health validation",
            ));
        }
        if let Ok(resp) = client
            .get(format!("{origin}{health_path}"))
            .bearer_auth(&token)
            .send()
        {
            if resp.status().as_u16() == 200 || resp.status().as_u16() == 503 {
                if let Ok(h) = resp.json::<Health>() {
                    if ["ok", "degraded", "down"].contains(&h.status.as_str()) {
                        health = Some(h);
                        break;
                    }
                }
            }
        }
        thread::sleep(Duration::from_millis(250));
    }
    let h = match health {
        Some(h) => h,
        None => {
            let _ = with_manager(|m| {
                if let Some(mut c) = m.children.remove(&id) {
                    let _ = c.child.kill();
                    drop(c._job);
                    let _ = c.child.wait();
                }
                Ok(())
            });
            return Err(AppError::new(
                "PLUGIN_HEALTH_TIMEOUT",
                "Plugin did not return authenticated health within 90 seconds",
            ));
        }
    };
    let rt = Runtime {
        config,
        origin,
        token,
        health: h,
        pid,
    };
    with_manager(|m| {
        m.runtime.insert(id, rt.clone());
        Ok(status_from(plugin, instance, &rt))
    })
}
fn status_from(plugin: &str, instance: &str, rt: &Runtime) -> PluginStatus {
    PluginStatus {
        plugin_id: plugin.into(),
        instance_id: instance.into(),
        running: true,
        health: rt.health.status.clone(),
        origin: Some(rt.origin.clone()),
        reason: Some(rt.health.message.clone()).filter(|s| !s.is_empty()),
        pid: Some(rt.pid),
        checks: Some(rt.health.checks.clone()),
    }
}

pub fn stop(plugin: &str, instance: &str) -> AppResult<PluginStatus> {
    stop_instance(plugin, instance, true)
}

fn stop_instance(plugin: &str, instance: &str, explicit: bool) -> AppResult<PluginStatus> {
    let id = key(plugin, instance);
    let instance_lock = owner_lock(&id);
    let _instance_guard = instance_lock.lock();
    with_manager(|m| {
        if explicit {
            m.stopped.insert(id.clone());
        }
        if let Some(mut c) = m.children.remove(&id) {
            let _ = c.child.kill();
            drop(c._job);
            let _ = c.child.wait();
        }
        m.runtime.remove(&id);
        Ok(PluginStatus {
            plugin_id: plugin.into(),
            instance_id: instance.into(),
            running: false,
            health: "stopped".into(),
            origin: None,
            reason: None,
            pid: None,
            checks: None,
        })
    })
}

pub fn stop_all() {
    let Some(lock) = MANAGER.get() else {
        return;
    };
    let mut m = lock.lock();
    for (_, mut child) in m.children.drain() {
        let _ = child.child.kill();
        drop(child._job);
        let _ = child.child.wait();
    }
    m.runtime.clear();
}

pub fn session(owner: &str) -> AppResult<Session> {
    let (plugin, instance) = owner_key(owner)?;
    let restart_required =
        with_manager(|m| Ok(registry(m).instance(&plugin, &instance)?.restart_required))?;
    if restart_required && !crate::plugins::routing::busy(owner) {
        stop_instance(&plugin, &instance, false)?;
        with_manager(|m| {
            let mut r = registry(m);
            let mut cfg = r.instance(&plugin, &instance)?;
            cfg.restart_required = false;
            r.settings.instances.insert(key(&plugin, &instance), cfg);
            config::save_atomic(&config::config_path(&m.app_data_dir), &r.settings)
        })?;
    }
    start_instance(&plugin, &instance, false)?;
    with_manager(|m| {
        let r = registry(m);
        let p = r.get_package(&plugin)?;
        let rt = m
            .runtime
            .get(&key(&plugin, &instance))
            .ok_or_else(|| AppError::new("PLUGIN_UNAVAILABLE", "Plugin instance is unavailable"))?;
        Ok(Session {
            origin: rt.origin.clone(),
            token: rt.token.clone(),
            config: rt.config.clone(),
            health_path: p.manifest.health_path.clone(),
            capabilities_path: p.manifest.capabilities_path.clone(),
            quota_path: p.manifest.quota_path.clone(),
        })
    })
}

pub fn save_config(
    plugin: &str,
    instance: &str,
    values: serde_json::Value,
    secrets: HashMap<String, String>,
) -> AppResult<PluginInstance> {
    with_manager(|m| {
        let mut r = registry(m);
        let p = r.get_package(plugin)?.clone();
        let old = r.instance(plugin, instance)?;
        validate_config(&p.manifest, &values)?;
        let schema_secrets = secret_fields(&p.manifest);
        for (field, value) in &secrets {
            if !schema_secrets.contains(field) {
                return Err(AppError::new(
                    "PLUGIN_CONFIG_SCHEMA",
                    format!("Unknown secret field '{field}'"),
                ));
            }
            config::put_secret(plugin, instance, field, value)?;
        }
        let mut cfg = old;
        let mut public_values = values;
        if let Some(object) = public_values.as_object_mut() {
            for field in &schema_secrets {
                object.remove(field);
            }
        }
        cfg.values = public_values;
        cfg.configured_secrets = schema_secrets
            .into_iter()
            .filter(|field| config::get_secret(plugin, instance, field).is_some())
            .collect();
        if m.runtime.contains_key(&key(plugin, instance)) {
            cfg.restart_required = true;
        }
        r.settings
            .instances
            .insert(key(plugin, instance), cfg.clone());
        config::save_atomic(&config::config_path(&m.app_data_dir), &r.settings)?;
        Ok(PluginInstance {
            plugin_id: plugin.into(),
            instance_id: instance.into(),
            enabled: cfg.enabled,
            trusted: p.bundled
                || (cfg.trusted
                    && cfg.approved_fingerprint.as_deref() == Some(p.fingerprint.as_str())),
            bundled: p.bundled,
            health: if m.runtime.contains_key(&key(plugin, instance)) {
                "degraded"
            } else {
                "stopped"
            }
            .into(),
            origin: m
                .runtime
                .get(&key(plugin, instance))
                .map(|v| v.origin.clone()),
            reason: if cfg.restart_required {
                Some("Restart required after current work completes".into())
            } else {
                None
            },
            restart_required: cfg.restart_required,
            config: cfg.values,
            configured_secrets: cfg.configured_secrets,
        })
    })
}

pub fn set_enabled(plugin: &str, instance: &str, enabled: bool) -> AppResult<PluginInstance> {
    update_flag(plugin, instance, |c| c.enabled = enabled)
}
pub fn set_trust(plugin: &str, instance: &str, trusted: bool) -> AppResult<PluginInstance> {
    if !trusted {
        let _ = stop(plugin, instance)?;
    }
    with_manager(|m| {
        let mut r = registry(m);
        let mut p = r.get_package(plugin)?.clone();
        if trusted {
            p.fingerprint = super::registry::fingerprint(&p.root)
                .map_err(|e| AppError::new("PLUGIN_PACKAGE_INVALID", e))?;
        }
        let mut c = r.instance(plugin, instance)?;
        c.trusted = trusted;
        c.approved_fingerprint = if trusted {
            Some(p.fingerprint.clone())
        } else {
            None
        };
        r.settings
            .instances
            .insert(key(plugin, instance), c.clone());
        config::save_atomic(&config::config_path(&m.app_data_dir), &r.settings)?;
        Ok(PluginInstance {
            plugin_id: plugin.into(),
            instance_id: instance.into(),
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
        })
    })
}
fn update_flag(
    plugin: &str,
    instance: &str,
    f: impl FnOnce(&mut InstanceConfig),
) -> AppResult<PluginInstance> {
    with_manager(|m| {
        let mut r = registry(m);
        let p = r.get_package(plugin)?.clone();
        let mut c = r.instance(plugin, instance)?;
        f(&mut c);
        r.settings
            .instances
            .insert(key(plugin, instance), c.clone());
        config::save_atomic(&config::config_path(&m.app_data_dir), &r.settings)?;
        Ok(PluginInstance {
            plugin_id: plugin.into(),
            instance_id: instance.into(),
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
        })
    })
}
pub fn add_instance(plugin: &str, instance: &str) -> AppResult<PluginInstance> {
    if !valid_id(instance) {
        return Err(AppError::new(
            "PLUGIN_INSTANCE_INVALID",
            "Instance id must match [a-z0-9-]+",
        ));
    }
    with_manager(|m| {
        let mut r = registry(m);
        let p = r.get_package(plugin)?.clone();
        let k = key(plugin, instance);
        if r.settings.instances.contains_key(&k) {
            return Err(AppError::new(
                "PLUGIN_INSTANCE_EXISTS",
                "Instance already exists",
            ));
        }
        let c = InstanceConfig::default();
        r.settings.instances.insert(k, c.clone());
        config::save_atomic(&config::config_path(&m.app_data_dir), &r.settings)?;
        Ok(PluginInstance {
            plugin_id: plugin.into(),
            instance_id: instance.into(),
            enabled: false,
            trusted: p.bundled,
            bundled: p.bundled,
            health: "stopped".into(),
            origin: None,
            reason: None,
            restart_required: false,
            config: c.values,
            configured_secrets: vec![],
        })
    })
}
pub fn remove_instance(plugin: &str, instance: &str) -> AppResult<bool> {
    let _ = stop(plugin, instance)?;
    with_manager(|m| {
        let mut r = registry(m);
        let package = r.get_package(plugin)?.clone();
        let existed = r
            .settings
            .instances
            .remove(&key(plugin, instance))
            .is_some();
        for field in secret_fields(&package.manifest) {
            let _ = config::put_secret(plugin, instance, &field, "");
        }
        config::save_atomic(&config::config_path(&m.app_data_dir), &r.settings)?;
        Ok(existed)
    })
}
pub fn capabilities(plugin: &str, instance: &str) -> AppResult<serde_json::Value> {
    let s = session(&format!("plugin:{plugin}:{instance}"))?;
    crate::plugins::client::get(&format!("plugin:{plugin}:{instance}"), &s.capabilities_path)
        .map_err(plugin_error)
}
pub fn quota(plugin: &str, instance: &str) -> AppResult<serde_json::Value> {
    let s = session(&format!("plugin:{plugin}:{instance}"))?;
    crate::plugins::client::get(&format!("plugin:{plugin}:{instance}"), &s.quota_path)
        .map_err(plugin_error)
}
pub fn action(
    plugin: &str,
    instance: &str,
    action_id: &str,
    payload: serde_json::Value,
) -> AppResult<serde_json::Value> {
    with_manager(|m| {
        let r = registry(m);
        let p = r.get_package(plugin)?;
        if !p.manifest.actions.iter().any(|a| a.id == action_id) {
            return Err(AppError::new(
                "PLUGIN_ACTION_NOT_FOUND",
                "Plugin action is not declared in its manifest",
            ));
        }
        Ok(())
    })?;
    crate::plugins::client::action(&format!("plugin:{plugin}:{instance}"), action_id, &payload)
        .map_err(plugin_error)
}
fn plugin_error(e: crate::plugins::client::PluginError) -> AppError {
    AppError::new(&e.code, e.message)
}

fn choose_port(preferred: u16) -> AppResult<u16> {
    if preferred != 0
        && preferred != 18610
        && TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), preferred)).is_ok()
    {
        return Ok(preferred);
    }
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .map_err(|e| AppError::new("PLUGIN_PORT", format!("No local port is available: {e}")))?
        .local_addr()
        .map(|a| a.port())
        .map_err(|e| AppError::new("PLUGIN_PORT", e.to_string()))
}
fn random_token() -> AppResult<String> {
    let mut b = [0u8; 32];
    getrandom::getrandom(&mut b).map_err(|e| AppError::new("PLUGIN_TOKEN", e.to_string()))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}
fn secret_fields(m: &PluginManifest) -> Vec<String> {
    fn visit(v: &serde_json::Value, out: &mut Vec<String>) {
        if let Some(props) = v.get("properties").and_then(|x| x.as_object()) {
            for (name, schema) in props {
                if schema.get("x-secret").and_then(|x| x.as_bool()) == Some(true)
                    || schema.get("format").and_then(|x| x.as_str()) == Some("password")
                {
                    out.push(name.clone());
                }
                visit(schema, out);
            }
        }
    }
    let mut v = Vec::new();
    visit(&m.config_schema, &mut v);
    v
}
fn secret_values(plugin: &str, instance: &str, m: &PluginManifest) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for k in secret_fields(m) {
        if let Some(v) = config::get_secret(plugin, instance, &k) {
            out.insert(k, serde_json::Value::String(v));
        }
    }
    serde_json::Value::Object(out)
}
fn validate_config(m: &PluginManifest, v: &serde_json::Value) -> AppResult<()> {
    let Some(obj) = v.as_object() else {
        return Err(AppError::new(
            "PLUGIN_CONFIG_SCHEMA",
            "Config must be a JSON object",
        ));
    };
    let Some(props) = m
        .config_schema
        .get("properties")
        .and_then(|x| x.as_object())
    else {
        return Ok(());
    };
    for (name, value) in obj {
        let Some(schema) = props.get(name) else {
            return Err(AppError::new(
                "PLUGIN_CONFIG_SCHEMA",
                format!("Unsupported config field '{name}'"),
            ));
        };
        let typ = schema.get("type").and_then(|x| x.as_str()).unwrap_or("");
        let valid = match typ {
            "string" => value.is_string(),
            "integer" => value.as_i64().is_some_and(|n| {
                n >= schema
                    .get("minimum")
                    .and_then(|x| x.as_i64())
                    .unwrap_or(i64::MIN)
                    && n <= schema
                        .get("maximum")
                        .and_then(|x| x.as_i64())
                        .unwrap_or(i64::MAX)
            }),
            "boolean" => value.is_boolean(),
            _ => false,
        };
        if !valid {
            return Err(AppError::new(
                "PLUGIN_CONFIG_SCHEMA",
                format!("Field '{name}' has unsupported type or invalid value"),
            ));
        }
        if let Some(options) = schema.get("enum").and_then(|x| x.as_array()) {
            if !options.contains(value) {
                return Err(AppError::new(
                    "PLUGIN_CONFIG_SCHEMA",
                    format!("Field '{name}' is not an allowed value"),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn manifest() -> PluginManifest {
        PluginManifest {
            id: "demo".into(),
            name: "Demo".into(),
            version: "1".into(),
            min_core_version: None,
            plugin_api_version: "1".into(),
            entry: super::super::manifest::PluginEntry {
                exe: "demo.exe".into(),
                args: vec![],
            },
            preferred_port: None,
            health_path: "/healthz".into(),
            capabilities_path: "/caps".into(),
            quota_path: "/quota".into(),
            config_schema: serde_json::json!({"type":"object","properties":{"model":{"type":"string","x-dynamic-enum":"capabilities.video.models[].id"},"count":{"type":"integer","minimum":1,"maximum":3}}}),
            actions: vec![],
            permissions: vec![],
        }
    }
    #[test]
    fn config_supports_dynamic_enum_and_enforces_integer_bounds() {
        let m = manifest();
        assert!(validate_config(&m, &serde_json::json!({"model":"m-any","count":2})).is_ok());
        assert!(validate_config(&m, &serde_json::json!({"model":"m-any","count":4})).is_err());
        assert!(validate_config(&m, &serde_json::json!({"other":"x"})).is_err());
    }
}
