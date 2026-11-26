//! Shared image/video capacity is accounted per canonical instance owner.
use super::{
    client::{self, PluginError},
    host,
};
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        OnceLock,
    },
    time::{Duration, Instant},
};

#[derive(Debug, PartialEq, Eq)]
pub enum Source<'a> {
    Pinned(&'a str, &'a str),
    Pool(&'a str),
}
pub fn parse(source: &str) -> Option<Source<'_>> {
    fn slug(s: &str) -> bool {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    }
    let mut parts = source.split(':');
    let scheme = parts.next()?;
    let id = parts.next()?;
    if !slug(id) || matches!(id, "gateway" | "internal") {
        return None;
    }
    match (scheme, parts.next(), parts.next()) {
        ("plugin", None, None) => Some(Source::Pinned(id, "default")),
        ("plugin", Some(inst), None) if slug(inst) => Some(Source::Pinned(id, inst)),
        ("plugin-pool", None, None) => Some(Source::Pool(id)),
        _ => None,
    }
}
pub fn is_plugin(source: &str) -> bool {
    parse(source).is_some()
}
pub fn candidates(source: &str) -> Result<Vec<String>, PluginError> {
    match parse(source) {
        Some(Source::Pinned(id, instance)) => Ok(vec![format!("plugin:{id}:{instance}")]),
        Some(Source::Pool(id)) => {
            host::owners(id).map_err(|e| PluginError::local("PLUGIN_UNAVAILABLE", e.message))
        }
        None => Err(PluginError::local(
            "PLUGIN_UNAVAILABLE",
            "Nguồn plugin không hợp lệ",
        )),
    }
}

#[derive(Default)]
struct State {
    used: HashMap<String, u64>,
    cooldown: HashMap<(String, String), Instant>,
}
fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(Default::default)
}
pub fn busy(owner: &str) -> bool {
    state().lock().used.get(owner).copied().unwrap_or(0) > 0
}
pub fn block(owner: &str, kind: &str, error: &PluginError) {
    let key = match error.scope.as_ref() {
        "instance" => "*",
        "media_kind" => kind,
        _ => return,
    };
    // Without an explicit deadline the next admission must recheck health and quota.
    if let Some(delay) = error
        .retry_after_s
        .and_then(|s| Instant::now().checked_add(Duration::from_secs(s)))
    {
        let mut st = state().lock();
        let until = st
            .cooldown
            .entry((owner.into(), key.into()))
            .or_insert(delay);
        *until = (*until).max(delay);
    }
}

pub struct Lease {
    pub owner: String,
    pub model: String,
    pub poll_interval_s: u64,
    pub poll_max_s: u64,
}
impl Drop for Lease {
    fn drop(&mut self) {
        let mut st = state().lock();
        if let Some(n) = st.used.get_mut(&self.owner) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                st.used.remove(&self.owner);
            }
        }
    }
}
impl Lease {
    fn from_caps(owner: &str, model: String, caps: &serde_json::Value) -> Self {
        let hint = &caps["video"]["poll_hint"];
        Self {
            owner: owner.into(),
            model,
            poll_interval_s: hint["interval_s"].as_u64().unwrap_or(15).max(1),
            poll_max_s: hint["max_s"].as_u64().unwrap_or(1800).max(1),
        }
    }
}

struct Admission {
    model: String,
    caps: serde_json::Value,
    slots: u64,
}

