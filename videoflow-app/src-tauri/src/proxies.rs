//! Quản lý Proxy toàn cục của ứng dụng (`%APPDATA%/VideoFlowDesktop/proxies.db`).
//! Rust là nguồn sự thật duy nhất (Single Source of Truth) cho cấu hình Proxy
//! và liên kết `account_id -> proxy_id`, tự động resolve sang URL proxy đầy đủ
//! khi đồng bộ xuống sidecar.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};

pub const PROXIES_DB_FILE: &str = "proxies.db";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyItem {
    pub id: String,
    pub name: String,
    pub kind: String, // "http" | "socks5"
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyInput {
    pub name: String,
    pub kind: String,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    pub note: Option<String>,
}

pub fn default_db_path() -> PathBuf {
    let dir = crate::settings::app_data_dir();
    let _ = std::fs::create_dir_all(&dir);
    dir.join(PROXIES_DB_FILE)
}

pub fn open_db_at(path: &Path) -> AppResult<Connection> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(path)
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

pub fn format_proxy_url(
    kind: &str,
    host: &str,
    port: u16,
    username: Option<&str>,
    password: Option<&str>,
) -> String {
    let scheme = if kind.eq_ignore_ascii_case("socks5") {
        "socks5"
    } else {
        "http"
    };
    let clean_host = host.trim();
    let host_part = if clean_host.contains(':') && !clean_host.starts_with('[') {
        format!("[{clean_host}]")
    } else {
        clean_host.to_string()
    };

    let user = username.map(str::trim).filter(|s| !s.is_empty());
    let pass = password.map(str::trim).filter(|s| !s.is_empty());
    match (user, pass) {
        (Some(u), Some(p)) => format!(
            "{scheme}://{}:{}@{host_part}:{port}",
            url_encode_userinfo(u),
            url_encode_userinfo(p)
        ),
        (Some(u), None) => format!("{scheme}://{}@{host_part}:{port}", url_encode_userinfo(u)),
        _ => format!("{scheme}://{host_part}:{port}"),
    }
}

impl ProxyItem {
    pub fn to_proxy_url(&self) -> String {
        format_proxy_url(
            &self.kind,
            &self.host,
            self.port,
            self.username.as_deref(),
            self.password.as_deref(),
        )
    }

    pub fn to_masked_url(&self) -> String {
        let scheme = if self.kind.eq_ignore_ascii_case("socks5") {
            "socks5"
        } else {
            "http"
        };
        let has_user = self
            .username
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        if has_user || self.has_password {
            format!("{scheme}://***@{}:{}", self.host, self.port)
        } else {
            format!("{scheme}://{}:{}", self.host, self.port)
        }
    }
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
    let host = input
        .host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim()
        .to_string();
    if host.is_empty()
        || host.contains('/')
        || host.contains('\\')
        || host.contains('@')
        || host.chars().any(char::is_whitespace)
    {
        return Err(AppError::new(
            "PROXY_INVALID",
            "Host/IP proxy không hợp lệ (không chứa khoảng trắng, '/', '@' hoặc đường dẫn).",
        ));
    }
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
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
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
            let password: Option<String> = r.get(6)?;
            let has_password = password.as_ref().map(|s| !s.is_empty()).unwrap_or(false);
            Ok(ProxyItem {
                id,
                name: r.get(1)?,
                kind: r.get(2)?,
                host: r.get(3)?,
                port: r.get::<_, i64>(4)? as u16,
                username: r.get(5)?,
                password,
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
                let password: Option<String> = r.get(6)?;
                let has_password = password.as_ref().map(|s| !s.is_empty()).unwrap_or(false);
                Ok(ProxyItem {
                    id,
                    name: r.get(1)?,
                    kind: r.get(2)?,
                    host: r.get(3)?,
                    port: r.get::<_, i64>(4)? as u16,
                    username: r.get(5)?,
                    password,
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
    conn.execute(
        "INSERT INTO proxies (
            id, name, kind, host, port, username, password, note,
            last_status, created_at, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'untested', ?9, ?9)",
        params![id, v.name, v.kind, v.host, v.port as i64, v.username, v.password, v.note, now],
    )
    .map_err(|e| AppError::new("PROXY_DB_WRITE", format!("Không lưu được proxy mới: {e}")))?;
    get_proxy_in(conn, &id)
}

pub fn update_proxy_in(conn: &Connection, proxy_id: &str, input: &ProxyInput) -> AppResult<ProxyItem> {
    let existing = get_proxy_in(conn, proxy_id)?;
    let v = validate_input(input)?;
    let now = chrono::Local::now().to_rfc3339();

    let endpoint_changed = existing.kind != v.kind
        || existing.host != v.host
        || existing.port != v.port
        || existing.username != v.username
        || existing.password != v.password;

    if endpoint_changed {
        conn.execute(
            "UPDATE proxies
             SET name = ?1, kind = ?2, host = ?3, port = ?4,
                 username = ?5, password = ?6, note = ?7,
                 last_status = 'untested', exit_ip = NULL, latency_ms = NULL,
                 tested_at = NULL, last_error = NULL, updated_at = ?8
             WHERE id = ?9",
            params![v.name, v.kind, v.host, v.port as i64, v.username, v.password, v.note, now, proxy_id],
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

/// Kiểm tra kết nối THẬT qua HTTP/SOCKS5 proxy tới `target_url` (mặc định `https://muse.ai`)
/// và truy vấn Exit IP thực tế từ `ip_urls`.
pub fn test_proxy_connection_with_urls(
    conn: &Connection,
    proxy_id: &str,
    target_url: &str,
    ip_urls: &[&str],
) -> AppResult<ProxyItem> {
    let proxy = get_proxy_in(conn, proxy_id)?;
    let proxy_url = proxy.to_proxy_url();

    let req_proxy = match reqwest::Proxy::all(&proxy_url) {
        Ok(p) => p,
        Err(e) => {
            return record_test_result_in(
                conn,
                proxy_id,
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
            return record_test_result_in(
                conn,
                proxy_id,
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
            return record_test_result_in(
                conn,
                proxy_id,
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
        return record_test_result_in(
            conn,
            proxy_id,
            false,
            None,
            Some(elapsed),
            Some("Proxy từ chối xác thực (HTTP 407 Proxy Authentication Required)."),
        );
    }
    if code >= 500 {
        return record_test_result_in(
            conn,
            proxy_id,
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
                                let clean = ip_str.trim();
                                if !clean.is_empty() && clean.len() <= 64 {
                                    detected_ip = Some(clean.to_string());
                                    break;
                                }
                            }
                        }
                    } else if !trimmed.is_empty()
                        && trimmed.len() <= 64
                        && !trimmed.contains('<')
                        && !trimmed.contains(' ')
                    {
                        detected_ip = Some(trimmed.to_string());
                        break;
                    }
                }
            }
        }
    }

    record_test_result_in(conn, proxy_id, true, detected_ip.as_deref(), Some(elapsed), None)
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
                note: Some("Ghi chú US".into()),
            },
        )
        .expect("create proxy");

        assert!(created.id.starts_with("px_"));
        assert_eq!(created.name, "Proxy US-01");
        assert_eq!(created.kind, "socks5");
        assert_eq!(created.host, "198.51.100.10");
        assert_eq!(created.port, 1080);
        assert!(created.has_password);
        assert_eq!(created.last_status, "untested");
        assert_eq!(
            created.to_proxy_url(),
            "socks5://user%40name:p%40ss%3A123@198.51.100.10:1080"
        );
        assert_eq!(created.to_masked_url(), "socks5://***@198.51.100.10:1080");

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
                note: None,
            },
        )
        .expect("update host");
        assert_eq!(host_updated.last_status, "untested");
        assert_eq!(host_updated.exit_ip, None);

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
}
