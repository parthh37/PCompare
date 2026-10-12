//! The index: an embedded SQLite database in the app's data directory that
//! makes folder scans faster and folder syncs crash-safe.
//!
//! It holds three kinds of state that outlive a single comparison:
//! - a [`checksums`] cache, so rescans skip reading files whose size and
//!   modification time haven't changed;
//! - sync [`baselines`], the state of every path after the last good sync of
//!   a folder pair, which is what makes true two-way sync possible;
//! - a [`jobs`] journal of every sync run and its operations, so a run cut
//!   short by a crash or power loss can be resumed or rolled back, and so
//!   the user has a history to look at.
//!
//! SQLite runs in WAL mode with `synchronous=FULL`: a committed transaction
//! survives power loss, and readers never block the writer. The schema is
//! versioned with `PRAGMA user_version`; see [`MIGRATIONS`].
//!
//! The index is a cache and a safety net, never the only copy of user data,
//! but it is still treated with care: a database that SQLite reports as
//! corrupt is moved aside (`<name>.corrupt-<unix_ms>`) and replaced with a
//! fresh one rather than deleted.

pub mod automation; // automation
pub mod baselines;
pub mod checksums;
pub mod jobs;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use pcompare_core::store::now_ms;
use rusqlite::{Connection, ErrorCode};
use serde::Serialize;

pub use automation::{AutomationRun, NewRun, RunFinish, RunStatus, RunTrigger, TaskState}; // automation
pub use checksums::CachedChecksums;
pub use jobs::{JobDetail, JobJournal, JobOp, JobStatus, JobSummary, NewJob, OpState};

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("index database: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("index database: bad stored JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// `PRAGMA quick_check` found damage.
    #[error("index database is corrupt: {0}")]
    Corrupt(String),
    /// The file was written by a newer PCompare; it is left untouched.
    #[error(
        "index database has schema version {found}, newer than this build supports ({supported})"
    )]
    TooNew { found: i64, supported: i64 },
    #[error("sync job {0} not found")]
    JobNotFound(String),
    /// A stored value this build doesn't understand.
    #[error("index database: invalid stored value: {0}")]
    Invalid(String),
}

impl IndexError {
    fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        IndexError::Io {
            path: path.into(),
            source,
        }
    }

    /// Whether this error means the database file itself is damaged.
    fn is_corruption(&self) -> bool {
        match self {
            IndexError::Corrupt(_) => true,
            IndexError::Sqlite(e) => matches!(
                e.sqlite_error_code(),
                Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase)
            ),
            _ => false,
        }
    }
}

pub type Result<T> = std::result::Result<T, IndexError>;

