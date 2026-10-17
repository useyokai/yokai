//! Instance creation, persistent lock ownership, and format-1 identity.
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};
use yokai_platform::{InstanceLock, random_instance_id};
use yokai_types::InstanceId;

use crate::preflight;
use crate::{create_file, foreign_key_violations, init_connection};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IncompleteReason { NoLockFile, NoIdentityFile, BadIdentityFile, NoDatabase }

#[cfg(windows)]
const ERROR_SHARING_VIOLATION: i32 = 32;
#[cfg(windows)]
const ERROR_LOCK_VIOLATION: i32 = 33;

#[derive(Debug)]
#[non_exhaustive]
pub enum InstanceError {
    ParentInvalid(String),
    PathInvalid(String),
    UnsupportedLocation(String),
    AlreadyExists(String),
    Busy(String),
    Incomplete(IncompleteReason),
    UnsupportedFormat(u32),
    IdentityMismatch,
    HeaderInvalid(String),
    UnexpectedJournal,
    SideFileStateUnverified { header_version: u32 },
    SchemaTooNew { found: u32, maximum: u32 },
    SchemaInconsistent(String),
    Integrity(String),
    ForeignKeyViolations(Vec<String>),
    PreflightUnavailable(String),
    PreflightWrote(String),
    Store(String),
    Identity(String),
    Io(String),
    Injected(String),
    StageB { cause: String, observed_effects: String },
    CloseIncomplete,
}

impl fmt::Display for IncompleteReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{:?}", self) }
}
impl fmt::Display for InstanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ParentInvalid(e) => write!(f, "invalid Instance parent: {e}"),
            Self::PathInvalid(e) => write!(f, "invalid Instance path: {e}"),
            Self::UnsupportedLocation(e) => write!(f, "unsupported Instance location: {e}"),
            Self::AlreadyExists(p) => write!(f, "Instance already exists: {p}"),
            Self::Busy(p) => write!(f, "Instance is already locked: {p}"),
            Self::Incomplete(reason) => write!(f, "incomplete Instance: {reason}"),
            Self::UnsupportedFormat(v) => write!(f, "unsupported Instance format {v}"),
            Self::IdentityMismatch => f.write_str("Instance identity does not match its directory"),
            Self::HeaderInvalid(field) => write!(f, "invalid SQLite header field: {field}"),
            Self::UnexpectedJournal => f.write_str("rollback journal is not supported"),
            Self::SideFileStateUnverified { header_version } => write!(f, "SHM exists without WAL; side-file state is unverified (raw header user_version={header_version})"),
            Self::SchemaTooNew { found, maximum } => write!(f, "schema {found} is newer than supported maximum {maximum}"),
            Self::SchemaInconsistent(e) => write!(f, "inconsistent schema: {e}"),
            Self::Integrity(e) => write!(f, "integrity check failed: {e}"),
            Self::ForeignKeyViolations(e) => write!(f, "foreign-key violations: {e:?}"),
            Self::PreflightUnavailable(e) => write!(f, "read-only preflight unavailable: {e}"),
            Self::PreflightWrote(e) => write!(f, "preflight changed a protected path: {e}"),
            Self::Store(e) => write!(f, "store initialization failed: {e}"),
            Self::Identity(e) => write!(f, "identity file error: {e}"),
            Self::Io(e) => f.write_str(e),
            Self::Injected(e) => write!(f, "test failpoint: {e}"),
            Self::StageB { cause, observed_effects } => write!(f, "Stage B refused: {cause}; observed_effects={observed_effects}"),
            Self::CloseIncomplete => f.write_str("Instance close did not complete; ownership remains held"),
        }
    }
}
impl std::error::Error for InstanceError {}

fn is_lock_contention(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::WouldBlock {
        return true;
    }
    #[cfg(windows)]
    if matches!(error.raw_os_error(), Some(ERROR_SHARING_VIOLATION) | Some(ERROR_LOCK_VIOLATION)) {
        return true;
    }
    false
}

fn path_error(error: io::Error, parent: bool) -> InstanceError {
    if error.kind() == io::ErrorKind::Unsupported {
        InstanceError::UnsupportedLocation(error.to_string())
    } else if parent {
        InstanceError::ParentInvalid(error.to_string())
    } else {
        InstanceError::PathInvalid(error.to_string())
    }
}

fn chosen_identity() -> Result<InstanceId, InstanceError> {
    #[cfg(feature = "test-api")]
    if let Some(id) = crate::failpoint::identity_override().map_err(InstanceError::Injected)? {
        return Ok(id);
    }
    random_instance_id().map_err(|e| InstanceError::Io(format!("OS random source: {e}")))
}

