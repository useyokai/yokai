//! Yokai's local storage: one SQLite database file per Instance.
//!
//! This version provides the connection initializer and the runtime checks that later storage
//! components rely on. Application code opens connections only through [`create_file`] or
//! [`open_file`]. Both refuse anything but an absolute path to a database file, refuse a SQLite
//! older than [`MIN_SQLITE_VERSION`], and apply and then read back `journal_mode = WAL`,
//! `synchronous = FULL`, `foreign_keys = ON` and the busy timeout.
#![forbid(unsafe_code)]

use std::fmt;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

/// Oldest SQLite release accepted at run time, as (major, minor, patch).
pub const MIN_SQLITE_VERSION: (u32, u32, u32) = (3, 53, 0);
/// Busy timeout set on every connection, in milliseconds.
pub const BUSY_TIMEOUT_MS: u64 = 5000;

#[derive(Debug)]
#[non_exhaustive]
pub enum StoreError {
    Sqlite(rusqlite::Error),
    NotAbsolutePath(String),
    AlreadyExists(String),
    NotFileBacked,
    InTransaction,
    Pragma { name: &'static str, expected: String, actual: String },
    SqliteVersion(String),
    ForeignKeyViolations(Vec<String>),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Sqlite(e) => write!(f, "sqlite: {e} (extended code {:?})", ext_code(e)),
            StoreError::NotAbsolutePath(p) => write!(f, "database path must be absolute: {p}"),
            StoreError::AlreadyExists(p) => write!(f, "refusing to create over an existing path: {p}"),
            StoreError::NotFileBacked => write!(f, "the main database is not backed by a file"),
            StoreError::InTransaction => write!(f, "connection initializer called inside a transaction"),
            StoreError::Pragma { name, expected, actual } => write!(f, "pragma {name}: expected {expected}, got {actual}"),
            StoreError::SqliteVersion(v) => write!(f, "SQLite {v} is older than the minimum supported release"),
            StoreError::ForeignKeyViolations(v) => write!(f, "foreign_key_check found {} violation(s): {v:?}", v.len()),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Sqlite(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sqlite(e)
    }
}

/// Extended SQLite result code of an error, if it is a SQLite failure.
pub fn ext_code(e: &rusqlite::Error) -> Option<i32> {
    e.sqlite_error().map(|s| s.extended_code)
}

/// Settings read back from a connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionSettings {
    pub journal_mode: String,
    pub synchronous: i64,
    pub foreign_keys: i64,
    pub busy_timeout: i64,
}

/// Version and source identifier of the SQLite library linked into this process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqliteRuntime {
    pub version: String,
    pub source_id: String,
}

fn rw_flags() -> OpenFlags {
    OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX
}

fn require_absolute(path: &Path) -> Result<(), StoreError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(StoreError::NotAbsolutePath(path.display().to_string()))
    }
}

/// Creates a new database file. Refuses any existing directory entry at `path`, including a
/// dangling symbolic link.
pub fn create_file(path: &Path) -> Result<Connection, StoreError> {
    require_absolute(path)?;
    if std::fs::symlink_metadata(path).is_ok() {
        return Err(StoreError::AlreadyExists(path.display().to_string()));
    }
    let conn = Connection::open_with_flags(path, rw_flags() | OpenFlags::SQLITE_OPEN_CREATE)?;
    prepare(&conn)?;
    Ok(conn)
}

/// Opens an existing database file, and refuses it if `foreign_key_check` reports a violation.
pub fn open_file(path: &Path) -> Result<Connection, StoreError> {
    require_absolute(path)?;
    let conn = Connection::open_with_flags(path, rw_flags())?;
    prepare(&conn)?;
    let violations = foreign_key_violations(&conn)?;
    if !violations.is_empty() {
        return Err(StoreError::ForeignKeyViolations(violations));
    }
    Ok(conn)
}

