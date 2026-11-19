//! state.db theo sơ đồ 13.1: 6 bảng (+ meta), khóa ngoại bật, đường dẫn lưu tương đối.
//! Mỗi dự án có đúng một state.db nằm ở gốc thư mục dự án.

use rusqlite::Connection;
use std::path::Path;
use std::time::Duration;

pub const SCHEMA_VERSION: i64 = 2;

/// Kiểu trả về chuẩn của `latest_job_status`:
/// `(status, image_status, video_status, last_error, attempts_count)`
pub type LatestJobStatus = (String, String, String, Option<String>, i64);

pub fn open(project_path: &Path) -> Result<Connection, rusqlite::Error> {
    let db_path = project_path.join("state.db");
    let conn = Connection::open(db_path)?;
    conn.busy_timeout(Duration::from_millis(5000))?;
    // Luôn bật foreign_keys, WAL và busy_timeout trên MỌI kết nối mở ra (kể cả khi user_version >= SCHEMA_VERSION)
    // để 1–3 luồng worker song song + luồng đọc UI không bao giờ đụng SQLITE_BUSY hay sót WAL.
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA journal_mode = WAL;
         PRAGMA busy_timeout = 5000;",
    )?;

    let user_ver: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if user_ver < SCHEMA_VERSION {
        create_schema(&conn)?;
    }
    Ok(conn)
}

pub fn create_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS runs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            kind TEXT NOT NULL CHECK (kind IN ('chung','rieng','lich','lam_lai')),
            status TEXT NOT NULL CHECK (status IN ('dang_chay','hoan_tat','da_dung','gian_doan')),
            started_at TEXT NOT NULL,
            finished_at TEXT,
            total_rows INTEGER NOT NULL DEFAULT 0,
            done_count INTEGER NOT NULL DEFAULT 0,
            error_count INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_runs_status ON runs(status);
        CREATE INDEX IF NOT EXISTS idx_runs_started ON runs(started_at);

        CREATE TABLE IF NOT EXISTS jobs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            run_id INTEGER NOT NULL REFERENCES runs(id),
            row_key TEXT NOT NULL,
            row_no INTEGER,
            status TEXT NOT NULL CHECK (status IN
                ('dang_cho','da_tao_image','da_tao_video','loi_tao_image','loi_tao_video','bo_qua')),
            image_status TEXT NOT NULL DEFAULT 'chua_chay' CHECK (image_status IN
                ('chua_chay','dang_chay','xong','loi')),
            video_status TEXT NOT NULL DEFAULT 'chua_chay' CHECK (video_status IN
                ('chua_chay','dang_chay','xong','loi')),
            image_prompt_hash TEXT,
            video_prompt_hash TEXT,
            video_task_id TEXT,
            video_download_url TEXT,
            attempts_count INTEGER NOT NULL DEFAULT 0,
            last_error TEXT,
            started_at TEXT,
            finished_at TEXT,
            duration_ms INTEGER
        );
        CREATE UNIQUE INDEX IF NOT EXISTS uq_jobs_run_row ON jobs(run_id, row_key);
        CREATE INDEX IF NOT EXISTS idx_jobs_row ON jobs(row_key);
        CREATE INDEX IF NOT EXISTS idx_jobs_status ON jobs(status);

        CREATE TABLE IF NOT EXISTS file_refs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            job_id INTEGER REFERENCES jobs(id),
            row_key TEXT NOT NULL,
            kind TEXT NOT NULL CHECK (kind IN ('tham_chieu','anh','video')),
            file_name TEXT NOT NULL,
            file_path TEXT NOT NULL,
            version INTEGER NOT NULL DEFAULT 1,
            is_current INTEGER NOT NULL DEFAULT 1,
            size_bytes INTEGER,
            created_at TEXT NOT NULL
        );
        CREATE UNIQUE INDEX IF NOT EXISTS uq_refs_row_kind_ver ON file_refs(row_key, kind, version);
        CREATE INDEX IF NOT EXISTS idx_refs_row ON file_refs(row_key, kind, is_current);

        CREATE TABLE IF NOT EXISTS attempts (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            job_id INTEGER NOT NULL REFERENCES jobs(id),
            step TEXT NOT NULL CHECK (step IN ('B2','B3','download')),
            attempt_no INTEGER NOT NULL,
            started_at TEXT,
            finished_at TEXT,
            result TEXT NOT NULL CHECK (result IN ('ok','loi_tam_thoi','loi_nghiep_vu')),
            http_status INTEGER,
            error_code TEXT,
            error_message TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_attempts_job ON attempts(job_id, step);

        CREATE TABLE IF NOT EXISTS settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS logs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            ts TEXT NOT NULL,
            level TEXT NOT NULL CHECK (level IN ('info','warn','error')),
            run_id INTEGER REFERENCES runs(id),
            row_key TEXT,
            step TEXT CHECK (step IN ('B1','B2','B3','download','dong_bo','ghi_sheet')),
            message TEXT NOT NULL,
            detail TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_logs_ts ON logs(ts);
        CREATE INDEX IF NOT EXISTS idx_logs_run ON logs(run_id);
        "#,
    )?;

    // Migration idempotent v1 -> v2: bổ sung cột lưu video_task_id & video_download_url
    ensure_column(conn, "jobs", "video_task_id", "TEXT")?;
    ensure_column(conn, "jobs", "video_download_url", "TEXT")?;

    conn.execute(
        "INSERT INTO meta(key, value) VALUES ('schema_version', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![SCHEMA_VERSION.to_string()],
    )?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    col_type: &str,
) -> Result<(), rusqlite::Error> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name.eq_ignore_ascii_case(column) {
            return Ok(());
        }
    }
    drop(rows);
    drop(stmt);
    conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {col_type};"))?;
    Ok(())
}