pub fn create_instance(parent: &Path) -> Result<Instance, InstanceError> {
    let parent = yokai_platform::location::validate_parent(parent).map_err(|e| path_error(e, true))?;
    let id = chosen_identity()?;
    let root = parent.join(id.to_hex());
    create_instance_dir(&root).map_err(|e| {
        if e.kind() == io::ErrorKind::AlreadyExists { InstanceError::AlreadyExists(root.display().to_string()) }
        else { InstanceError::Io(format!("create Instance directory: {e}")) }
    })?;
    hit_failpoint("after-root")?;

    let lock_path = root.join("instance.lock");
    let lock = InstanceLock::create_and_lock(&lock_path).map_err(|e| {
        if e.kind() == io::ErrorKind::AlreadyExists { InstanceError::AlreadyExists(lock_path.display().to_string()) }
        else { InstanceError::Io(format!("create and lock: {e}")) }
    })?;
    hit_failpoint("after-lock")?;

    let db_path = root.join("instance.db");
    let connection = create_file(&db_path).map_err(|e| InstanceError::Store(e.to_string()))?;
    hit_failpoint("after-db")?;

    write_identity(&root.join("instance.id"), id)?;
    yokai_platform::location::flush_directory(&root)
        .map_err(|e| InstanceError::Io(format!("flush Instance directory: {e}")))?;
    yokai_platform::location::flush_directory(&parent)
        .map_err(|e| InstanceError::Io(format!("flush parent directory: {e}")))?;
    Ok(Instance::new(root, id, connection, lock))
}

pub fn open_instance(root: &Path) -> Result<Instance, InstanceError> {
    let root = yokai_platform::location::validate_root(root).map_err(|e| path_error(e, false))?;
    let lock_path = root.join("instance.lock");
    let lock_meta = fs::symlink_metadata(&lock_path).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound { InstanceError::Incomplete(IncompleteReason::NoLockFile) }
        else { InstanceError::PathInvalid(e.to_string()) }
    })?;
    yokai_platform::location::validate_regular_single_link(&lock_path, &lock_meta)
        .map_err(|e| InstanceError::PathInvalid(e.to_string()))?;
    let lock = InstanceLock::open_and_lock(&lock_path).map_err(|e| {
        if is_lock_contention(&e) { InstanceError::Busy(root.display().to_string()) }
        else { InstanceError::PathInvalid(e.to_string()) }
    })?;
    yokai_platform::location::validate_entries(&root).map_err(|e| InstanceError::PathInvalid(e.to_string()))?;

    let id_path = root.join("instance.id");
    let id_meta = fs::symlink_metadata(&id_path).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound { InstanceError::Incomplete(IncompleteReason::NoIdentityFile) }
        else { InstanceError::PathInvalid(e.to_string()) }
    })?;
    yokai_platform::location::validate_regular_single_link(&id_path, &id_meta)
        .map_err(|e| InstanceError::PathInvalid(e.to_string()))?;
    let (id, _created) = read_identity(&id_path)?;
    if root.file_name().and_then(|name| name.to_str()) != Some(id.to_hex().as_str()) {
        return Err(InstanceError::IdentityMismatch);
    }

    let db_path = root.join("instance.db");
    match fs::symlink_metadata(&db_path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(InstanceError::Incomplete(IncompleteReason::NoDatabase));
        }
        Err(error) => return Err(InstanceError::PathInvalid(error.to_string())),
        Ok(_) => {}
    }
    let accepted = preflight::stage_a(&root)?;
    let before = preflight::capture_inventory(&root)
        .map_err(|e| InstanceError::PreflightUnavailable(e.to_string()))?;
    let connection = open_rw_connection(&db_path);
    match connection {
        Ok(connection) => {
            if !accepted.wal_present {
                if let Err(error) = stage_b_validate(&connection) {
                    drop(connection);
                    let after = preflight::capture_inventory(&root)
                        .map_err(|e| InstanceError::PreflightUnavailable(e.to_string()))?;
                    return Err(InstanceError::StageB {
                        cause: error.to_string(),
                        observed_effects: before.describe_difference(&after),
                    });
                }
            }
            Ok(Instance::new(root, id, connection, lock))
        }
        Err(cause) => {
            let after = preflight::capture_inventory(&root)
                .map_err(|e| InstanceError::PreflightUnavailable(e.to_string()))?;
            Err(InstanceError::StageB { cause, observed_effects: before.describe_difference(&after) })
        }
    }
}