fn prepare(conn: &Connection) -> Result<(), StoreError> {
    require_min_version(&sqlite_runtime(conn)?)?;
    init_connection(conn)?;
    if main_database_file(conn)?.is_empty() {
        return Err(StoreError::NotFileBacked);
    }
    Ok(())
}

/// The only way a connection is configured. It must run outside any transaction
/// (`PRAGMA foreign_keys` is a no-op inside one), and it verifies every setting by reading it
/// back.
pub fn init_connection(conn: &Connection) -> Result<ConnectionSettings, StoreError> {
    if !conn.is_autocommit() {
        return Err(StoreError::InTransaction);
    }
    let _: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    conn.execute_batch("PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;")?;
    conn.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))?;
    let settings = read_settings(conn)?;
    let checks: [(&'static str, String, String); 4] = [
        ("journal_mode", "wal".into(), settings.journal_mode.to_ascii_lowercase()),
        ("synchronous", "2".into(), settings.synchronous.to_string()),
        ("foreign_keys", "1".into(), settings.foreign_keys.to_string()),
        ("busy_timeout", BUSY_TIMEOUT_MS.to_string(), settings.busy_timeout.to_string()),
    ];
    for (name, expected, actual) in checks {
        if expected != actual {
            return Err(StoreError::Pragma { name, expected, actual });
        }
    }
    Ok(settings)
}

pub fn read_settings(conn: &Connection) -> rusqlite::Result<ConnectionSettings> {
    Ok(ConnectionSettings {
        journal_mode: conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?,
        synchronous: conn.query_row("PRAGMA synchronous", [], |r| r.get(0))?,
        foreign_keys: conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?,
        busy_timeout: conn.query_row("PRAGMA busy_timeout", [], |r| r.get(0))?,
    })
}

/// Path of the file behind the `main` database; empty for an in-memory or temporary database.
pub fn main_database_file(conn: &Connection) -> rusqlite::Result<String> {
    let file: Option<String> =
        conn.query_row("SELECT file FROM pragma_database_list WHERE name = 'main'", [], |r| r.get(0))?;
    Ok(file.unwrap_or_default())
}

pub fn sqlite_runtime(conn: &Connection) -> rusqlite::Result<SqliteRuntime> {
    Ok(SqliteRuntime {
        version: conn.query_row("SELECT sqlite_version()", [], |r| r.get(0))?,
        source_id: conn.query_row("SELECT sqlite_source_id()", [], |r| r.get(0))?,
    })
}

/// Parses "major.minor.patch".
pub fn parse_version(version: &str) -> Option<(u32, u32, u32)> {
    let mut parts = version.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    let patch: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

pub fn require_min_version(runtime: &SqliteRuntime) -> Result<(), StoreError> {
    match parse_version(&runtime.version) {
        Some(v) if v >= MIN_SQLITE_VERSION => Ok(()),
        _ => Err(StoreError::SqliteVersion(runtime.version.clone())),
    }
}

/// Checks that the SQL function `load_extension()` is refused on this connection.
/// Returns the refusal message, or a description of the unexpected success.
pub fn load_extension_refused(conn: &Connection) -> Result<String, String> {
    match conn.query_row("SELECT load_extension('/nonexistent/yokai-extension')", [], |r| {
        r.get::<_, Option<String>>(0)
    }) {
        Err(e) => Ok(e.to_string()),
        Ok(v) => Err(format!("load_extension() returned {v:?}")),
    }
}

/// Rows reported by `PRAGMA foreign_key_check`, one string per violation.
pub fn foreign_key_violations(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("PRAGMA foreign_key_check")?;
    let rows = stmt.query_map([], |r| {
        let table: String = r.get(0)?;
        let rowid: Option<i64> = r.get(1)?;
        let parent: String = r.get(2)?;
        let fkid: i64 = r.get(3)?;
        Ok(format!("{table} rowid={rowid:?} parent={parent} fkid={fkid}"))
    })?;
    rows.collect()
}