/// Phục hồi các lượt đang dở (`status = 'dang_chay'`) sang `'gian_doan'` khi mở lại dự án/app.
pub fn recover_interrupted_runs(conn: &Connection) -> Result<usize, rusqlite::Error> {
    let now = chrono::Local::now().to_rfc3339();
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "UPDATE jobs
         SET image_status = CASE WHEN image_status = 'dang_chay' THEN 'chua_chay' ELSE image_status END,
             video_status = CASE WHEN video_status = 'dang_chay' THEN 'chua_chay' ELSE video_status END
         WHERE run_id IN (SELECT id FROM runs WHERE status = 'dang_chay')
           AND (image_status = 'dang_chay' OR video_status = 'dang_chay')",
        [],
    )?;
    let updated = tx.execute(
        "UPDATE runs SET status = 'gian_doan', finished_at = ?1 WHERE status = 'dang_chay'",
        rusqlite::params![now],
    )?;
    tx.commit()?;
    Ok(updated)
}

/// Tự động dọn lịch sử và nhật ký cũ quá `days` ngày (mục 4.2 & 6.5):
/// - Nếu `days == 0`: không dọn, trả `Ok((0, 0))`.
/// - Xóa `logs` cũ hơn `days` ngày.
/// - Xóa các bản ghi `jobs` KHÔNG phải bản mới nhất của từng dòng (`MAX(id)` theo `LOWER(row_key)`)
///   thuộc các lượt `runs` đã kết thúc (`status != 'dang_chay' AND total_rows > 0`) cũ hơn `days` ngày
///   (gỡ tham chiếu `file_refs.job_id = NULL` và xóa `attempts` đi kèm trước để bảo toàn 100% `file_refs`!).
/// - Xóa các lượt `runs` đã kết thúc cũ hơn `days` ngày không còn bản ghi `jobs` nào.
///
/// Trả về `(deleted_runs, deleted_logs)`.
pub fn cleanup_old_history(conn: &Connection, days: u32) -> Result<(usize, usize), rusqlite::Error> {
    if days == 0 {
        return Ok((0, 0));
    }
    let modifier = format!("-{days} days");
    let tx = conn.unchecked_transaction()?;

    // 1) Xóa logs quá hạn
    let deleted_logs = tx.execute(
        "DELETE FROM logs WHERE datetime(ts) < datetime('now', ?1)",
        rusqlite::params![modifier],
    )?;

    // 2) Tách liên kết file_refs khỏi các jobs lịch sử cũ sắp xóa để giữ nguyên 100% file_refs
    let stale_jobs_subquery = "
        SELECT j.id FROM jobs j
        INNER JOIN runs r ON r.id = j.run_id
        WHERE r.status != 'dang_chay'
          AND r.total_rows > 0
          AND datetime(COALESCE(r.finished_at, r.started_at)) < datetime('now', ?1)
          AND j.id NOT IN (SELECT MAX(id) FROM jobs GROUP BY LOWER(row_key))
    ";

    tx.execute(
        &format!("UPDATE file_refs SET job_id = NULL WHERE job_id IN ({stale_jobs_subquery})"),
        rusqlite::params![modifier],
    )?;

    tx.execute(
        &format!("DELETE FROM attempts WHERE job_id IN ({stale_jobs_subquery})"),
        rusqlite::params![modifier],
    )?;

    tx.execute(
        &format!("DELETE FROM jobs WHERE id IN ({stale_jobs_subquery})"),
        rusqlite::params![modifier],
    )?;

    // 3) Gỡ run_id trên logs còn lại (nếu có log mới trỏ tới run cũ sắp xóa) rồi xóa runs trống quá hạn
    let stale_empty_runs_subquery = "
        SELECT r.id FROM runs r
        WHERE r.status != 'dang_chay'
          AND r.total_rows > 0
          AND datetime(COALESCE(r.finished_at, r.started_at)) < datetime('now', ?1)
          AND NOT EXISTS (SELECT 1 FROM jobs j WHERE j.run_id = r.id)
    ";

    tx.execute(
        &format!("UPDATE logs SET run_id = NULL WHERE run_id IN ({stale_empty_runs_subquery})"),
        rusqlite::params![modifier],
    )?;

    let deleted_runs = tx.execute(
        &format!("DELETE FROM runs WHERE id IN ({stale_empty_runs_subquery})"),
        rusqlite::params![modifier],
    )?;

    tx.commit()?;
    Ok((deleted_runs, deleted_logs))
}

