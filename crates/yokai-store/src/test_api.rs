//! Diagnostic access; excluded from default builds.

use std::path::{Path, PathBuf};

use crate::{Instance, InstanceError};
use crate::failpoint;
static SPAWN_FAILURE: std::sync::Mutex<Option<usize>> = std::sync::Mutex::new(None);
static STARTUP_CLOSE_BUSY: std::sync::atomic::AtomicBool=std::sync::atomic::AtomicBool::new(false);
/// Use only in an owned child: the next startup-error close deliberately retains
/// a real unfinalized statement, and the child must be terminated after inspection.
pub fn fail_startup_close_once(){STARTUP_CLOSE_BUSY.store(true,std::sync::atomic::Ordering::Release);}
pub(crate) fn poison_startup_close_if_requested(connection:&rusqlite::Connection){
    if STARTUP_CLOSE_BUSY.swap(false,std::sync::atomic::Ordering::AcqRel){
        // sqlite3_close (not close_v2) returns BUSY for this leaked real statement.
        // The existing error path must retain both connection and Instance lock.
        if let Ok(statement)=connection.prepare("SELECT 1"){std::mem::forget(statement);}
    }
}
pub fn fail_worker_spawn_after(successful_spawns: usize) {
    *crate::runtime::lock(&SPAWN_FAILURE) = Some(successful_spawns);
}
pub(crate) fn fail_worker_spawn() -> bool {
    let mut countdown = crate::runtime::lock(&SPAWN_FAILURE);
    match *countdown {
        Some(0) => { *countdown = None; true }
        Some(n) => { *countdown = Some(n - 1); false }
        None => false,
    }
}
pub fn pause_health_after_commit(instance: &Instance, deadline: std::time::Instant) -> Result<(Pause, crate::PendingWrite), String> {
    let pause = Pause::new(); let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let pending = instance.store().paused_health(deadline, tx, pause.released.clone()).map_err(|e| e.to_string())?;
    rx.recv_timeout(std::time::Duration::from_secs(5)).map_err(|e| e.to_string())?;
    Ok((pause, pending))
}
pub struct Pause { pub(crate) released: std::sync::Arc<std::sync::atomic::AtomicBool> }
impl Pause {
    pub(crate) fn new() -> Self { Self { released: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)) } }
}
impl Drop for Pause { fn drop(&mut self) { self.released.store(true, std::sync::atomic::Ordering::Release); } }
pub fn block_writer(instance: &Instance) -> Result<Pause, InstanceError> { instance.runtime.block_writer() }
pub fn read_probe(instance: &Instance, deadline: std::time::Instant, steps: u32, rows: u32, gap: std::time::Duration) -> Result<crate::PendingRead, crate::ReadError> {
    instance.store().read_probe(deadline, steps, rows, gap)
}
#[derive(Clone, Debug)]
pub struct ReaderEvent {
    pub query_id: u64,
    pub action: &'static str,
    pub deadline: std::time::Instant,
    pub observed: std::time::Instant,
    pub shutdown: bool,
}
pub fn reader_events(instance: &Instance) -> Vec<ReaderEvent> { instance.runtime.reader_events() }
pub fn set_next_request_id(instance: &Instance, id: u64) { instance.runtime.set_next_request_id(id); }
pub fn hold_reader(instance: &Instance, uninterruptible: bool) -> Result<(Pause, crate::PendingRead), crate::ReadError> {
    instance.store().hold_reader(uninterruptible)
}
/// Fault injection returns synthetic SQLite errors; it does not simulate physical media failure.
#[derive(Clone, Debug)]
pub enum WriteFault {
    Refuse { rollback_fails: bool },
    None, Begin(i32), Statement(i32), RollbackFailure(i32), Commit { code: i32, actually_commit: bool },
    PanicQueued, PanicBegun, PanicAfterCommit, DelayBeforeCommit(std::time::Duration),
    HoldBeforeCommit { ready: std::sync::mpsc::SyncSender<()>, released: std::sync::Arc<std::sync::atomic::AtomicBool> },
}
pub fn read_error_probe(instance: &Instance, deadline: std::time::Instant, code: i32) -> Result<crate::PendingRead, crate::ReadError> {
    instance.store().read_error_probe(deadline, code)
}
#[derive(Clone, Debug)]
pub struct WriteProbe { pub sql: String, pub fault: WriteFault }
pub fn accept_command_fault(instance:&Instance,request:crate::CommandRequest,deadline:std::time::Instant,fault:WriteFault)->Result<crate::PendingCommand,crate::WriteFailure>{
    instance.store().command_fault(request,deadline,fault)
}
pub fn write_probe(instance: &Instance, deadline: std::time::Instant, probe: WriteProbe) -> Result<crate::PendingWrite, crate::WriteFailure> {
    instance.store().write_probe(deadline, probe)
}
pub fn writer_sql(instance: &Instance, sql: &str) -> Result<(), InstanceError> {
    let sql = sql.to_owned(); instance.runtime.control(move |c| c.execute_batch(&sql).map_err(|e| e.to_string()))
}
pub fn hold_write(instance: &Instance, deadline: std::time::Instant, sql: &str) -> Result<(Pause, crate::PendingWrite), String> {
    let pause = Pause::new(); let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let pending = write_probe(instance, deadline, WriteProbe { sql: sql.into(), fault: WriteFault::HoldBeforeCommit { ready: tx, released: pause.released.clone() } }).map_err(|e| e.to_string())?;
    rx.recv_timeout(std::time::Duration::from_secs(5)).map_err(|e| e.to_string())?;
    Ok((pause, pending))
}
pub fn worker_threads(instance: &Instance) -> Vec<(String, String)> { instance.runtime.worker_threads() }
pub fn open_test_versions(root:&Path,latest:u32,fail:bool)->Result<Instance,InstanceError>{crate::instance::open_with_plan(root,crate::schema::test_plan(latest,fail)?)}
pub fn lint_migration(sql:&str,version:u32)->Result<(),String>{crate::schema::lint(sql,version)}
pub fn backup_warning_for_test(total:u64,threshold:u64)->bool{crate::snapshot::policy(total,threshold).warning}

