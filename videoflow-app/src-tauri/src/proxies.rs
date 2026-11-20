//! Quản lý Proxy toàn cục của ứng dụng (`%APPDATA%/VideoFlowDesktop/proxies.db`).
//! Rust là nguồn sự thật duy nhất (Single Source of Truth) cho cấu hình Proxy
//! và liên kết `account_id -> proxy_id`, tự động resolve sang URL proxy đầy đủ
//! khi đồng bộ xuống sidecar.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use base64::Engine as _;
use parking_lot::Mutex;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};

pub const PROXIES_DB_FILE: &str = "proxies.db";
#[cfg(not(test))]
const KEY_PROXY_MASTER: &str = "proxies_master_key";
const ENC_PREFIX: &str = "enc:v1:";

type ProxyEndpointRow = (String, String, i64, Option<String>, Option<String>);

static MASTER_KEY_CACHE: OnceLock<Mutex<Option<[u8; 32]>>> = OnceLock::new();

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyItem {
    pub id: String,
    pub name: String,
    pub kind: String, // "http" | "socks5"
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    #[serde(skip_serializing, default)]
    pub password: Option<String>,
    pub has_password: bool,
    pub note: Option<String>,
    pub last_status: String, // "untested" | "alive" | "dead"
    pub exit_ip: Option<String>,
    pub latency_ms: Option<i64>,
    pub tested_at: Option<String>,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub assigned_account_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ProxyRawSnapshot {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub host: String,
    pub port: i64,
    pub username: Option<String>,
    pub password: Option<String>,
    pub note: Option<String>,
    pub last_status: String,
    pub exit_ip: Option<String>,
    pub latency_ms: Option<i64>,
    pub tested_at: Option<String>,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub assigned_links: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
struct EndpointFingerprint {
    kind: String,
    host: String,
    port: i64,
    username: Option<String>,
    password: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyInput {
    pub name: String,
    pub kind: String,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    #[serde(default)]
    pub clear_password: Option<bool>,
    pub note: Option<String>,
}

pub fn default_db_path() -> PathBuf {
    let dir = crate::settings::app_data_dir();
    let _ = std::fs::create_dir_all(&dir);
    dir.join(PROXIES_DB_FILE)
}

#[cfg(test)]
fn get_or_create_master_key() -> AppResult<[u8; 32]> {
    let cell = MASTER_KEY_CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cell.lock();
    if let Some(key) = *guard {
        return Ok(key);
    }
    let mut key = [0u8; 32];
    getrandom::getrandom(&mut key).map_err(|e| {
        AppError::new("PROXY_KEY_GEN", format!("Không tạo được khóa mã hóa proxy: {e}"))
    })?;
    *guard = Some(key);
    Ok(key)
}

#[cfg(not(test))]
fn get_or_create_master_key() -> AppResult<[u8; 32]> {
    let cell = MASTER_KEY_CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cell.lock();
    if let Some(key) = *guard {
        return Ok(key);
    }
    let entry = keyring::Entry::new(crate::settings::KEYRING_SERVICE, KEY_PROXY_MASTER)
        .map_err(|e| AppError::new("PROXY_KEYRING", format!("Không mở được Keyring proxy: {e}")))?;
    match entry.get_password() {
        Ok(encoded) => {
            let raw = base64::engine::general_purpose::STANDARD
                .decode(encoded.trim())
                .map_err(|e| {
                    AppError::new("PROXY_KEYRING", format!("Khóa proxy trong Keyring bị hỏng: {e}"))
                })?;
            if raw.len() != 32 {
                return Err(AppError::new(
                    "PROXY_KEYRING",
                    "Khóa proxy trong Keyring có độ dài không hợp lệ.",
                ));
            }
            let mut key = [0u8; 32];
            key.copy_from_slice(&raw);
            *guard = Some(key);
            Ok(key)
        }
        Err(keyring::Error::NoEntry) => {
            let mut key = [0u8; 32];
            getrandom::getrandom(&mut key).map_err(|e| {
                AppError::new("PROXY_KEY_GEN", format!("Không tạo được khóa mã hóa proxy: {e}"))
            })?;
            let encoded = base64::engine::general_purpose::STANDARD.encode(key);
            entry.set_password(&encoded).map_err(|e| {
                AppError::new("PROXY_KEYRING", format!("Không lưu được khóa proxy vào Keyring: {e}"))
            })?;
            *guard = Some(key);
            Ok(key)
        }
        Err(e) => Err(AppError::new(
            "PROXY_KEYRING",
            format!("Không đọc được khóa proxy từ Keyring: {e}"),
        )),
    }
}

fn encrypt_password(proxy_id: &str, plain: &str) -> AppResult<String> {
    let key_bytes = get_or_create_master_key()?;
    let unbound = UnboundKey::new(&AES_256_GCM, &key_bytes)
        .map_err(|_| AppError::new("PROXY_ENCRYPT", "Không khởi tạo được khóa AES-256-GCM."))?;
    let key = LessSafeKey::new(unbound);

    let mut nonce_bytes = [0u8; 12];
    getrandom::getrandom(&mut nonce_bytes)
        .map_err(|e| AppError::new("PROXY_ENCRYPT", format!("Không tạo được nonce ngẫu nhiên: {e}")))?;
    let nonce = Nonce::assume_unique_for_key(nonce_bytes);

    let mut in_out = plain.as_bytes().to_vec();
    key.seal_in_place_append_tag(nonce, Aad::from(proxy_id.as_bytes()), &mut in_out)
        .map_err(|_| AppError::new("PROXY_ENCRYPT", "Mã hóa mật khẩu proxy thất bại."))?;

    let mut blob = Vec::with_capacity(12 + in_out.len());
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&in_out);
    Ok(format!(
        "{ENC_PREFIX}{}",
        base64::engine::general_purpose::STANDARD.encode(&blob)
    ))
}

fn decrypt_password(proxy_id: &str, stored: &str) -> AppResult<String> {
    let Some(b64) = stored.strip_prefix(ENC_PREFIX) else {
        return Ok(stored.to_string());
    };
    let blob = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|e| AppError::new("PROXY_DECRYPT", format!("Dữ liệu mật khẩu proxy hỏng: {e}")))?;
    let min_len = 12 + AES_256_GCM.tag_len();
    if blob.len() < min_len {
        return Err(AppError::new(
            "PROXY_DECRYPT",
            "Bản mã mật khẩu proxy có độ dài không hợp lệ.",
        ));
    }
    let (nonce_slice, ct_tag) = blob.split_at(12);
    let nonce_bytes: [u8; 12] = nonce_slice
        .try_into()
        .map_err(|_| AppError::new("PROXY_DECRYPT", "Nonce mật khẩu proxy không hợp lệ."))?;

    let key_bytes = get_or_create_master_key()?;
    let unbound = UnboundKey::new(&AES_256_GCM, &key_bytes)
        .map_err(|_| AppError::new("PROXY_DECRYPT", "Không khởi tạo được khóa AES-256-GCM."))?;
    let key = LessSafeKey::new(unbound);

    let mut buf = ct_tag.to_vec();
    let plain_bytes = key
        .open_in_place(
            Nonce::assume_unique_for_key(nonce_bytes),
            Aad::from(proxy_id.as_bytes()),
            &mut buf,
        )
        .map_err(|_| AppError::new("PROXY_DECRYPT", "Giải mã mật khẩu proxy thất bại."))?;

    String::from_utf8(plain_bytes.to_vec())
        .map_err(|e| AppError::new("PROXY_DECRYPT", format!("Mật khẩu giải mã không phải UTF-8: {e}")))
}

fn register_proxy_password_for_redaction(plain: &str) {
    crate::settings::register_secret_for_redaction(plain);
    let encoded = url_encode_userinfo(plain);
    crate::settings::register_secret_for_redaction(&encoded);
}

fn migrate_plaintext_passwords(conn: &mut Connection) -> AppResult<()> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| AppError::new("PROXY_DB_MIGRATE", format!("{e}")))?;
    let rows: Vec<(String, String)> = {
        let mut stmt = tx
            .prepare(
                "SELECT id, password FROM proxies
                 WHERE password IS NOT NULL AND password != '' AND password NOT LIKE 'enc:v1:%'",
            )
            .map_err(|e| AppError::new("PROXY_DB_MIGRATE", format!("{e}")))?;
        let mapped = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| AppError::new("PROXY_DB_MIGRATE", format!("{e}")))?;
        let mut out = Vec::new();
        for item in mapped {
            out.push(item.map_err(|e| AppError::new("PROXY_DB_MIGRATE", format!("{e}")))?);
        }
        out
    };

    for (id, plain) in rows {
        register_proxy_password_for_redaction(&plain);
        let encrypted = encrypt_password(&id, &plain)?;
        tx.execute(
            "UPDATE proxies SET password = ?1 WHERE id = ?2",
            params![encrypted, id],
        )
        .map_err(|e| AppError::new("PROXY_DB_MIGRATE", format!("{e}")))?;
    }

    tx.commit()
        .map_err(|e| AppError::new("PROXY_DB_MIGRATE", format!("{e}")))?;
    Ok(())
}