/// Schema migrations, oldest first. Entry `i` takes the database from
/// `user_version` `i` to `i + 1`; append new entries, never edit old ones.
pub const MIGRATIONS: &[&str] = &[
    // v1: initial schema.
    r#"
    CREATE TABLE checksums (
        path     TEXT PRIMARY KEY,
        size     INTEGER NOT NULL,
        mtime_ms INTEGER NOT NULL,
        crc      INTEGER NOT NULL,
        seen_ms  INTEGER NOT NULL
    ) WITHOUT ROWID;

    CREATE TABLE pairs (
        id         INTEGER PRIMARY KEY,
        left       TEXT NOT NULL,
        right      TEXT NOT NULL,
        updated_ms INTEGER NOT NULL,
        UNIQUE (left, right)
    );

    CREATE TABLE baseline (
        pair_id  INTEGER NOT NULL REFERENCES pairs (id) ON DELETE CASCADE,
        rel_path TEXT NOT NULL,
        is_dir   INTEGER NOT NULL,
        l_size   INTEGER NOT NULL,
        l_mtime  INTEGER,
        r_size   INTEGER NOT NULL,
        r_mtime  INTEGER,
        PRIMARY KEY (pair_id, rel_path)
    ) WITHOUT ROWID;

    CREATE TABLE jobs (
        id           TEXT PRIMARY KEY,
        left         TEXT NOT NULL,
        right        TEXT NOT NULL,
        mode         TEXT NOT NULL,
        options      TEXT NOT NULL,
        status       TEXT NOT NULL,
        started_ms   INTEGER NOT NULL,
        finished_ms  INTEGER,
        total_ops    INTEGER NOT NULL,
        succeeded    INTEGER,
        failed       INTEGER,
        bytes_copied INTEGER,
        cancelled    INTEGER NOT NULL DEFAULT 0,
        stopped      TEXT,
        elapsed_ms   INTEGER
    );
    CREATE INDEX jobs_by_pair ON jobs (left, right, started_ms);
    CREATE INDEX jobs_by_start ON jobs (started_ms);
    CREATE INDEX jobs_by_status ON jobs (status);

    CREATE TABLE job_ops (
        job_id     TEXT NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
        op_id      INTEGER NOT NULL,
        op         TEXT NOT NULL,
        state      TEXT NOT NULL,
        attempts   INTEGER NOT NULL DEFAULT 0,
        staged     TEXT,
        result     TEXT,
        bytes      INTEGER NOT NULL DEFAULT 0,
        updated_ms INTEGER NOT NULL,
        PRIMARY KEY (job_id, op_id)
    ) WITHOUT ROWID;
    "#,
    // automation:engine
    // v2: sync automation run history and per-task scheduler state (see
    // `automation`). `notifications` is a JSON array of delivery results.
    r#"
    CREATE TABLE automation_runs (
        id            TEXT PRIMARY KEY,
        task_id       TEXT NOT NULL,
        task_name     TEXT NOT NULL,
        run_trigger   TEXT NOT NULL,
        attempt       INTEGER NOT NULL,
        status        TEXT NOT NULL,
        started_ms    INTEGER NOT NULL,
        finished_ms   INTEGER,
        job_id        TEXT,
        ops_total     INTEGER NOT NULL DEFAULT 0,
        ops_done      INTEGER NOT NULL DEFAULT 0,
        ops_failed    INTEGER NOT NULL DEFAULT 0,
        conflicts     INTEGER NOT NULL DEFAULT 0,
        bytes_copied  INTEGER NOT NULL DEFAULT 0,
        message       TEXT,
        notifications TEXT NOT NULL DEFAULT '[]'
    );
    CREATE INDEX automation_runs_by_task ON automation_runs (task_id, started_ms);
    CREATE INDEX automation_runs_by_start ON automation_runs (started_ms);

    CREATE TABLE automation_state (
        task_id                   TEXT PRIMARY KEY,
        last_run_ms               INTEGER,
        last_status               TEXT,
        last_success_ms           INTEGER,
        consecutive_failures      INTEGER NOT NULL DEFAULT 0,
        last_notified_ms          INTEGER,
        last_notified_fingerprint TEXT
    ) WITHOUT ROWID;
    "#,
    // automation:engine end
];

/// Schema version this build writes.
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

/// The index database. Cheap to share: every method takes `&self` and
/// serializes access to the one connection internally.
pub struct Index {
    conn: Mutex<Connection>,
    path: Option<PathBuf>,
}

/// Sizes of what the index holds, for a settings page.
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct IndexStats {
    pub db_bytes: u64,
    pub checksums: usize,
    /// Folder pairs with a stored baseline.
    pub baselines: usize,
    pub baseline_entries: usize,
    pub jobs: usize,
}