pub fn set_next_identity(bytes: [u8; 16]) -> Result<(), String> { failpoint::set_next_identity(bytes) }
pub fn clear() {
    *crate::runtime::lock(&SPAWN_FAILURE) = None;
    failpoint::clear();
    yokai_platform::location::clear_directory_flush_observations();
}
pub fn connection_events() -> Vec<&'static str> { failpoint::take_connection_events() }
/// Take and clear versions decoded from validated nonempty raw headers.
pub fn header_versions() -> Result<Vec<u32>,String> { failpoint::take_header_versions() }
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
    instance.runtime.control(|c| c.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(|e| e.to_string()))
}
pub fn no_checkpoint_on_close(instance: &Instance) -> Result<(), InstanceError> { instance.runtime.no_checkpoint_on_close() }
pub fn hardened_settings(instance: &Instance) -> Result<(i64, bool, i64, i64), InstanceError> {
    instance.runtime.control(|c| {
        let read = || -> rusqlite::Result<_> { Ok((c.query_row("PRAGMA trusted_schema", [], |r| r.get(0))?,
            c.db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE)?,
            c.query_row("PRAGMA auto_vacuum", [], |r| r.get(0))?, c.query_row("PRAGMA wal_autocheckpoint", [], |r| r.get(0))?)) };
        read().map_err(|e| e.to_string())
    })
}
pub fn schema_objects(instance: &Instance) -> Result<i64, InstanceError> {
    instance.runtime.control(|c| c.query_row("SELECT count(*) FROM sqlite_schema WHERE lower(substr(name, 1, 7)) <> 'sqlite_'", [], |r| r.get(0)).map_err(|e| e.to_string()))
}
/// Low-level access exists only for the explicit diagnostic feature.
pub fn create_file(path: &Path) -> Result<rusqlite::Connection, crate::StoreError> { crate::connection::create_file(path) }
pub fn open_file(path: &Path) -> Result<rusqlite::Connection, crate::StoreError> { crate::connection::open_file(path) }
pub fn init_connection(c: &rusqlite::Connection) -> Result<crate::ConnectionSettings, crate::StoreError> { crate::connection::init_connection(c) }
pub fn read_settings(c: &rusqlite::Connection) -> rusqlite::Result<crate::ConnectionSettings> { crate::connection::read_settings(c) }
pub fn main_database_file(c: &rusqlite::Connection) -> rusqlite::Result<String> { crate::connection::main_database_file(c) }
pub fn sqlite_runtime(c: &rusqlite::Connection) -> rusqlite::Result<crate::SqliteRuntime> { crate::connection::sqlite_runtime(c) }
pub fn foreign_key_violations(c: &rusqlite::Connection) -> rusqlite::Result<Vec<String>> { crate::connection::foreign_key_violations(c) }
pub fn load_extension_refused(c: &rusqlite::Connection) -> Result<String, String> { crate::connection::load_extension_refused(c) }