pub fn open_db_at(path: &Path) -> AppResult<Connection> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut conn = Connection::open(path)
        .map_err(|e| AppError::new("PROXY_DB_OPEN", format!("Không mở được proxies.db: {e}")))?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;

         CREATE TABLE IF NOT EXISTS proxies (
             id TEXT PRIMARY KEY,
             name TEXT NOT NULL,
             kind TEXT NOT NULL CHECK(kind IN ('http', 'socks5')),
             host TEXT NOT NULL,
             port INTEGER NOT NULL CHECK(port BETWEEN 1 AND 65535),
             username TEXT,
             password TEXT,
             note TEXT,
             last_status TEXT NOT NULL DEFAULT 'untested' CHECK(last_status IN ('untested', 'alive', 'dead')),
             exit_ip TEXT,
             latency_ms INTEGER,
             tested_at TEXT,
             last_error TEXT,
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL
         );

         CREATE TABLE IF NOT EXISTS account_proxies (
             account_id TEXT PRIMARY KEY,
             proxy_id TEXT NOT NULL REFERENCES proxies(id) ON DELETE CASCADE,
             updated_at TEXT NOT NULL
         );

         CREATE INDEX IF NOT EXISTS idx_account_proxies_proxy_id ON account_proxies(proxy_id);",
    )
    .map_err(|e| AppError::new("PROXY_DB_INIT", format!("Không khởi tạo được schema proxies.db: {e}")))?;
    migrate_plaintext_passwords(&mut conn)?;
    Ok(conn)
}

pub fn open_db() -> AppResult<Connection> {
    open_db_at(&default_db_path())
}

fn generate_proxy_id() -> String {
    let mut buf = [0u8; 6];
    if getrandom::getrandom(&mut buf).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        for (i, b) in buf.iter_mut().enumerate() {
            *b = ((nanos >> (i * 8)) & 0xff) as u8;
        }
    }
    let mut hex = String::with_capacity(12);
    for b in buf {
        use std::fmt::Write;
        let _ = write!(&mut hex, "{b:02x}");
    }
    format!("px_{hex}")
}

fn url_encode_userinfo(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for b in raw.as_bytes() {
        match *b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~' => out.push(*b as char),
            _ => {
                use std::fmt::Write;
                let _ = write!(&mut out, "%{b:02X}");
            }
        }
    }
    out
}

fn format_scheme(kind: &str) -> &'static str {
    if kind.eq_ignore_ascii_case("socks5") {
        "socks5h"
    } else {
        "http"
    }
}

fn format_host_part(host: &str) -> String {
    let clean_host = host.trim();
    if clean_host.contains(':') && !clean_host.starts_with('[') {
        format!("[{clean_host}]")
    } else {
        clean_host.to_string()
    }
}

pub fn format_proxy_url(
    kind: &str,
    host: &str,
    port: u16,
    username: Option<&str>,
    password: Option<&str>,
) -> String {
    let scheme = format_scheme(kind);
    let host_part = format_host_part(host);

    let user = username.map(str::trim).filter(|s| !s.is_empty());
    let pass = password.filter(|s| !s.is_empty());
    match (user, pass) {
        (Some(u), Some(p)) => format!(
            "{scheme}://{}:{}@{host_part}:{port}",
            url_encode_userinfo(u),
            url_encode_userinfo(p)
        ),
        (Some(u), None) => format!("{scheme}://{}@{host_part}:{port}", url_encode_userinfo(u)),
        (None, Some(p)) => format!("{scheme}://:{}@{host_part}:{port}", url_encode_userinfo(p)),
        (None, None) => format!("{scheme}://{host_part}:{port}"),
    }
}

impl ProxyItem {
    pub fn to_masked_url(&self) -> String {
        let scheme = format_scheme(&self.kind);
        let host_part = format_host_part(&self.host);
        let has_user = self
            .username
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        if has_user || self.has_password {
            format!("{scheme}://***@{host_part}:{}", self.port)
        } else {
            format!("{scheme}://{host_part}:{}", self.port)
        }
    }
}

pub fn resolve_proxy_url_in(conn: &Connection, proxy_id: &str) -> AppResult<String> {
    let row: Option<ProxyEndpointRow> = conn
        .query_row(
            "SELECT kind, host, port, username, password FROM proxies WHERE id = ?1",
            params![proxy_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()
        .map_err(|e| AppError::new("PROXY_DB_READ", format!("{e}")))?;

    let (kind, host, port, username, stored_password) = row
        .ok_or_else(|| AppError::new("PROXY_NOT_FOUND", format!("Không tìm thấy proxy '{proxy_id}'.")))?;

    let plain_password = match stored_password {
        Some(ref s) if !s.is_empty() => {
            let plain = decrypt_password(proxy_id, s)?;
            register_proxy_password_for_redaction(&plain);
            Some(plain)
        }
        _ => None,
    };

    Ok(format_proxy_url(
        &kind,
        &host,
        port as u16,
        username.as_deref(),
        plain_password.as_deref(),
    ))
}

pub fn resolve_proxy_urls_map_in(conn: &Connection) -> AppResult<HashMap<String, String>> {
    let mut stmt = conn
        .prepare("SELECT id, kind, host, port, username, password FROM proxies")
        .map_err(|e| AppError::new("PROXY_DB_READ", format!("{e}")))?;

    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })
        .map_err(|e| AppError::new("PROXY_DB_READ", format!("{e}")))?;

    let mut out = HashMap::new();
    for item in rows {
        let (id, kind, host, port, username, stored_password) =
            item.map_err(|e| AppError::new("PROXY_DB_READ", format!("{e}")))?;
        let plain_password = match stored_password {
            Some(ref s) if !s.is_empty() => {
                let plain = decrypt_password(&id, s)?;
                register_proxy_password_for_redaction(&plain);
                Some(plain)
            }
            _ => None,
        };
        let url = format_proxy_url(
            &kind,
            &host,
            port as u16,
            username.as_deref(),
            plain_password.as_deref(),
        );
        out.insert(id, url);
    }
    Ok(out)
}

