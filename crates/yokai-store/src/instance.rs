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
use crate::{create_file, init_connection};

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
    MigrationDiverged(String),
    Migration(String),
    Snapshot(String),
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
            Self::MigrationDiverged(e) => write!(f, "migration history diverged: {e}"),
            Self::Migration(e) => write!(f, "migration failed: {e}"),
            Self::Snapshot(e) => write!(f, "recovery snapshot unavailable: {e}"),
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
    create_instance_with_options(parent, crate::StoreOptions::default())
}
pub fn create_instance_with_options(parent: &Path, options: crate::StoreOptions) -> Result<Instance, InstanceError> {
    options.validate()?;
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
    let mut connection = create_file(&db_path).map_err(|e| InstanceError::Store(e.to_string()))?;
    hit_failpoint("after-db")?;

    let created = utc_timestamp().map_err(InstanceError::Identity)?;
    crate::schema::migrate_new(&mut connection,id,&created)?;
    hit_failpoint("after-migration")?;
    write_identity(&root.join("instance.id"), id, &created)?;
    yokai_platform::location::flush_directory(&root)
        .map_err(|e| InstanceError::Io(format!("flush Instance directory: {e}")))?;
    yokai_platform::location::flush_directory(&parent)
        .map_err(|e| InstanceError::Io(format!("flush parent directory: {e}")))?;
    Instance::new(root, id, connection, lock, options, Some(crate::StartupReport::created()))
}

pub fn open_instance(root: &Path) -> Result<Instance, InstanceError> {
    open_instance_with_options(root, crate::StoreOptions::default())
}
pub fn open_instance_with_options(root: &Path, options: crate::StoreOptions) -> Result<Instance, InstanceError> {
    open_with_options_plan(root, crate::schema::production(), options)
}
#[cfg(feature = "test-api")]
pub(crate) fn open_with_plan(root: &Path, plan: crate::schema::Plan) -> Result<Instance, InstanceError> {
    open_with_options_plan(root, plan, crate::StoreOptions::default())
}
fn open_with_options_plan(root: &Path, plan: crate::schema::Plan, options: crate::StoreOptions) -> Result<Instance, InstanceError> {
    options.validate()?;
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
    let (id, created) = read_identity(&id_path)?;
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
    let accepted = preflight::stage_a(&root, id, &created,plan)?;
    let before = preflight::capture_inventory(&root)
        .map_err(|e| InstanceError::PreflightUnavailable(e.to_string()))?;
    let connection = open_rw_connection(&db_path);
    match connection {
        Ok(mut connection) => {
            if !accepted.wal_present {
                if let Err(error) = crate::schema::inspect(&connection,id,&created,plan) {
                    drop(connection);
                    let after = preflight::capture_inventory(&root)
                        .map_err(|e| InstanceError::PreflightUnavailable(e.to_string()))?;
                    return Err(InstanceError::StageB {
                        cause: error.to_string(),
                        observed_effects: before.describe_difference(&after),
                    });
                }
            }
            crate::schema::upgrade(&mut connection,&root,id,&created,plan)?;
            let startup=if plan.has_durable(){
                match crate::startup::recover(&mut connection){
                    Ok(report)=>Some(report),
                    Err(error)=>{
                        match connection.close(){
                            Ok(())=>drop(lock),
                            Err((connection,close_error))=>{
                                std::mem::forget(connection);std::mem::forget(lock);
                                return Err(InstanceError::Store(format!("{error}; SQLite close unconfirmed: {close_error}; ownership retained")));
                            }
                        }
                        return Err(error);
                    }
                }
            }else{None};
            Instance::new(root, id, connection, lock, options, startup)
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

pub(crate) fn hit_failpoint(boundary: &str) -> Result<(), InstanceError> {
    #[cfg(feature = "test-api")]
    { return crate::failpoint::hit(boundary).map_err(InstanceError::Injected); }
    #[cfg(not(feature = "test-api"))]
    { let _ = boundary; Ok(()) }
}

fn write_identity(path: &Path, id: InstanceId, stamp: &str) -> Result<(), InstanceError> {
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

pub(crate) fn valid_timestamp(value: &str) -> bool {
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

pub(crate) fn utc_timestamp() -> Result<String, String> {
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

pub struct Instance {
    pub(crate) root: PathBuf,
    pub(crate) id: InstanceId,
    pub(crate) runtime: crate::runtime::Runtime,
    startup: Option<crate::StartupReport>,
}

impl Instance {
    pub(crate) fn new(root: PathBuf, id: InstanceId, connection: Connection, lock: InstanceLock, options: crate::StoreOptions, startup:Option<crate::StartupReport>) -> Result<Self, InstanceError> {
        let runtime = crate::runtime::Runtime::start(connection, &root, id, lock, options)?;
        Ok(Self { root, id, runtime, startup })
    }
    pub fn store(&self) -> crate::StoreHandle { self.runtime.handle() }
    /// Creation initializes epoch1; later successful opens complete startup before returning.
    /// None is reserved for diagnostic plans deliberately stopping at V1.
    pub fn startup_report(&self)->Option<&crate::StartupReport>{self.startup.as_ref()}
    pub fn root(&self) -> &Path { &self.root }
    pub fn id(&self) -> InstanceId { self.id }
    pub fn is_locked(&self) -> bool { self.runtime.is_locked() }
    /// All retained backup bytes, including partial attempts. No automatic deletion occurs.
    pub fn backup_usage(&self) -> Result<crate::BackupUsage, InstanceError> {
        crate::snapshot::usage(&self.root)
    }

    /// Rejects new/queued work, joins workers and closes all connections before releasing ownership.
    /// A close deadline failure deliberately retains the lock until process exit.
    pub fn close(mut self) -> Result<(), InstanceError> { self.runtime.close() }
}