fn create_instance_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    { fs::create_dir(path) }
}

fn hit_failpoint(boundary: &str) -> Result<(), InstanceError> {
    #[cfg(feature = "test-api")]
    { return crate::failpoint::hit(boundary).map_err(InstanceError::Injected); }
    #[cfg(not(feature = "test-api"))]
    { let _ = boundary; Ok(()) }
}

fn write_identity(path: &Path, id: InstanceId) -> Result<(), InstanceError> {
    let stamp = utc_timestamp().map_err(InstanceError::Identity)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path).map_err(|e| InstanceError::Identity(e.to_string()))?;
    let bytes = format!("yokai-instance format 1\nid {}\ncreated {}\n", id.to_hex(), stamp).into_bytes();
    let split = bytes.len() / 2;
    file.write_all(&bytes[..split]).map_err(|e| InstanceError::Identity(e.to_string()))?;
    hit_failpoint("during-identity")?;
    file.write_all(&bytes[split..]).map_err(|e| InstanceError::Identity(e.to_string()))?;
    file.sync_all().map_err(|e| InstanceError::Identity(e.to_string()))
}

fn read_identity(path: &Path) -> Result<(InstanceId, String), InstanceError> {
    let metadata = fs::symlink_metadata(path).map_err(|e| InstanceError::Identity(e.to_string()))?;
    if metadata.len() > 4096 { return Err(InstanceError::Incomplete(IncompleteReason::BadIdentityFile)); }
    let bytes = fs::read(path).map_err(|e| InstanceError::Identity(e.to_string()))?;
    let text = std::str::from_utf8(&bytes).map_err(|_| InstanceError::Incomplete(IncompleteReason::BadIdentityFile))?;
    if !text.ends_with('\n') || text.contains('\r') {
        return Err(InstanceError::Incomplete(IncompleteReason::BadIdentityFile));
    }
    let lines: Vec<&str> = text.strip_suffix('\n').unwrap_or(text).split('\n').collect();
    if lines.len() != 3 {
        return Err(InstanceError::Incomplete(IncompleteReason::BadIdentityFile));
    }
    if lines[0] != "yokai-instance format 1" {
        if let Some(raw) = lines[0].strip_prefix("yokai-instance format ") {
            if let Ok(format) = raw.parse::<u32>() {
                return Err(InstanceError::UnsupportedFormat(format));
            }
        }
        return Err(InstanceError::Incomplete(IncompleteReason::BadIdentityFile));
    }
    let hex = lines[1].strip_prefix("id ").ok_or(InstanceError::Incomplete(IncompleteReason::BadIdentityFile))?;
    let stamp = lines[2].strip_prefix("created ").ok_or(InstanceError::Incomplete(IncompleteReason::BadIdentityFile))?;
    let id = parse_hex_id(hex).ok_or(InstanceError::Incomplete(IncompleteReason::BadIdentityFile))?;
    if !valid_timestamp(stamp) { return Err(InstanceError::Incomplete(IncompleteReason::BadIdentityFile)); }
    Ok((id, stamp.to_owned()))
}

fn parse_hex_id(hex: &str) -> Option<InstanceId> {
    if hex.len() != 32 { return None; }
    let mut bytes = [0u8; 16];
    for i in 0..16 {
        let pair = &hex.as_bytes()[i * 2..i * 2 + 2];
        let digit = |b: u8| match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            _ => None,
        };
        bytes[i] = (digit(pair[0])? << 4) | digit(pair[1])?;
    }
    Some(InstanceId::from_bytes(bytes))
}

fn valid_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T'
        || bytes[13] != b':' || bytes[16] != b':' || bytes[19] != b'Z' { return false; }
    if ![0,1,2,3,5,6,8,9,11,12,14,15,17,18].iter().all(|&i| bytes[i].is_ascii_digit()) { return false; }
    let number = |a: usize, b: usize| -> u32 { value[a..b].parse().unwrap_or(u32::MAX) };
    let (year, month, day) = (number(0,4), number(5,7), number(8,10));
    let (hour, minute, second) = (number(11,13), number(14,16), number(17,19));
    if !(1..=12).contains(&month) || hour > 23 || minute > 59 || second > 59 { return false; }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month { 2 if leap => 29, 2 => 28, 4 | 6 | 9 | 11 => 30, _ => 31 };
    (1..=days).contains(&day)
}