fn validate_input(input: &ProxyInput) -> AppResult<ProxyInput> {
    let name = input.name.trim().to_string();
    if name.is_empty() {
        return Err(AppError::new("PROXY_INVALID", "Tên proxy không được để trống."));
    }
    let kind = input.kind.trim().to_ascii_lowercase();
    if kind != "http" && kind != "socks5" {
        return Err(AppError::new(
            "PROXY_INVALID",
            "Loại proxy không hợp lệ (chỉ hỗ trợ 'http' hoặc 'socks5').",
        ));
    }
    let raw_host = input.host.trim();
    if raw_host.is_empty() {
        return Err(AppError::new(
            "PROXY_INVALID",
            "Host/IP proxy không được để trống.",
        ));
    }
    let host = if raw_host.starts_with('[') || raw_host.ends_with(']') {
        let inner = raw_host
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .ok_or_else(|| {
                AppError::new(
                    "PROXY_INVALID",
                    "Địa chỉ IPv6 có dấu ngoặc '[' / ']' không khớp.",
                )
            })?;
        inner.parse::<Ipv6Addr>().map_err(|_| {
            AppError::new(
                "PROXY_INVALID",
                "Địa chỉ IPv6 trong dấu '[...]' không hợp lệ.",
            )
        })?;
        inner.to_string()
    } else if raw_host.contains(':') {
        raw_host.parse::<Ipv6Addr>().map_err(|_| {
            AppError::new(
                "PROXY_INVALID",
                "Host/IP proxy không hợp lệ (không nhập kèm ':port' vào ô Host; IPv6 phải đúng định dạng).",
            )
        })?;
        raw_host.to_string()
    } else {
        if raw_host.chars().any(|c| {
            matches!(c, '/' | '\\' | '@' | '?' | '#' | ':' | '[' | ']')
                || c.is_whitespace()
                || c.is_control()
        }) {
            return Err(AppError::new(
                "PROXY_INVALID",
                "Host/IP proxy không hợp lệ (không chứa khoảng trắng, ký tự điều khiển, ':', '?', '#', '/', '@' hoặc ngoặc vuông).",
            ));
        }
        raw_host.to_string()
    };
    if input.port == 0 {
        return Err(AppError::new(
            "PROXY_INVALID",
            "Port proxy phải nằm trong khoảng 1..65535.",
        ));
    }
    let username = input
        .username
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let password = input
        .password
        .as_ref()
        .filter(|s| !s.is_empty())
        .cloned();
    let note = input
        .note
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Ok(ProxyInput {
        name,
        kind,
        host,
        port: input.port,
        username,
        password,
        clear_password: input.clear_password,
        note,
    })
}

fn load_assignments_map(conn: &Connection) -> AppResult<HashMap<String, Vec<String>>> {
    let mut stmt = conn
        .prepare("SELECT proxy_id, account_id FROM account_proxies ORDER BY account_id ASC")
        .map_err(|e| AppError::new("PROXY_DB_READ", format!("{e}")))?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| AppError::new("PROXY_DB_READ", format!("{e}")))?;
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for item in rows.flatten() {
        map.entry(item.0).or_default().push(item.1);
    }
    Ok(map)
}

pub fn load_account_to_proxy_map(conn: &Connection) -> AppResult<HashMap<String, String>> {
    let mut stmt = conn
        .prepare("SELECT account_id, proxy_id FROM account_proxies")
        .map_err(|e| AppError::new("PROXY_DB_READ", format!("{e}")))?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| AppError::new("PROXY_DB_READ", format!("{e}")))?;
    let mut map = HashMap::new();
    for item in rows.flatten() {
        map.insert(item.0, item.1);
    }
    Ok(map)
}

pub fn list_proxies_in(conn: &Connection) -> AppResult<Vec<ProxyItem>> {
    let mut assigned = load_assignments_map(conn)?;
    let mut stmt = conn
        .prepare(
            "SELECT id, name, kind, host, port, username, password, note,
                    last_status, exit_ip, latency_ms, tested_at, last_error,
                    created_at, updated_at
             FROM proxies
             ORDER BY created_at ASC, id ASC",
        )
        .map_err(|e| AppError::new("PROXY_DB_READ", format!("{e}")))?;

    let rows = stmt
        .query_map([], |r| {
            let id: String = r.get(0)?;
            let stored_password: Option<String> = r.get(6)?;
            let has_password = stored_password.as_ref().map(|s| !s.is_empty()).unwrap_or(false);
            Ok(ProxyItem {
                id,
                name: r.get(1)?,
                kind: r.get(2)?,
                host: r.get(3)?,
                port: r.get::<_, i64>(4)? as u16,
                username: r.get(5)?,
                password: None,
                has_password,
                note: r.get(7)?,
                last_status: r.get(8)?,
                exit_ip: r.get(9)?,
                latency_ms: r.get(10)?,
                tested_at: r.get(11)?,
                last_error: r.get(12)?,
                created_at: r.get(13)?,
                updated_at: r.get(14)?,
                assigned_account_ids: Vec::new(),
            })
        })
        .map_err(|e| AppError::new("PROXY_DB_READ", format!("{e}")))?;

    let mut out = Vec::new();
    for item in rows {
        let mut p = item.map_err(|e| AppError::new("PROXY_DB_READ", format!("{e}")))?;
        p.assigned_account_ids = assigned.remove(&p.id).unwrap_or_default();
        out.push(p);
    }
    Ok(out)
}

pub fn get_proxy_in(conn: &Connection, proxy_id: &str) -> AppResult<ProxyItem> {
    let mut assigned = load_assignments_map(conn)?;
    let mut item: ProxyItem = conn
        .query_row(
            "SELECT id, name, kind, host, port, username, password, note,
                    last_status, exit_ip, latency_ms, tested_at, last_error,
                    created_at, updated_at
             FROM proxies
             WHERE id = ?1",
            params![proxy_id],
            |r| {
                let id: String = r.get(0)?;
                let stored_password: Option<String> = r.get(6)?;
                let has_password = stored_password.as_ref().map(|s| !s.is_empty()).unwrap_or(false);
                Ok(ProxyItem {
                    id,
                    name: r.get(1)?,
                    kind: r.get(2)?,
                    host: r.get(3)?,
                    port: r.get::<_, i64>(4)? as u16,
                    username: r.get(5)?,
                    password: None,
                    has_password,
                    note: r.get(7)?,
                    last_status: r.get(8)?,
                    exit_ip: r.get(9)?,
                    latency_ms: r.get(10)?,
                    tested_at: r.get(11)?,
                    last_error: r.get(12)?,
                    created_at: r.get(13)?,
                    updated_at: r.get(14)?,
                    assigned_account_ids: Vec::new(),
                })
            },
        )
        .optional()
        .map_err(|e| AppError::new("PROXY_DB_READ", format!("{e}")))?
        .ok_or_else(|| AppError::new("PROXY_NOT_FOUND", format!("Không tìm thấy proxy '{proxy_id}'.")))?;
    item.assigned_account_ids = assigned.remove(&item.id).unwrap_or_default();
    Ok(item)
}

pub fn create_proxy_in(conn: &Connection, input: &ProxyInput) -> AppResult<ProxyItem> {
    let v = validate_input(input)?;
    let id = generate_proxy_id();
    let now = chrono::Local::now().to_rfc3339();
    let stored_password = match v.password {
        Some(ref p) => {
            register_proxy_password_for_redaction(p);
            Some(encrypt_password(&id, p)?)
        }
        None => None,
    };
    conn.execute(
        "INSERT INTO proxies (
            id, name, kind, host, port, username, password, note,
            last_status, created_at, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'untested', ?9, ?9)",
        params![
            id,
            v.name,
            v.kind,
            v.host,
            v.port as i64,
            v.username,
            stored_password,
            v.note,
            now
        ],
    )
    .map_err(|e| AppError::new("PROXY_DB_WRITE", format!("Không lưu được proxy mới: {e}")))?;
    get_proxy_in(conn, &id)
}