impl Index {
    /// Opens (creating if needed) the index at `path` and brings its schema
    /// up to date. A corrupt file is moved aside and replaced; see the
    /// module docs.
    pub fn open(path: &Path) -> Result<Index> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir).map_err(|e| IndexError::io(dir, e))?;
        }
        let conn = match open_file(path) {
            Ok(conn) => conn,
            Err(e) if e.is_corruption() => {
                let aside = quarantine(path)?;
                eprintln!(
                    "pcompare-index: {e}; moved it to {} and starting fresh",
                    aside.display()
                );
                open_file(path)?
            }
            Err(e) => return Err(e),
        };
        Ok(Index {
            conn: Mutex::new(conn),
            path: Some(path.to_path_buf()),
        })
    }

    /// A throwaway index (tests, or when the data dir is unwritable).
    pub fn open_in_memory() -> Result<Index> {
        let mut conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        migrate(&mut conn)?;
        Ok(Index {
            conn: Mutex::new(conn),
            path: None,
        })
    }

    /// The database file, or `None` for an in-memory index.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn stats(&self) -> Result<IndexStats> {
        let conn = self.conn();
        let count = |sql: &str| -> Result<usize> {
            Ok(conn
                .prepare_cached(sql)?
                .query_row([], |r| r.get::<_, i64>(0))? as usize)
        };
        let pages: i64 = conn.pragma_query_value(None, "page_count", |r| r.get(0))?;
        let page_size: i64 = conn.pragma_query_value(None, "page_size", |r| r.get(0))?;
        Ok(IndexStats {
            db_bytes: (pages * page_size) as u64,
            checksums: count("SELECT COUNT(*) FROM checksums")?,
            baselines: count("SELECT COUNT(*) FROM pairs")?,
            baseline_entries: count("SELECT COUNT(*) FROM baseline")?,
            jobs: count("SELECT COUNT(*) FROM jobs")?,
        })
    }

    /// Locks the connection. A panic while it was held leaves nothing half
    /// done (open transactions roll back on drop), so poisoning is ignored.
    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Normalizes a folder or file path for use as a key: trailing separators
/// are trimmed (a bare root such as `/` or `C:\` is kept), and on Windows,
/// whose file systems are case-insensitive, `/` becomes `\` and letters are
/// lowercased so `C:/Data` and `c:\data\` are the same key.
pub fn normalize_key(path: &str) -> String {
    let mut key = if cfg!(windows) {
        path.replace('/', "\\").to_lowercase()
    } else {
        path.to_owned()
    };
    while key.len() > 1 && key.ends_with(['/', '\\']) && !is_bare_root(&key) {
        key.pop();
    }
    key
}

/// `/`, `\`, or a drive root like `C:\`.
fn is_bare_root(key: &str) -> bool {
    let b = key.as_bytes();
    matches!(b, [b'/' | b'\\']) || matches!(b, [d, b':', b'/' | b'\\'] if d.is_ascii_alphabetic())
}

/// Opens a database file, configures the connection, checks integrity and
/// migrates. Any failure drops the connection, so the file can be renamed.
fn open_file(path: &Path) -> Result<Connection> {
    let mut conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_millis(5000))?;
    // The first statement that reads the file; garbage fails here with
    // SQLITE_NOTADB.
    let mode: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        eprintln!("pcompare-index: WAL unavailable, using journal_mode={mode}");
    }
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    let check: String = conn.pragma_query_value(None, "quick_check", |r| r.get(0))?;
    if check != "ok" {
        return Err(IndexError::Corrupt(check));
    }
    migrate(&mut conn)?;
    Ok(conn)
}