fn utc_timestamp() -> Result<String, String> {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?.as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let day_seconds = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = day_seconds / 3_600;
    let minute = (day_seconds % 3_600) / 60;
    let second = day_seconds % 60;
    if !(0..=9999).contains(&year) { return Err("timestamp year outside RFC3339 range".into()); }
    Ok(format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"))
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe/1_460 + doe/36_524 - doe/146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe/4 - yoe/100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn open_rw_connection(path: &Path) -> Result<Connection, String> {
    #[cfg(feature = "test-api")]
    crate::failpoint::record_connection("stage-b-read-write");
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let conn = Connection::open_with_flags(path, flags).map_err(|e| e.to_string())?;
    init_connection(&conn).map_err(|e| e.to_string())?;
    Ok(conn)
}

fn stage_b_validate(conn: &Connection) -> Result<(), InstanceError> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|e| InstanceError::SchemaInconsistent(e.to_string()))?;
    if version != 0 {
        return Err(InstanceError::SchemaTooNew { found: version.max(0) as u32, maximum: 0 });
    }
    let objects: i64 = conn.query_row("SELECT count(*) FROM sqlite_schema WHERE lower(substr(name, 1, 7)) <> 'sqlite_'", [], |row| row.get(0))
        .map_err(|e| InstanceError::SchemaInconsistent(e.to_string()))?;
    if objects != 0 { return Err(InstanceError::SchemaInconsistent(format!("{objects} schema objects at version 0"))); }
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(|e| InstanceError::Integrity(e.to_string()))?;
    if integrity != "ok" { return Err(InstanceError::Integrity(integrity)); }
    let fk = foreign_key_violations(conn).map_err(|e| InstanceError::ForeignKeyViolations(vec![e.to_string()]))?;
    if !fk.is_empty() { return Err(InstanceError::ForeignKeyViolations(fk)); }
    Ok(())
}

pub struct Instance {
    pub(crate) root: PathBuf,
    pub(crate) id: InstanceId,
    pub(crate) connection: Option<Connection>,
    pub(crate) lock: Option<InstanceLock>,
}

impl Instance {
    pub(crate) fn new(root: PathBuf, id: InstanceId, connection: Connection, lock: InstanceLock) -> Self {
        Self { root, id, connection: Some(connection), lock: Some(lock) }
    }
    pub fn root(&self) -> &Path { &self.root }
    pub fn id(&self) -> InstanceId { self.id }
    pub fn is_locked(&self) -> bool { self.lock.as_ref().is_some_and(InstanceLock::is_locked) }

    /// Closes the SQLite handle before releasing the persistent lock-file handle.
    pub fn close(mut self) -> Result<(), InstanceError> {
        drop(self.connection.take());
        #[cfg(feature = "test-api")]
        crate::failpoint::record_connection("connection-closed");
        drop(self.lock.take());
        #[cfg(feature = "test-api")]
        crate::failpoint::record_connection("lock-released");
        Ok(())
    }
}

#[cfg(feature = "test-api")]
pub mod test_api {
    use std::path::{Path, PathBuf};

    use super::{Instance, InstanceError};
    use crate::failpoint;

    pub fn set_next_identity(bytes: [u8; 16]) -> Result<(), String> { failpoint::set_next_identity(bytes) }
    pub fn clear() {
        failpoint::clear();
        yokai_platform::location::clear_directory_flush_observations();
    }
    pub fn connection_events() -> Vec<&'static str> { failpoint::take_connection_events() }
    pub fn directory_flush_observations() -> Vec<(PathBuf, bool)> {
        yokai_platform::location::take_directory_flush_observations()
    }
    pub fn mutate_during_stage_a_once() -> Result<(), String> {
        failpoint::arm_stage_a_mutation()
    }
    pub fn sha256_chunks_for_test(chunks: &[&[u8]]) -> String {
        crate::preflight::sha256_chunks_for_test(chunks)
    }
    pub fn allow_tmpfs_for_test(path: &Path) -> std::io::Result<()> {
        yokai_platform::location::allow_tmpfs_for_test(path)
    }
    pub fn user_version(instance: &Instance) -> Result<i64, InstanceError> {
        instance.connection.as_ref().ok_or(InstanceError::CloseIncomplete)?
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|e| InstanceError::Store(e.to_string()))
    }
    pub fn schema_objects(instance: &Instance) -> Result<i64, InstanceError> {
        instance.connection.as_ref().ok_or(InstanceError::CloseIncomplete)?
            .query_row("SELECT count(*) FROM sqlite_schema WHERE lower(substr(name, 1, 7)) <> 'sqlite_'", [], |row| row.get(0))
            .map_err(|e| InstanceError::Store(e.to_string()))
    }
}