pub fn update_proxy_in(conn: &Connection, proxy_id: &str, input: &ProxyInput) -> AppResult<ProxyItem> {
    let existing_row: Option<ProxyEndpointRow> = conn
        .query_row(
            "SELECT kind, host, port, username, password FROM proxies WHERE id = ?1",
            params![proxy_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()
        .map_err(|e| AppError::new("PROXY_DB_READ", format!("{e}")))?;
    let (existing_kind, existing_host, existing_port, existing_username, existing_stored_pw) =
        existing_row.ok_or_else(|| {
            AppError::new("PROXY_NOT_FOUND", format!("Không tìm thấy proxy '{proxy_id}'."))
        })?;

    let v = validate_input(input)?;
    let now = chrono::Local::now().to_rfc3339();

    let existing_plain_pw = match existing_stored_pw {
        Some(ref s) if !s.is_empty() => Some(decrypt_password(proxy_id, s)?),
        _ => None,
    };

    let (password_changed, new_stored_pw) = if v.clear_password == Some(true) {
        (existing_plain_pw.is_some(), None)
    } else if let Some(ref new_p) = v.password {
        let changed = existing_plain_pw.as_deref() != Some(new_p.as_str());
        register_proxy_password_for_redaction(new_p);
        let stored = if changed
            || existing_stored_pw
                .as_deref()
                .map(|s| !s.starts_with(ENC_PREFIX))
                .unwrap_or(true)
        {
            Some(encrypt_password(proxy_id, new_p)?)
        } else {
            existing_stored_pw
        };
        (changed, stored)
    } else {
        (false, existing_stored_pw)
    };

    let endpoint_changed = existing_kind != v.kind
        || existing_host != v.host
        || (existing_port as u16) != v.port
        || existing_username != v.username
        || password_changed;

    if endpoint_changed {
        conn.execute(
            "UPDATE proxies
             SET name = ?1, kind = ?2, host = ?3, port = ?4,
                 username = ?5, password = ?6, note = ?7,
                 last_status = 'untested', exit_ip = NULL, latency_ms = NULL,
                 tested_at = NULL, last_error = NULL, updated_at = ?8
             WHERE id = ?9",
            params![
                v.name,
                v.kind,
                v.host,
                v.port as i64,
                v.username,
                new_stored_pw,
                v.note,
                now,
                proxy_id
            ],
        )
        .map_err(|e| AppError::new("PROXY_DB_WRITE", format!("Không cập nhật được proxy: {e}")))?;
    } else {
        conn.execute(
            "UPDATE proxies
             SET name = ?1, note = ?2, updated_at = ?3
             WHERE id = ?4",
            params![v.name, v.note, now, proxy_id],
        )
        .map_err(|e| AppError::new("PROXY_DB_WRITE", format!("Không cập nhật được proxy: {e}")))?;
    }
    get_proxy_in(conn, proxy_id)
}

pub fn delete_proxy_in(conn: &Connection, proxy_id: &str) -> AppResult<Vec<String>> {
    let existing = get_proxy_in(conn, proxy_id)?;
    let affected = existing.assigned_account_ids;
    conn.execute("DELETE FROM proxies WHERE id = ?1", params![proxy_id])
        .map_err(|e| AppError::new("PROXY_DB_WRITE", format!("Không xoá được proxy: {e}")))?;
    Ok(affected)
}

pub fn set_account_proxy_in(
    conn: &Connection,
    account_id: &str,
    proxy_id: Option<&str>,
) -> AppResult<Option<ProxyItem>> {
    let acc_id = account_id.trim();
    if acc_id.is_empty() {
        return Err(AppError::new("PROXY_INVALID", "account_id không hợp lệ."));
    }
    let clean_pid = proxy_id.map(str::trim).filter(|s| !s.is_empty());
    if let Some(pid) = clean_pid {
        let proxy = get_proxy_in(conn, pid)?;
        let now = chrono::Local::now().to_rfc3339();
        conn.execute(
            "INSERT INTO account_proxies(account_id, proxy_id, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(account_id) DO UPDATE SET proxy_id = excluded.proxy_id, updated_at = excluded.updated_at",
            params![acc_id, pid, now],
        )
        .map_err(|e| AppError::new("PROXY_DB_WRITE", format!("Không gán được proxy cho tài khoản: {e}")))?;
        Ok(Some(get_proxy_in(conn, &proxy.id)?))
    } else {
        conn.execute(
            "DELETE FROM account_proxies WHERE account_id = ?1",
            params![acc_id],
        )
        .map_err(|e| AppError::new("PROXY_DB_WRITE", format!("Không gỡ được proxy của tài khoản: {e}")))?;
        Ok(None)
    }
}

pub fn remove_account_link_in(conn: &Connection, account_id: &str) -> AppResult<()> {
    conn.execute(
        "DELETE FROM account_proxies WHERE account_id = ?1",
        params![account_id.trim()],
    )
    .map_err(|e| AppError::new("PROXY_DB_WRITE", format!("{e}")))?;
    Ok(())
}

pub(crate) fn snapshot_proxy_row_in(conn: &Connection, proxy_id: &str) -> AppResult<ProxyRawSnapshot> {
    let mut snap: ProxyRawSnapshot = conn
        .query_row(
            "SELECT id, name, kind, host, port, username, password, note,
                    last_status, exit_ip, latency_ms, tested_at, last_error,
                    created_at, updated_at
             FROM proxies
             WHERE id = ?1",
            params![proxy_id],
            |r| {
                Ok(ProxyRawSnapshot {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    kind: r.get(2)?,
                    host: r.get(3)?,
                    port: r.get(4)?,
                    username: r.get(5)?,
                    password: r.get(6)?,
                    note: r.get(7)?,
                    last_status: r.get(8)?,
                    exit_ip: r.get(9)?,
                    latency_ms: r.get(10)?,
                    tested_at: r.get(11)?,
                    last_error: r.get(12)?,
                    created_at: r.get(13)?,
                    updated_at: r.get(14)?,
                    assigned_links: Vec::new(),
                })
            },
        )
        .optional()
        .map_err(|e| AppError::new("PROXY_DB_READ", format!("{e}")))?
        .ok_or_else(|| AppError::new("PROXY_NOT_FOUND", format!("Không tìm thấy proxy '{proxy_id}'.")))?;

    let mut stmt = conn
        .prepare("SELECT account_id, updated_at FROM account_proxies WHERE proxy_id = ?1 ORDER BY account_id ASC")
        .map_err(|e| AppError::new("PROXY_DB_READ", format!("{e}")))?;
    let rows = stmt
        .query_map(params![proxy_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| AppError::new("PROXY_DB_READ", format!("{e}")))?;
    for row in rows {
        snap.assigned_links
            .push(row.map_err(|e| AppError::new("PROXY_DB_READ", format!("{e}")))?);
    }
    Ok(snap)
}

pub(crate) fn restore_proxy_row_in(conn: &mut Connection, snap: &ProxyRawSnapshot) -> AppResult<()> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| AppError::new("PROXY_DB_WRITE", format!("{e}")))?;
    tx.execute(
        "INSERT OR REPLACE INTO proxies (
            id, name, kind, host, port, username, password, note,
            last_status, exit_ip, latency_ms, tested_at, last_error,
            created_at, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            snap.id,
            snap.name,
            snap.kind,
            snap.host,
            snap.port,
            snap.username,
            snap.password,
            snap.note,
            snap.last_status,
            snap.exit_ip,
            snap.latency_ms,
            snap.tested_at,
            snap.last_error,
            snap.created_at,
            snap.updated_at,
        ],
    )
    .map_err(|e| AppError::new("PROXY_DB_WRITE", format!("{e}")))?;

    tx.execute("DELETE FROM account_proxies WHERE proxy_id = ?1", params![snap.id])
        .map_err(|e| AppError::new("PROXY_DB_WRITE", format!("{e}")))?;
    for (acc_id, upd) in &snap.assigned_links {
        tx.execute(
            "INSERT OR REPLACE INTO account_proxies (account_id, proxy_id, updated_at)
             VALUES (?1, ?2, ?3)",
            params![acc_id, snap.id, upd],
        )
        .map_err(|e| AppError::new("PROXY_DB_WRITE", format!("{e}")))?;
    }

    tx.commit()
        .map_err(|e| AppError::new("PROXY_DB_WRITE", format!("{e}")))?;
    Ok(())
}

pub fn record_test_result_in(
    conn: &Connection,
    proxy_id: &str,
    alive: bool,
    exit_ip: Option<&str>,
    latency_ms: Option<i64>,
    error: Option<&str>,
) -> AppResult<ProxyItem> {
    let status = if alive { "alive" } else { "dead" };
    let now = chrono::Local::now().to_rfc3339();
    let safe_err = error.map(crate::settings::redact_secrets);
    conn.execute(
        "UPDATE proxies
         SET last_status = ?1,
             exit_ip = ?2,
             latency_ms = ?3,
             tested_at = ?4,
             last_error = ?5,
             updated_at = ?4
         WHERE id = ?6",
        params![status, exit_ip, latency_ms, now, safe_err, proxy_id],
    )
    .map_err(|e| AppError::new("PROXY_DB_WRITE", format!("Không lưu được kết quả test proxy: {e}")))?;
    get_proxy_in(conn, proxy_id)
}