/// Applies every migration past the database's `user_version`, each in its
/// own transaction together with the version bump.
fn migrate(conn: &mut Connection) -> Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(IndexError::TooNew {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(version.max(0) as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", i as i64 + 1)?;
        tx.commit()?;
    }
    Ok(())
}

/// Renames a damaged database and its `-wal`/`-shm` companions to
/// `<name>.corrupt-<unix_ms>[-wal|-shm]`, keeping them together so the set
/// can still be opened for salvage. Returns the new database path.
fn quarantine(path: &Path) -> Result<PathBuf> {
    let suffix = format!(".corrupt-{}", now_ms());
    let with = |extra: &str| {
        let mut s = path.as_os_str().to_owned();
        s.push(extra);
        PathBuf::from(s)
    };
    let aside = with(&suffix);
    fs::rename(path, &aside).map_err(|e| IndexError::io(path, e))?;
    for companion in ["-wal", "-shm"] {
        let from = with(companion);
        match fs::rename(&from, with(&format!("{suffix}{companion}"))) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(IndexError::io(from, e)),
        }
    }
    Ok(aside)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_creates_dirs_and_persists_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/data/index.db");
        {
            let index = Index::open(&path).unwrap();
            assert_eq!(index.path(), Some(path.as_path()));
            let conn = index.conn();
            let v: i64 = conn
                .pragma_query_value(None, "user_version", |r| r.get(0))
                .unwrap();
            assert_eq!(v, SCHEMA_VERSION);
            let mode: String = conn
                .pragma_query_value(None, "journal_mode", |r| r.get(0))
                .unwrap();
            assert_eq!(mode, "wal");
            conn.execute("INSERT INTO checksums VALUES ('/x', 1, 2, 3, 4)", [])
                .unwrap();
        }
        let index = Index::open(&path).unwrap();
        assert_eq!(index.stats().unwrap().checksums, 1, "data survives reopen");
        assert!(index.stats().unwrap().db_bytes > 0);
    }

    #[test]
    fn newer_schema_is_refused_not_wiped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let index = Index::open(&path).unwrap();
            index
                .conn()
                .pragma_update(None, "user_version", SCHEMA_VERSION + 5)
                .unwrap();
        }
        assert!(matches!(Index::open(&path), Err(IndexError::TooNew { .. })));
        assert!(path.exists());
    }

    #[test]
    fn corrupt_file_is_moved_aside_and_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let garbage: Vec<u8> = (0..8192u32).map(|i| (i * 31 % 251) as u8).collect();
        fs::write(&path, &garbage).unwrap();

        let index = Index::open(&path).unwrap();
        assert_eq!(index.stats().unwrap().jobs, 0, "fresh database works");
        index
            .conn()
            .execute("INSERT INTO checksums VALUES ('/x', 1, 2, 3, 4)", [])
            .unwrap();

        let aside: Vec<PathBuf> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                let name = p.file_name().unwrap().to_string_lossy();
                name.starts_with("index.db.corrupt-")
                    && !name.ends_with("-wal")
                    && !name.ends_with("-shm")
            })
            .collect();
        assert_eq!(aside.len(), 1, "corrupt file kept");
        assert_eq!(fs::read(&aside[0]).unwrap(), garbage, "never deleted");
        drop(index);
        assert_eq!(Index::open(&path).unwrap().stats().unwrap().checksums, 1);
    }

    #[test]
    fn quarantine_moves_companions_together() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        fs::write(&path, b"db").unwrap();
        fs::write(dir.path().join("index.db-wal"), b"wal").unwrap();
        let aside = quarantine(&path).unwrap();
        let name = aside.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("index.db.corrupt-"));
        assert_eq!(fs::read(&aside).unwrap(), b"db");
        assert_eq!(
            fs::read(dir.path().join(format!("{name}-wal"))).unwrap(),
            b"wal"
        );
        assert!(!path.exists() && !dir.path().join("index.db-wal").exists());
    }

    #[test]
    fn index_and_cache_are_shareable_across_threads() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<Index>();
        send_sync::<CachedChecksums<'static>>();
    }

    #[test]
    fn normalize_key_trims_separators_but_keeps_roots() {
        assert_eq!(normalize_key("/"), "/");
        assert_eq!(normalize_key("//"), "/");
        assert_eq!(
            normalize_key("C:\\"),
            if cfg!(windows) { "c:\\" } else { "C:\\" }
        );
        if cfg!(windows) {
            assert_eq!(normalize_key("C:/Data/Sub/"), "c:\\data\\sub");
        } else {
            assert_eq!(normalize_key("/Users/A/dir/"), "/Users/A/dir");
            assert_eq!(normalize_key("/a/b//"), "/a/b");
        }
    }
}