fn inspect(owner: &str, kind: &str) -> Result<Admission, PluginError> {
    let Some(Source::Pinned(id, _)) = parse(owner) else {
        return Err(PluginError::local(
            "PLUGIN_UNAVAILABLE",
            "Owner không hợp lệ",
        ));
    };
    let enabled =
        host::owners(id).map_err(|e| PluginError::local("PLUGIN_UNAVAILABLE", e.message))?;
    if !enabled.iter().any(|candidate| candidate == owner) {
        return Err(PluginError::local(
            "PLUGIN_DISABLED",
            "Instance chưa được bật",
        ));
    }
    let s =
        host::session(owner).map_err(|e| PluginError::local("PLUGIN_UNAVAILABLE", e.message))?;
    let health = client::get(owner, &s.health_path)?;
    if health["status"] != "ok" {
        return Err(PluginError::local(
            "PLUGIN_UNAVAILABLE",
            "Instance chưa sẵn sàng",
        ));
    }
    let caps = client::get(owner, &s.capabilities_path)?;
    if !caps["kinds"]
        .as_array()
        .is_some_and(|list| list.iter().any(|v| v == kind))
    {
        return Err(PluginError::local(
            "PLUGIN_UNSUPPORTED",
            "Instance chưa hỗ trợ loại media này",
        ));
    }
    let quota = client::get(owner, &s.quota_path)?;
    for limit in quota["limits"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|v| v["media_kind"] == kind)
    {
        if let Some(accounts) = limit["accounts"].as_array() {
            if !accounts.is_empty()
                && accounts.iter().all(|a| {
                    matches!(
                        a["status"].as_str(),
                        Some("locked" | "auth_required" | "exhausted")
                    )
                })
            {
                return Err(PluginError::local(
                    "PLUGIN_UNAVAILABLE",
                    "Tài khoản chưa sẵn sàng hoặc đã hết hạn mức",
                ));
            }
        }
    }
    let model = s
        .config
        .get(format!("{kind}_model"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| caps[kind]["models"].get(0)?.get("id")?.as_str())
        .ok_or_else(|| PluginError::local("PLUGIN_MODEL", "Chưa có model cho instance"))?;
    let cap = caps["video"]["max_concurrent_jobs_per_account"]
        .as_u64()
        .unwrap_or(1);
    Ok(Admission {
        model: model.to_owned(),
        caps,
        slots: cap,
    })
}

pub fn acquire(source: &str, kind: &str, stop: &AtomicBool) -> Result<Lease, PluginError> {
    let owners = candidates(source)?;
    loop {
        if stop.load(Ordering::SeqCst) {
            return Err(PluginError::local("STOPPED", "Đã dừng"));
        }
        let mut error = PluginError::local("PLUGIN_UNAVAILABLE", "Không có instance khả dụng");
        let mut occupied = false;
        for owner in &owners {
            // Capacity probes may make HTTP calls and must never hold the slot mutex.
            let admission = match inspect(owner, kind) {
                Ok(info) => info,
                Err(e) => {
                    error = e;
                    continue;
                }
            };
            let mut st = state().lock();
            let cooling = ["*", kind].iter().any(|k| {
                st.cooldown
                    .get(&(owner.clone(), k.to_string()))
                    .is_some_and(|t| *t > Instant::now())
            });
            if !cooling && st.used.get(owner).copied().unwrap_or(0) < admission.slots {
                *st.used.entry(owner.clone()).or_default() += 1;
                return Ok(Lease::from_caps(owner, admission.model, &admission.caps));
            }
            occupied |= !cooling && admission.slots > 0;
        }
        if !occupied {
            return Err(error);
        }
        if crate::adapters::sleep_interruptible(Duration::from_millis(500), stop) {
            return Err(PluginError::local("STOPPED", "Đã dừng"));
        }
    }
}
pub fn hold_owner(owner: &str, kind: &str, stop: &AtomicBool) -> Result<Lease, PluginError> {
    let s =
        host::session(owner).map_err(|e| PluginError::local("PLUGIN_UNAVAILABLE", e.message))?;
    let caps = client::get(owner, &s.capabilities_path).unwrap_or_default();
    let model = s
        .config
        .get(format!("{kind}_model"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    let slots = caps["video"]["max_concurrent_jobs_per_account"]
        .as_u64()
        .unwrap_or(1)
        .max(1);
    loop {
        {
            let mut st = state().lock();
            let used = st.used.entry(owner.into()).or_default();
            if *used < slots {
                *used += 1;
                return Ok(Lease::from_caps(owner, model, &caps));
            }
        }
        if crate::adapters::sleep_interruptible(Duration::from_millis(200), stop) {
            return Err(PluginError::local(
                "STOPPED",
                "Đã dừng trong lúc chờ instance của job",
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plugin_sources_are_unambiguous() {
        assert_eq!(
            parse("plugin:demo"),
            Some(Source::Pinned("demo", "default"))
        );
        assert_eq!(parse("plugin-pool:demo"), Some(Source::Pool("demo")));
        for input in [
            "plugin:",
            "plugin:demo:",
            "plugin-pool:demo:a",
            "plugin:../a",
            "plugin:gateway",
            "plugin:demo:a:b",
        ] {
            assert!(parse(input).is_none())
        }
    }
}