fn record_test_result_if_unchanged_in(
    conn: &Connection,
    proxy_id: &str,
    fp: &EndpointFingerprint,
    alive: bool,
    exit_ip: Option<&str>,
    latency_ms: Option<i64>,
    error: Option<&str>,
) -> AppResult<ProxyItem> {
    let status = if alive { "alive" } else { "dead" };
    let now = chrono::Local::now().to_rfc3339();
    let safe_err = error.map(crate::settings::redact_secrets);
    conn.execute(
        "UPDATE proxies
         SET last_status = ?1,
             exit_ip = ?2,
             latency_ms = ?3,
             tested_at = ?4,
             last_error = ?5,
             updated_at = ?4
         WHERE id = ?6
           AND kind = ?7
           AND host = ?8
           AND port = ?9
           AND username IS ?10
           AND password IS ?11",
        params![
            status,
            exit_ip,
            latency_ms,
            now,
            safe_err,
            proxy_id,
            fp.kind,
            fp.host,
            fp.port,
            fp.username,
            fp.password
        ],
    )
    .map_err(|e| AppError::new("PROXY_DB_WRITE", format!("Không lưu được kết quả test proxy: {e}")))?;
    get_proxy_in(conn, proxy_id)
}

/// Kiểm tra kết nối THẬT qua HTTP/SOCKS5 proxy tới `target_url` (mặc định `https://muse.ai`)
/// và truy vấn Exit IP thực tế từ `ip_urls`.
pub fn test_proxy_connection_with_urls(
    conn: &Connection,
    proxy_id: &str,
    target_url: &str,
    ip_urls: &[&str],
) -> AppResult<ProxyItem> {
    let fp = conn
        .query_row(
            "SELECT kind, host, port, username, password FROM proxies WHERE id = ?1",
            params![proxy_id],
            |r| {
                Ok(EndpointFingerprint {
                    kind: r.get(0)?,
                    host: r.get(1)?,
                    port: r.get(2)?,
                    username: r.get(3)?,
                    password: r.get(4)?,
                })
            },
        )
        .optional()
        .map_err(|e| AppError::new("PROXY_DB_READ", format!("{e}")))?
        .ok_or_else(|| {
            AppError::new("PROXY_NOT_FOUND", format!("Không tìm thấy proxy '{proxy_id}'."))
        })?;

    let plain_password = match fp.password {
        Some(ref s) if !s.is_empty() => {
            let plain = decrypt_password(proxy_id, s)?;
            register_proxy_password_for_redaction(&plain);
            Some(plain)
        }
        _ => None,
    };
    let proxy_url = format_proxy_url(
        &fp.kind,
        &fp.host,
        fp.port as u16,
        fp.username.as_deref(),
        plain_password.as_deref(),
    );

    let req_proxy = match reqwest::Proxy::all(&proxy_url) {
        Ok(p) => p,
        Err(e) => {
            return record_test_result_if_unchanged_in(
                conn,
                proxy_id,
                &fp,
                false,
                None,
                None,
                Some(&format!("Cấu hình URL proxy không hợp lệ: {e}")),
            );
        }
    };

    let client = match reqwest::blocking::Client::builder()
        .proxy(req_proxy)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(15))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return record_test_result_if_unchanged_in(
                conn,
                proxy_id,
                &fp,
                false,
                None,
                None,
                Some(&format!("Không khởi tạo được HTTP/SOCKS5 client: {e}")),
            );
        }
    };

    let started = Instant::now();
    let resp = match client.get(target_url).send() {
        Ok(r) => r,
        Err(e) => {
            let elapsed = started.elapsed().as_millis() as i64;
            return record_test_result_if_unchanged_in(
                conn,
                proxy_id,
                &fp,
                false,
                None,
                Some(elapsed),
                Some(&format!("Kết nối tới {target_url} qua proxy thất bại: {e}")),
            );
        }
    };

    let elapsed = started.elapsed().as_millis() as i64;
    let code = resp.status().as_u16();
    if code == 407 {
        return record_test_result_if_unchanged_in(
            conn,
            proxy_id,
            &fp,
            false,
            None,
            Some(elapsed),
            Some("Proxy từ chối xác thực (HTTP 407 Proxy Authentication Required)."),
        );
    }
    if matches!(code, 403 | 404 | 429) {
        return record_test_result_if_unchanged_in(
            conn,
            proxy_id,
            &fp,
            false,
            None,
            Some(elapsed),
            Some(&format!("Proxy hoặc máy chủ đích từ chối/chặn truy cập (HTTP {code}).")),
        );
    }
    if code >= 500 {
        return record_test_result_if_unchanged_in(
            conn,
            proxy_id,
            &fp,
            false,
            None,
            Some(elapsed),
            Some(&format!("Máy chủ đích trả mã lỗi HTTP {code}.")),
        );
    }

    // Truy vấn Exit IP thực tế qua cùng client proxy
    let mut detected_ip: Option<String> = None;
    for ip_url in ip_urls {
        if let Ok(ip_resp) = client.get(*ip_url).timeout(Duration::from_secs(8)).send() {
            if ip_resp.status().is_success() {
                if let Ok(body) = ip_resp.text() {
                    let trimmed = body.trim();
                    if trimmed.starts_with('{') {
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
                            if let Some(ip_str) = val.get("ip").and_then(|v| v.as_str()) {
                                if let Ok(ip) = ip_str.trim().parse::<IpAddr>() {
                                    detected_ip = Some(ip.to_string());
                                    break;
                                }
                            }
                        }
                    } else if let Ok(ip) = trimmed.parse::<IpAddr>() {
                        detected_ip = Some(ip.to_string());
                        break;
                    }
                }
            }
        }
    }

    record_test_result_if_unchanged_in(
        conn,
        proxy_id,
        &fp,
        true,
        detected_ip.as_deref(),
        Some(elapsed),
        None,
    )
}