/// Trạng thái dòng mới nhất theo row_key (jobs mới nhất theo id)
pub fn latest_job_status(
    conn: &Connection,
    row_key: &str,
) -> Result<Option<LatestJobStatus>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT status, image_status, video_status, last_error, attempts_count
         FROM jobs WHERE LOWER(row_key) = LOWER(?1) ORDER BY id DESC LIMIT 1",
    )?;
    let mut rows = stmt.query(rusqlite::params![row_key])?;
    if let Some(r) = rows.next()? {
        Ok(Some((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
        )))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_project_dir(prefix: &str) -> std::path::PathBuf {
        let idx = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "videoflow_state_db_test_{}_{}_{}",
            prefix,
            std::process::id(),
            idx
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp test dir");
        dir
    }

    #[test]
    fn test_fresh_schema_v2_and_fast_reopen_keeps_wal() {
        let dir = temp_project_dir("fresh");
        {
            let conn = open(&dir).expect("open fresh state.db");
            let user_ver: i64 = conn
                .pragma_query_value(None, "user_version", |r| r.get(0))
                .expect("read user_version");
            assert_eq!(user_ver, SCHEMA_VERSION);

            let meta_ver: String = conn
                .query_row(
                    "SELECT value FROM meta WHERE key = 'schema_version'",
                    [],
                    |r| r.get(0),
                )
                .expect("read meta schema_version");
            assert_eq!(meta_ver, "2");

            conn.execute(
                "INSERT INTO runs(kind, status, started_at, total_rows) VALUES ('chung','dang_chay','2026-11-18T00:00:00Z',1)",
                [],
            )
            .expect("insert run");
            let run_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO jobs(run_id, row_key, status, video_task_id, video_download_url, attempts_count)
                 VALUES (?1, 'cat.png', 'da_tao_image', 'task_123', 'https://cdn.example.com/out.mp4', 1)",
                rusqlite::params![run_id],
            )
            .expect("insert job with v2 columns");

            let st = latest_job_status(&conn, "CAT.PNG")
                .expect("query latest_job_status case-insensitive")
                .expect("job exists");
            assert_eq!(st.0, "da_tao_image");
            assert_eq!(st.4, 1);
        }

        // Reopen existing v2 database: WAL, foreign_keys, busy_timeout vẫn được đảm bảo 100%
        {
            let conn = open(&dir).expect("reopen v2 state.db");
            let fk: i64 = conn
                .pragma_query_value(None, "foreign_keys", |r| r.get(0))
                .expect("read foreign_keys");
            assert_eq!(fk, 1);

            let mode: String = conn
                .pragma_query_value(None, "journal_mode", |r| r.get(0))
                .expect("read journal_mode");
            assert_eq!(mode.to_lowercase(), "wal");

            let (task_id, dl_url): (Option<String>, Option<String>) = conn
                .query_row(
                    "SELECT video_task_id, video_download_url FROM jobs WHERE row_key = 'cat.png'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .expect("read v2 columns after reopen");
            assert_eq!(task_id.as_deref(), Some("task_123"));
            assert_eq!(dl_url.as_deref(), Some("https://cdn.example.com/out.mp4"));
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_migrate_v1_to_v2_idempotent() {
        let dir = temp_project_dir("migrate_v1");
        let db_path = dir.join("state.db");
        {
            let conn = Connection::open(&db_path).expect("open raw v1 db");
            conn.execute_batch(
                r#"
                CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                INSERT INTO meta(key, value) VALUES ('schema_version', '1');
                CREATE TABLE runs (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    kind TEXT NOT NULL,
                    status TEXT NOT NULL,
                    started_at TEXT NOT NULL,
                    finished_at TEXT,
                    total_rows INTEGER NOT NULL DEFAULT 0,
                    done_count INTEGER NOT NULL DEFAULT 0,
                    error_count INTEGER NOT NULL DEFAULT 0
                );
                CREATE TABLE jobs (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    run_id INTEGER NOT NULL REFERENCES runs(id),
                    row_key TEXT NOT NULL,
                    row_no INTEGER,
                    status TEXT NOT NULL,
                    image_status TEXT NOT NULL DEFAULT 'chua_chay',
                    video_status TEXT NOT NULL DEFAULT 'chua_chay',
                    image_prompt_hash TEXT,
                    video_prompt_hash TEXT,
                    attempts_count INTEGER NOT NULL DEFAULT 0,
                    last_error TEXT,
                    started_at TEXT,
                    finished_at TEXT,
                    duration_ms INTEGER
                );
                INSERT INTO runs(kind, status, started_at, total_rows) VALUES ('chung', 'hoan_tat', '2026-11-17T00:00:00Z', 1);
                INSERT INTO jobs(run_id, row_key, status) VALUES (1, 'old.png', 'da_tao_image');
                "#,
            )
            .expect("setup v1 schema");
        }

        let conn = open(&dir).expect("open & migrate v1 -> v2");
        create_schema(&conn).expect("idempotent create_schema");

        let user_ver: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .expect("user_version");
        assert_eq!(user_ver, 2);

        conn.execute(
            "UPDATE jobs SET video_task_id = 'migrated_task', video_download_url = 'https://example.com/v.mp4' WHERE row_key = 'old.png'",
            [],
        )
        .expect("update newly added columns on v1 row");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_recover_interrupted_runs() {
        let dir = temp_project_dir("recover");
        let conn = open(&dir).expect("open db");

        conn.execute(
            "INSERT INTO runs(kind, status, started_at, total_rows) VALUES ('chung', 'dang_chay', '2026-11-18T01:00:00Z', 3)",
            [],
        )
        .expect("insert interrupted run");
        let interrupted_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO jobs(run_id, row_key, status, image_status, video_status)
             VALUES (?1, 'a.png', 'da_tao_image', 'xong', 'dang_chay')",
            rusqlite::params![interrupted_id],
        )
        .expect("insert stuck job");

        let count = recover_interrupted_runs(&conn).expect("recover_interrupted_runs");
        assert_eq!(count, 1);

        let st1: String = conn
            .query_row(
                "SELECT status FROM runs WHERE id = ?1",
                rusqlite::params![interrupted_id],
                |r| r.get(0),
            )
            .expect("check interrupted run");
        assert_eq!(st1, "gian_doan");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cleanup_old_history_preserves_latest_jobs_and_file_refs() {
        let dir = temp_project_dir("cleanup");
        let conn = open(&dir).expect("open db");

        // Run #1 cũ 60 ngày trước: có a.png (sẽ có job mới hơn ở Run #2) và only_once.png (chỉ có 1 job duy nhất -> phải giữ!)
        conn.execute(
            "INSERT INTO runs(kind, status, started_at, finished_at, total_rows)
             VALUES ('chung', 'hoan_tat', '2020-01-01T00:00:00Z', '2020-01-01T00:10:00Z', 2)",
            [],
        )
        .expect("insert old run 1");
        let run1 = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO jobs(run_id, row_key, status) VALUES (?1, 'a.png', 'da_tao_video')",
            rusqlite::params![run1],
        )
        .expect("insert old job a");
        let job_a_old = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO file_refs(job_id, row_key, kind, file_name, file_path, version, is_current, created_at)
             VALUES (?1, 'a.png', 'anh', 'a_gen.png', 'image_out/a_gen.png', 1, 0, '2020-01-01T00:05:00Z')",
            rusqlite::params![job_a_old],
        )
        .expect("insert file_ref v1 linked to old job");

        conn.execute(
            "INSERT INTO attempts(job_id, step, attempt_no, result) VALUES (?1, 'B2', 1, 'ok')",
            rusqlite::params![job_a_old],
        )
        .expect("insert attempt on old job");

        // Run #0 cũ chỉ chứa 'a.png' (nên sau khi xóa job_a_old_0 thì Run #0 trống jobs -> bị xóa luôn!)
        conn.execute(
            "INSERT INTO runs(kind, status, started_at, finished_at, total_rows)
             VALUES ('chung', 'hoan_tat', '2019-12-01T00:00:00Z', '2019-12-01T00:05:00Z', 1)",
            [],
        )
        .expect("insert old run 0");
        let run0 = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO jobs(run_id, row_key, status) VALUES (?1, 'b_superseded.png', 'loi_tao_image')",
            rusqlite::params![run0],
        )
        .expect("insert superseded job");

        // Run #2 mới hôm nay: cập nhật cả a.png và b_superseded.png
        let now_ts = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO runs(kind, status, started_at, finished_at, total_rows)
             VALUES ('chung', 'hoan_tat', ?1, ?1, 2)",
            rusqlite::params![now_ts],
        )
        .expect("insert recent run 2");
        let run2 = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO jobs(run_id, row_key, status) VALUES (?1, 'a.png', 'da_tao_video')",
            rusqlite::params![run2],
        )
        .expect("insert new job a");
        conn.execute(
            "INSERT INTO jobs(run_id, row_key, status) VALUES (?1, 'b_superseded.png', 'da_tao_video')",
            rusqlite::params![run2],
        )
        .expect("insert new job b");

        // Thêm job duy nhất cho 'only_once.png' vào Run #1 cũ -> Run #1 phải được giữ vì còn giữ latest job của only_once.png!
        conn.execute(
            "INSERT INTO jobs(run_id, row_key, status) VALUES (?1, 'only_once.png', 'da_tao_video')",
            rusqlite::params![run1],
        )
        .expect("insert unique row job in old run1");

        // Logs: 1 cũ, 1 mới
        conn.execute(
            "INSERT INTO logs(ts, level, run_id, message) VALUES ('2019-12-01T00:01:00Z', 'info', ?1, 'old log')",
            rusqlite::params![run0],
        )
        .expect("insert old log");
        conn.execute(
            "INSERT INTO logs(ts, level, run_id, message) VALUES (?1, 'info', ?2, 'recent log')",
            rusqlite::params![now_ts, run2],
        )
        .expect("insert new log");

        // days == 0 là no-op
        assert_eq!(cleanup_old_history(&conn, 0).expect("days=0"), (0, 0));

        let (del_runs, del_logs) = cleanup_old_history(&conn, 30).expect("cleanup 30d");
        assert_eq!(del_runs, 1, "run0 had only superseded jobs so it must be deleted");
        assert_eq!(del_logs, 1, "old log must be deleted");

        // file_refs của v1 cũ vẫn phải còn nguyên 100%
        let ref_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM file_refs WHERE row_key = 'a.png'", [], |r| r.get(0))
            .expect("count file_refs");
        assert_eq!(ref_count, 1);

        // Latest job của only_once.png (nằm ở run1 cũ) vẫn được bảo toàn!
        assert!(latest_job_status(&conn, "only_once.png").unwrap().is_some());
        assert!(latest_job_status(&conn, "a.png").unwrap().is_some());
        assert!(latest_job_status(&conn, "b_superseded.png").unwrap().is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