pub fn test_proxy_connection(conn: &Connection, proxy_id: &str) -> AppResult<ProxyItem> {
    test_proxy_connection_with_urls(
        conn,
        proxy_id,
        "https://muse.ai",
        &[
            "https://api.ipify.org?format=json",
            "https://icanhazip.com",
            "https://ifconfig.me/ip",
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn temp_db_path(label: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "vf_proxies_test_{label}_{}_{}.db",
            std::process::id(),
            chrono::Local::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        p
    }

    #[test]
    fn test_crud_and_account_assignment_and_cascade_delete() {
        let db_path = temp_db_path("crud");
        let conn = open_db_at(&db_path).expect("open proxies.db");

        // 1. Create proxy
        let created = create_proxy_in(
            &conn,
            &ProxyInput {
                name: "Proxy US-01".into(),
                kind: "SOCKS5".into(),
                host: "198.51.100.10".into(),
                port: 1080,
                username: Some("user@name".into()),
                password: Some("p@ss:123".into()),
                clear_password: None,
                note: Some("Ghi chú US".into()),
            },
        )
        .expect("create proxy");

        assert!(created.id.starts_with("px_"));
        assert_eq!(created.name, "Proxy US-01");
        assert_eq!(created.kind, "socks5");
        assert_eq!(created.host, "198.51.100.10");
        assert_eq!(created.port, 1080);
        assert!(created.password.is_none());
        assert!(created.has_password);
        assert_eq!(created.last_status, "untested");
        assert_eq!(
            resolve_proxy_url_in(&conn, &created.id).expect("resolve proxy url"),
            "socks5h://user%40name:p%40ss%3A123@198.51.100.10:1080"
        );
        assert_eq!(created.to_masked_url(), "socks5h://***@198.51.100.10:1080");

        // 2. Assign to 2 accounts
        set_account_proxy_in(&conn, "acc_1", Some(&created.id)).expect("assign acc_1");
        set_account_proxy_in(&conn, "acc_2", Some(&created.id)).expect("assign acc_2");

        let loaded = get_proxy_in(&conn, &created.id).expect("get proxy");
        assert_eq!(loaded.assigned_account_ids, vec!["acc_1".to_string(), "acc_2".to_string()]);

        // 3. Record test result -> update note only preserves test status, changing host resets status
        let tested = record_test_result_in(
            &conn,
            &created.id,
            true,
            Some("198.51.100.10"),
            Some(142),
            None,
        )
        .expect("record test");
        assert_eq!(tested.last_status, "alive");
        assert_eq!(tested.exit_ip.as_deref(), Some("198.51.100.10"));
        assert_eq!(tested.latency_ms, Some(142));

        let note_updated = update_proxy_in(
            &conn,
            &created.id,
            &ProxyInput {
                name: "Proxy US-01 Renamed".into(),
                kind: "socks5".into(),
                host: "198.51.100.10".into(),
                port: 1080,
                username: Some("user@name".into()),
                password: Some("p@ss:123".into()),
                clear_password: None,
                note: Some("Updated note".into()),
            },
        )
        .expect("update note");
        assert_eq!(note_updated.last_status, "alive");
        assert_eq!(note_updated.exit_ip.as_deref(), Some("198.51.100.10"));

        let host_updated = update_proxy_in(
            &conn,
            &created.id,
            &ProxyInput {
                name: "Proxy US-01 Renamed".into(),
                kind: "http".into(),
                host: "203.0.113.5".into(),
                port: 8080,
                username: None,
                password: None,
                clear_password: Some(true),
                note: None,
            },
        )
        .expect("update host");
        assert_eq!(host_updated.last_status, "untested");
        assert_eq!(host_updated.exit_ip, None);
        assert!(!host_updated.has_password);

        // 4. Delete proxy -> returns affected accounts and cascades
        let affected = delete_proxy_in(&conn, &created.id).expect("delete proxy");
        assert_eq!(affected, vec!["acc_1".to_string(), "acc_2".to_string()]);
        assert!(load_account_to_proxy_map(&conn).unwrap().is_empty());

        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn test_real_http_proxy_test_connection_updates_exit_ip_and_latency() {
        let db_path = temp_db_path("http_test");
        let conn = open_db_at(&db_path).expect("open proxies.db");

        // Spawn a minimal local HTTP proxy server that handles 2 requests:
        // 1. Target check (muse.ai) -> 200 OK
        // 2. Exit IP check (ipify) -> 200 OK {"ip":"203.0.113.99"}
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock proxy");
        let port = listener.local_addr().unwrap().port();

        let server_thread = std::thread::spawn(move || {
            for i in 0..2 {
                if let Ok((mut stream, _)) = listener.accept() {
                    let mut buf = [0u8; 2048];
                    let _ = stream.read(&mut buf);
                    let body = if i == 0 {
                        "<html>muse</html>"
                    } else {
                        "{\"ip\":\"203.0.113.99\"}"
                    };
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes());
                }
            }
        });

        let created = create_proxy_in(
            &conn,
            &ProxyInput {
                name: "Local Test Proxy".into(),
                kind: "http".into(),
                host: "127.0.0.1".into(),
                port,
                username: None,
                password: None,
                clear_password: None,
                note: None,
            },
        )
        .expect("create proxy");

        let tested = test_proxy_connection_with_urls(
            &conn,
            &created.id,
            "http://example.com/",
            &["http://api.ipify.org/?format=json"],
        )
        .expect("test proxy");

        server_thread.join().unwrap();

        assert_eq!(tested.last_status, "alive");
        assert_eq!(tested.exit_ip.as_deref(), Some("203.0.113.99"));
        assert!(tested.latency_ms.is_some());
        assert!(tested.tested_at.is_some());
        assert!(tested.last_error.is_none());

        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn test_h1_h2_socks5h_ipv6_and_password_only_url_and_mask_alignment() {
        assert_eq!(
            format_proxy_url("socks5", "198.51.100.10", 1080, Some("u"), Some("p")),
            "socks5h://u:p@198.51.100.10:1080"
        );
        assert_eq!(
            format_proxy_url("http", "2001:db8::1", 8080, None, Some("p@ss")),
            "http://:p%40ss@[2001:db8::1]:8080"
        );
        assert_eq!(
            format_proxy_url("socks5", "::1", 1080, None, None),
            "socks5h://[::1]:1080"
        );

        let base = ProxyItem {
            id: "px_1".into(),
            name: "v6".into(),
            kind: "socks5".into(),
            host: "2001:db8::1".into(),
            port: 1080,
            username: None,
            password: None,
            has_password: true,
            note: None,
            last_status: "untested".into(),
            exit_ip: None,
            latency_ms: None,
            tested_at: None,
            last_error: None,
            created_at: String::new(),
            updated_at: String::new(),
            assigned_account_ids: Vec::new(),
        };
        assert_eq!(base.to_masked_url(), "socks5h://***@[2001:db8::1]:1080");

        let no_auth = ProxyItem {
            has_password: false,
            ..base.clone()
        };
        assert_eq!(no_auth.to_masked_url(), "socks5h://[2001:db8::1]:1080");

        let http_user_only = ProxyItem {
            kind: "http".into(),
            host: "::1".into(),
            port: 8080,
            username: Some("alice".into()),
            has_password: false,
            ..base
        };
        assert_eq!(http_user_only.to_masked_url(), "http://***@[::1]:8080");
    }

    #[test]
    fn test_h3_password_encrypted_at_rest_aad_bound_and_stripped_from_ipc() {
        let db_path = temp_db_path("h3_crypto");
        let conn = open_db_at(&db_path).expect("open proxies.db");

        let created = create_proxy_in(
            &conn,
            &ProxyInput {
                name: "Encrypted Proxy".into(),
                kind: "socks5".into(),
                host: "198.51.100.20".into(),
                port: 1080,
                username: Some("user1".into()),
                password: Some("S3cr3t@Pass!".into()),
                clear_password: None,
                note: None,
            },
        )
        .expect("create proxy");

        assert!(created.password.is_none());
        assert!(created.has_password);

        let serialized = serde_json::to_string(&created).expect("serialize ProxyItem");
        assert!(!serialized.contains("\"password\""));
        assert!(!serialized.contains("S3cr3t@Pass!"));

        let listed = list_proxies_in(&conn).expect("list proxies");
        assert_eq!(listed.len(), 1);
        assert!(listed[0].password.is_none());
        assert!(listed[0].has_password);

        let raw_db_pw: Option<String> = conn
            .query_row(
                "SELECT password FROM proxies WHERE id = ?1",
                params![&created.id],
                |r| r.get(0),
            )
            .expect("query raw db");
        let cipher = raw_db_pw.expect("stored ciphertext");
        assert!(cipher.starts_with(ENC_PREFIX));
        assert!(!cipher.contains("S3cr3t@Pass!"));

        // AAD bound to proxy_id: decrypting under wrong proxy_id or tampered ciphertext fails
        assert_eq!(
            decrypt_password(&created.id, &cipher).expect("decrypt own id"),
            "S3cr3t@Pass!"
        );
        assert!(decrypt_password("px_other_id", &cipher).is_err());
        assert!(decrypt_password(&created.id, "enc:v1:AAAA").is_err());

        let resolved = resolve_proxy_url_in(&conn, &created.id).expect("resolve single");
        assert_eq!(
            resolved,
            "socks5h://user1:S3cr3t%40Pass%21@198.51.100.20:1080"
        );
        let map = resolve_proxy_urls_map_in(&conn).expect("resolve map");
        assert_eq!(map.get(&created.id), Some(&resolved));

        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn test_h3_update_proxy_preserves_replaces_and_clears_password() {
        let db_path = temp_db_path("h3_update");
        let conn = open_db_at(&db_path).expect("open proxies.db");

        let created = create_proxy_in(
            &conn,
            &ProxyInput {
                name: "Update Flow".into(),
                kind: "http".into(),
                host: "198.51.100.30".into(),
                port: 8080,
                username: Some("u1".into()),
                password: Some("initial_pw".into()),
                clear_password: None,
                note: None,
            },
        )
        .expect("create");
        record_test_result_in(&conn, &created.id, true, Some("198.51.100.30"), Some(50), None)
            .expect("mark alive");

        // 1. Metadata-only edit with password=None preserves status AND existing encrypted password
        let meta_only = update_proxy_in(
            &conn,
            &created.id,
            &ProxyInput {
                name: "Update Flow v2".into(),
                kind: "http".into(),
                host: "198.51.100.30".into(),
                port: 8080,
                username: Some("u1".into()),
                password: None,
                clear_password: None,
                note: Some("memo".into()),
            },
        )
        .expect("meta edit");
        assert_eq!(meta_only.last_status, "alive");
        assert!(meta_only.has_password);
        assert_eq!(
            resolve_proxy_url_in(&conn, &created.id).unwrap(),
            "http://u1:initial_pw@198.51.100.30:8080"
        );

        // 2. Host change with password=None resets status AND still preserves existing password
        let host_changed = update_proxy_in(
            &conn,
            &created.id,
            &ProxyInput {
                name: "Update Flow v2".into(),
                kind: "http".into(),
                host: "198.51.100.31".into(),
                port: 8080,
                username: Some("u1".into()),
                password: None,
                clear_password: None,
                note: Some("memo".into()),
            },
        )
        .expect("host edit");
        assert_eq!(host_changed.last_status, "untested");
        assert!(host_changed.has_password);
        assert_eq!(
            resolve_proxy_url_in(&conn, &created.id).unwrap(),
            "http://u1:initial_pw@198.51.100.31:8080"
        );

        // 3. Password replacement resets status & updates ciphertext
        record_test_result_in(&conn, &created.id, true, Some("198.51.100.31"), Some(40), None)
            .expect("mark alive again");
        let pw_replaced = update_proxy_in(
            &conn,
            &created.id,
            &ProxyInput {
                name: "Update Flow v2".into(),
                kind: "http".into(),
                host: "198.51.100.31".into(),
                port: 8080,
                username: Some("u1".into()),
                password: Some("new_pw_2".into()),
                clear_password: None,
                note: None,
            },
        )
        .expect("pw replace");
        assert_eq!(pw_replaced.last_status, "untested");
        assert!(pw_replaced.has_password);
        assert_eq!(
            resolve_proxy_url_in(&conn, &created.id).unwrap(),
            "http://u1:new_pw_2@198.51.100.31:8080"
        );

        // 4. clear_password=Some(true) clears password even if password field is populated
        record_test_result_in(&conn, &created.id, true, Some("198.51.100.31"), Some(35), None)
            .expect("mark alive 3");
        let cleared = update_proxy_in(
            &conn,
            &created.id,
            &ProxyInput {
                name: "Update Flow v2".into(),
                kind: "http".into(),
                host: "198.51.100.31".into(),
                port: 8080,
                username: Some("u1".into()),
                password: Some("should_not_save".into()),
                clear_password: Some(true),
                note: None,
            },
        )
        .expect("clear password");
        assert_eq!(cleared.last_status, "untested");
        assert!(!cleared.has_password);
        let raw_pw: Option<String> = conn
            .query_row(
                "SELECT password FROM proxies WHERE id = ?1",
                params![&created.id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(raw_pw.is_none());
        assert_eq!(
            resolve_proxy_url_in(&conn, &created.id).unwrap(),
            "http://u1@198.51.100.31:8080"
        );

        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn test_h3_legacy_plaintext_password_migrated_on_open_db() {
        let db_path = temp_db_path("h3_migrate");
        {
            let raw_conn = Connection::open(&db_path).expect("open raw db");
            raw_conn
                .execute_batch(
                    "CREATE TABLE proxies (
                        id TEXT PRIMARY KEY,
                        name TEXT NOT NULL,
                        kind TEXT NOT NULL CHECK(kind IN ('http', 'socks5')),
                        host TEXT NOT NULL,
                        port INTEGER NOT NULL CHECK(port BETWEEN 1 AND 65535),
                        username TEXT,
                        password TEXT,
                        note TEXT,
                        last_status TEXT NOT NULL DEFAULT 'untested' CHECK(last_status IN ('untested', 'alive', 'dead')),
                        exit_ip TEXT,
                        latency_ms INTEGER,
                        tested_at TEXT,
                        last_error TEXT,
                        created_at TEXT NOT NULL,
                        updated_at TEXT NOT NULL
                    );
                    INSERT INTO proxies(id, name, kind, host, port, username, password, created_at, updated_at)
                    VALUES
                        ('px_legacy1', 'Legacy 1', 'socks5', '198.51.100.40', 1080, 'u_leg', 'legacy_plain_pw_99', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z'),
                        ('px_legacy2', 'Legacy 2', 'http', '198.51.100.41', 8080, NULL, NULL, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');",
                )
                .expect("seed legacy db");
        }

        let conn = open_db_at(&db_path).expect("open_db_at migrates plaintext");
        let raw_pw_1: Option<String> = conn
            .query_row("SELECT password FROM proxies WHERE id = 'px_legacy1'", [], |r| r.get(0))
            .unwrap();
        let raw_pw_2: Option<String> = conn
            .query_row("SELECT password FROM proxies WHERE id = 'px_legacy2'", [], |r| r.get(0))
            .unwrap();

        let enc_1 = raw_pw_1.expect("migrated ciphertext");
        assert!(enc_1.starts_with(ENC_PREFIX));
        assert!(!enc_1.contains("legacy_plain_pw_99"));
        assert!(raw_pw_2.is_none());

        assert_eq!(
            resolve_proxy_url_in(&conn, "px_legacy1").unwrap(),
            "socks5h://u_leg:legacy_plain_pw_99@198.51.100.40:1080"
        );
        let redacted = crate::settings::redact_secrets("boom legacy_plain_pw_99");
        assert!(!redacted.contains("legacy_plain_pw_99"));

        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn test_h5_validate_input_host_rules_and_password_whitespace_preserved() {
        let base_input = ProxyInput {
            name: "Host Test".into(),
            kind: "http".into(),
            host: "198.51.100.1".into(),
            port: 8080,
            username: None,
            password: None,
            clear_password: None,
            note: None,
        };

        let invalid_hosts = [
            "",
            "   ",
            "1.2.3.4:8080",
            "proxy.example.com:3128",
            "proxy.example.com?foo=1",
            "proxy.example.com#frag",
            "[::1",
            "::1]",
            "[not-ipv6]",
            "bad\x01host",
            "bad host",
            "proxy.example.com/path",
            "user@proxy.example.com",
        ];
        for bad in invalid_hosts {
            let mut inp = base_input.clone();
            inp.host = bad.into();
            assert!(
                validate_input(&inp).is_err(),
                "host '{bad}' must be rejected"
            );
        }

        // Bracketed IPv6 is accepted and normalized without brackets; bare IPv6 also accepted
        let mut v6_bracketed = base_input.clone();
        v6_bracketed.host = " [2001:db8::1] ".into();
        let validated_v6 = validate_input(&v6_bracketed).expect("bracketed ipv6 valid");
        assert_eq!(validated_v6.host, "2001:db8::1");

        let mut v6_bare = base_input.clone();
        v6_bare.host = "::1".into();
        let validated_bare = validate_input(&v6_bare).expect("bare ipv6 valid");
        assert_eq!(validated_bare.host, "::1");

        // Password leading/trailing whitespace must be preserved (not trimmed)
        let db_path = temp_db_path("h5_pw_ws");
        let conn = open_db_at(&db_path).expect("open proxies.db");
        let created = create_proxy_in(
            &conn,
            &ProxyInput {
                name: "Whitespace PW".into(),
                kind: "http".into(),
                host: "198.51.100.55".into(),
                port: 8080,
                username: Some("u".into()),
                password: Some("  lead_trail  ".into()),
                clear_password: None,
                note: None,
            },
        )
        .expect("create with spaced password");
        assert_eq!(
            resolve_proxy_url_in(&conn, &created.id).unwrap(),
            "http://u:%20%20lead_trail%20%20@198.51.100.55:8080"
        );

        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn test_m2_proxy_test_rejects_403_404_429_validates_exit_ip_and_redacts_error() {
        let db_path = temp_db_path("m2_http_codes");
        let conn = open_db_at(&db_path).expect("open proxies.db");

        // 1. Verify 403, 404, 429 are all rejected as "dead"
        for status_line in [
            "HTTP/1.1 403 Forbidden",
            "HTTP/1.1 404 Not Found",
            "HTTP/1.1 429 Too Many Requests",
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock proxy");
            let port = listener.local_addr().unwrap().port();
            let line = status_line.to_string();
            let handle = std::thread::spawn(move || {
                if let Ok((mut stream, _)) = listener.accept() {
                    let mut buf = [0u8; 2048];
                    let _ = stream.read(&mut buf);
                    let resp = format!("{line}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    let _ = stream.write_all(resp.as_bytes());
                }
            });

            let created = create_proxy_in(
                &conn,
                &ProxyInput {
                    name: format!("Mock {status_line}"),
                    kind: "http".into(),
                    host: "127.0.0.1".into(),
                    port,
                    username: None,
                    password: None,
                    clear_password: None,
                    note: None,
                },
            )
            .expect("create proxy");

            let tested = test_proxy_connection_with_urls(
                &conn,
                &created.id,
                "http://example.com/",
                &["http://api.ipify.org/?format=json"],
            )
            .expect("test proxy");
            handle.join().unwrap();
            assert_eq!(tested.last_status, "dead", "expected dead for {status_line}");
        }

        // 2. Verify invalid exit_ip (HTML / non-IP string) is ignored (exit_ip = None)
        let listener_bad_ip = TcpListener::bind("127.0.0.1:0").expect("bind mock bad ip");
        let port_bad_ip = listener_bad_ip.local_addr().unwrap().port();
        let handle_bad_ip = std::thread::spawn(move || {
            for i in 0..2 {
                if let Ok((mut stream, _)) = listener_bad_ip.accept() {
                    let mut buf = [0u8; 2048];
                    let _ = stream.read(&mut buf);
                    let body = if i == 0 {
                        "ok"
                    } else {
                        "{\"ip\":\"not-an-ip-address\"}"
                    };
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes());
                }
            }
        });
        let px_bad_ip = create_proxy_in(
            &conn,
            &ProxyInput {
                name: "Bad IP Body".into(),
                kind: "http".into(),
                host: "127.0.0.1".into(),
                port: port_bad_ip,
                username: None,
                password: None,
                clear_password: None,
                note: None,
            },
        )
        .expect("create bad ip proxy");
        let tested_bad_ip = test_proxy_connection_with_urls(
            &conn,
            &px_bad_ip.id,
            "http://example.com/",
            &["http://api.ipify.org/?format=json"],
        )
        .expect("test bad ip");
        handle_bad_ip.join().unwrap();
        assert_eq!(tested_bad_ip.last_status, "alive");
        assert_eq!(tested_bad_ip.exit_ip, None);

        // 3. Verify record_test_result_in redacts proxy URL credentials in last_error
        let px_err = create_proxy_in(
            &conn,
            &ProxyInput {
                name: "Err Redact".into(),
                kind: "socks5".into(),
                host: "127.0.0.1".into(),
                port: 65432,
                username: Some("u_err".into()),
                password: Some("my@sec:pass".into()),
                clear_password: None,
                note: None,
            },
        )
        .expect("create err proxy");
        let recorded = record_test_result_in(
            &conn,
            &px_err.id,
            false,
            None,
            Some(10),
            Some("error via socks5h://u_err:my@sec:pass@127.0.0.1:65432/check"),
        )
        .expect("record error");
        let err_str = recorded.last_error.expect("last_error stored");
        assert!(!err_str.contains("my@sec:pass"));
        assert!(err_str.contains("socks5h://u_err:[REDACTED]@127.0.0.1:65432/check"));

        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn test_stale_proxy_test_does_not_overwrite_edited_or_resurrect_deleted_proxy() {
        let db_path = temp_db_path("m2_stale_guard");
        let mut conn = open_db_at(&db_path).expect("open proxies.db");

        // 1. Snapshot & restore preserves encrypted password + assigned links
        let snap_px = create_proxy_in(
            &conn,
            &ProxyInput {
                name: "Snap Proxy".into(),
                kind: "http".into(),
                host: "127.0.0.1".into(),
                port: 8888,
                username: Some("u".into()),
                password: Some("pw_secret".into()),
                clear_password: None,
                note: Some("note".into()),
            },
        )
        .expect("create snap proxy");
        set_account_proxy_in(&conn, "acc_snap", Some(&snap_px.id)).expect("assign link");
        let snap = snapshot_proxy_row_in(&conn, &snap_px.id).expect("snapshot");
        delete_proxy_in(&conn, &snap_px.id).expect("delete before restore");
        assert!(get_proxy_in(&conn, &snap_px.id).is_err());
        restore_proxy_row_in(&mut conn, &snap).expect("restore snapshot");
        let restored = get_proxy_in(&conn, &snap_px.id).expect("get restored");
        assert_eq!(restored.assigned_account_ids, vec!["acc_snap".to_string()]);
        assert_eq!(
            resolve_proxy_url_in(&conn, &snap_px.id).expect("resolve restored"),
            "http://u:pw_secret@127.0.0.1:8888"
        );

        // 2. Concurrent edit mid-test must not have its untested status overwritten by stale test result
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind slow proxy");
        let port = listener.local_addr().unwrap().port();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();

        let server = std::thread::spawn(move || {
            for i in 0..2 {
                if let Ok((mut stream, _)) = listener.accept() {
                    let mut buf = [0u8; 2048];
                    let _ = stream.read(&mut buf);
                    if i == 0 {
                        let _ = entered_tx.send(());
                        let _ = release_rx.recv();
                    }
                    let body = if i == 0 { "ok" } else { "{\"ip\":\"203.0.113.9\"}" };
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes());
                }
            }
        });

        let edited_px = create_proxy_in(
            &conn,
            &ProxyInput {
                name: "Edit Mid-Test".into(),
                kind: "http".into(),
                host: "127.0.0.1".into(),
                port,
                username: None,
                password: None,
                clear_password: None,
                note: None,
            },
        )
        .expect("create edited_px");
        let db_path_worker = db_path.clone();
        let id_worker = edited_px.id.clone();
        let test_worker = std::thread::spawn(move || {
            let c2 = open_db_at(&db_path_worker).expect("worker open db");
            test_proxy_connection_with_urls(
                &c2,
                &id_worker,
                "http://example.com/",
                &["http://api.ipify.org/?format=json"],
            )
        });

        entered_rx.recv().expect("server accepted request");
        let updated_mid_test = update_proxy_in(
            &conn,
            &edited_px.id,
            &ProxyInput {
                name: "Edited Endpoint".into(),
                kind: "http".into(),
                host: "127.0.0.1".into(),
                port: port.saturating_add(1).max(1),
                username: None,
                password: None,
                clear_password: None,
                note: None,
            },
        )
        .expect("update mid-test");
        assert_eq!(updated_mid_test.last_status, "untested");
        release_tx.send(()).expect("release server");
        let test_res = test_worker.join().unwrap().expect("worker returned fresh row");
        server.join().unwrap();

        assert_eq!(test_res.last_status, "untested");
        assert_eq!(test_res.exit_ip, None);
        assert_eq!(get_proxy_in(&conn, &edited_px.id).unwrap().last_status, "untested");

        // 3. Delete mid-test must fail with PROXY_NOT_FOUND and never resurrect
        let del_px = create_proxy_in(
            &conn,
            &ProxyInput {
                name: "Del Mid-Test".into(),
                kind: "http".into(),
                host: "127.0.0.1".into(),
                port: 1,
                username: None,
                password: None,
                clear_password: None,
                note: None,
            },
        )
        .expect("create del_px");
        let del_fp = EndpointFingerprint {
            kind: "http".into(),
            host: "127.0.0.1".into(),
            port: 1,
            username: None,
            password: None,
        };
        delete_proxy_in(&conn, &del_px.id).expect("delete proxy");
        let stale_err = record_test_result_if_unchanged_in(
            &conn,
            &del_px.id,
            &del_fp,
            true,
            Some("1.2.3.4"),
            Some(5),
            None,
        )
        .expect_err("deleted proxy must not be resurrected");
        assert_eq!(stale_err.code, "PROXY_NOT_FOUND");
        assert!(get_proxy_in(&conn, &del_px.id).is_err());

        let _ = std::fs::remove_file(&db_path);
    }
}
