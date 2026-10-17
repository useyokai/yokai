//! Connections move into workers only after synchronous compatibility/migration completes.
mod types;
mod writer;
mod reader;
mod failure;
pub use types::*;
use std::collections::VecDeque;

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use rusqlite::{Connection, OpenFlags};
use rusqlite::config::DbConfig;
use yokai_platform::InstanceLock;
use crate::InstanceError;

pub(crate) fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|poison| poison.into_inner())
}

struct State {
    status: StoreState,
    degradation_generation: u64,
    next_id: Option<u64>,
    next_reader: usize,
    completed: VecDeque<Completion>,
    metrics: StoreMetrics,
}
pub(crate) struct Shared {
    instance_id: yokai_types::InstanceId,
    state: Mutex<State>,
    pub closing: AtomicBool,
    readers_alive: AtomicUsize,
    close_failed: Arc<AtomicBool>,
    workers: Mutex<Vec<(String, String)>>,
    #[cfg(feature = "test-api")]
    skip_checkpoint: AtomicBool,
    #[cfg(feature = "test-api")]
    read_events: Mutex<VecDeque<crate::test_api::ReaderEvent>>,
}
impl Shared {
    fn status(&self) -> StoreState { lock(&self.state).status }
    fn change(&self, status: StoreState) {
        let mut state = lock(&self.state);
        if matches!(state.status, StoreState::ShuttingDown | StoreState::Closed | StoreState::Failed) { return; }
        if status == StoreState::Degraded {
            // Every fault advances the generation, even if already Degraded. Never wrap
            // and let a stale successful health transaction match a later degradation.
            let Some(next) = state.degradation_generation.checked_add(1) else { state.status = StoreState::Failed; return; };
            state.degradation_generation = next;
        }
        state.status = status;
    }
    fn complete(&self, id: u64, result: WriteResult) {
        use std::io::Write;
        let record = Completion { operation_id: id, phase: Phase::Done, result };
        let _ = writeln!(std::io::stderr().lock(), "INFO store_write_completion instance_id={} operation_id={id} result={:?}", self.instance_id, record.result);
        let mut state = lock(&self.state);
        state.metrics.completed_writes = state.metrics.completed_writes.saturating_add(1);
        if state.completed.len() == 256 { state.completed.pop_front(); }
        state.completed.push_back(record);
    }
}

struct WriteRequest {
    id: u64,
    deadline: Instant,
    progress: Arc<Mutex<WriteEvidence>>,
    reply: mpsc::SyncSender<WriteResult>,
    work: WriteWork,
}
enum WriteWork {
    Health,
    Accept{command:crate::CommandRequest,#[cfg(feature="test-api")] fault:crate::test_api::WriteFault},
    #[cfg(feature = "test-api")]
    HealthPaused { ready: mpsc::SyncSender<()>, released: Arc<AtomicBool> },
    #[cfg(feature = "test-api")]
    Probe(crate::test_api::WriteProbe),
}
impl WriteWork {
    #[cfg(feature="test-api")]
    fn fault(&self)->Option<&crate::test_api::WriteFault>{match self{
        Self::Probe(probe)=>Some(&probe.fault),Self::Accept{fault,..}=>Some(fault),_=>None,
    }}
    fn is_health(&self) -> bool {
        match self {
            Self::Health => true,
            Self::Accept{..} => false,
            #[cfg(feature = "test-api")]
            Self::HealthPaused { .. } => true,
            #[cfg(feature = "test-api")]
            Self::Probe(_) => false,
        }
    }
}
enum OperationError {
    Sqlite(rusqlite::Error),
    // The health-only application API has no domain refusals. Test typed-operation fixtures
    // exercise this evidence rule before application domain operations are introduced.
    #[cfg(feature = "test-api")]
    Refused,
}
impl From<rusqlite::Error> for OperationError {
    fn from(error: rusqlite::Error) -> Self { Self::Sqlite(error) }
}
struct ReadRequest {
    id: u64,
    deadline: Instant,
    work: ReadWork,
    reply: mpsc::SyncSender<Result<InstanceMetadata, ReadError>>,
}
enum ReadWork {
    Metadata,
    #[cfg(feature = "test-api")]
    Compute { steps: u32, rows: u32, gap: Duration },
    #[cfg(feature = "test-api")]
    Hold { ready: mpsc::SyncSender<()>, released: Arc<AtomicBool>, uninterruptible: bool },
    #[cfg(feature = "test-api")]
    Error(i32),
}

enum WriterMessage {
    Health(WriteRequest),
    #[cfg(feature = "test-api")]
    Control(Box<dyn FnOnce(&Connection) + Send>),
}

/// Clones carry channels and state; no Connection or Transaction can escape.
#[derive(Clone)]
pub struct StoreHandle {
    shared: Arc<Shared>,
    writer: mpsc::SyncSender<WriterMessage>,
    readers: Vec<mpsc::SyncSender<ReadRequest>>,
}

fn write_refusal(status: StoreState) -> Option<WriteReason> {
    match status {
        StoreState::Running | StoreState::Degraded => None, // health maintenance is allowed when degraded
        StoreState::Failed => Some(WriteReason::Failed),
        StoreState::ShuttingDown | StoreState::Closed => Some(WriteReason::ShuttingDown),
    }
}
impl StoreHandle {
    pub fn accept_command(&self,request:crate::CommandRequest,deadline:Instant)->Result<crate::PendingCommand,WriteFailure>{
        self.submit_write(deadline,WriteWork::Accept{command:request,#[cfg(feature="test-api")] fault:crate::test_api::WriteFault::None}).map(|inner|crate::PendingCommand{inner})
    }
    #[cfg(feature="test-api")]
    pub(crate) fn command_fault(&self,command:crate::CommandRequest,deadline:Instant,fault:crate::test_api::WriteFault)->Result<crate::PendingCommand,WriteFailure>{
        self.submit_write(deadline,WriteWork::Accept{command,fault}).map(|inner|crate::PendingCommand{inner})
    }
    pub fn state(&self) -> StoreState { self.shared.status() }
    pub fn metrics(&self) -> StoreMetrics { lock(&self.shared.state).metrics }
    /// Recent true outcomes, bounded to256. Not a durable operation ledger.
    pub fn completions(&self) -> Vec<Completion> { lock(&self.shared.state).completed.iter().cloned().collect() }
    pub fn submit_health_check(&self, deadline: Instant) -> Result<PendingWrite, WriteFailure> {
        self.submit_write(deadline, WriteWork::Health)
    }
    #[cfg(feature = "test-api")]
    pub(crate) fn paused_health(&self, deadline: Instant, ready: mpsc::SyncSender<()>, released: Arc<AtomicBool>) -> Result<PendingWrite, WriteFailure> {
        self.submit_write(deadline, WriteWork::HealthPaused { ready, released })
    }
    #[cfg(feature = "test-api")]
    pub(crate) fn write_probe(&self, deadline: Instant, probe: crate::test_api::WriteProbe) -> Result<PendingWrite, WriteFailure> {
        self.submit_write(deadline, WriteWork::Probe(probe))
    }
    fn submit_write(&self, deadline: Instant, work: WriteWork) -> Result<PendingWrite, WriteFailure> {
        // Admission and close share this mutex. try_send never waits for capacity.
        let mut state = lock(&self.shared.state);
        if let Some(reason) = write_refusal(state.status) { return Err(WriteFailure::queued(None, reason)); }
        if state.status == StoreState::Degraded && !work.is_health() {
            return Err(WriteFailure::queued(None, WriteReason::Degraded));
        }
        let id = state.next_id.ok_or_else(|| WriteFailure::queued(None, WriteReason::Invalid))?;
        state.next_id = id.checked_add(1);
        let (tx, rx) = mpsc::sync_channel(1);
        let progress = Arc::new(Mutex::new(WriteEvidence::queued()));
        self.writer.try_send(WriterMessage::Health(WriteRequest { id, deadline, progress: progress.clone(), reply: tx, work }))
            .map_err(|e| WriteFailure::queued(None, match e {
                mpsc::TrySendError::Full(_) => WriteReason::Overloaded,
                mpsc::TrySendError::Disconnected(_) => { state.status = StoreState::Failed; WriteReason::Failed }
            }))?;
        Ok(PendingWrite { id, reply: rx, progress })
    }
    pub fn submit_metadata(&self, deadline: Instant) -> Result<PendingRead, ReadError> {
        self.submit_read(deadline, ReadWork::Metadata)
    }
    #[cfg(feature = "test-api")]
    pub(crate) fn read_probe(&self, deadline: Instant, steps: u32, rows: u32, gap: Duration) -> Result<PendingRead, ReadError> {
        self.submit_read(deadline, ReadWork::Compute { steps, rows, gap })
    }
    #[cfg(feature = "test-api")]
    pub(crate) fn read_error_probe(&self, deadline: Instant, code: i32) -> Result<PendingRead, ReadError> {
        self.submit_read(deadline, ReadWork::Error(code))
    }
    #[cfg(feature = "test-api")]
    pub(crate) fn hold_reader(&self, uninterruptible: bool) -> Result<(crate::test_api::Pause, PendingRead), ReadError> {
        let pause = crate::test_api::Pause::new();
        let (tx, rx) = mpsc::sync_channel(1);
        let pending = self.submit_read(Instant::now() + Duration::from_secs(30), ReadWork::Hold {
            ready: tx, released: pause.released.clone(), uninterruptible,
        })?;
        rx.recv_timeout(Duration::from_secs(2)).map_err(|_| ReadError::Failed)?;
        Ok((pause, pending))
    }
    fn submit_read(&self, deadline: Instant, work: ReadWork) -> Result<PendingRead, ReadError> {
        let mut state = lock(&self.shared.state);
        match state.status {
            StoreState::Failed => return Err(ReadError::Failed),
            StoreState::ShuttingDown | StoreState::Closed => return Err(ReadError::ShuttingDown),
            _ => {},
        }
        let id = state.next_id.ok_or(ReadError::Invalid)?;
        state.next_id = id.checked_add(1);
        let (tx, rx) = mpsc::sync_channel(1);
        let mut request = ReadRequest { id, deadline, work, reply: tx };
        let start = state.next_reader;
        state.next_reader = (start + 1) % self.readers.len();
        for offset in 0..self.readers.len() {
            match self.readers[(start + offset) % self.readers.len()].try_send(request) {
                Ok(()) => return Ok(PendingRead { id, reply: rx }),
                Err(mpsc::TrySendError::Full(unsent)) => request = unsent,
                Err(mpsc::TrySendError::Disconnected(_)) => { state.status = StoreState::Failed; return Err(ReadError::Failed); }
            }
        }
        Err(ReadError::Overloaded)
    }
}

pub(crate) struct Runtime {
    handle: StoreHandle,
    threads: Vec<JoinHandle<()>>,
    ownership: Option<InstanceLock>,
    close_timeout: Duration,
}

fn worker(shared: &Shared, role: &str) {
    lock(&shared.workers).push((role.into(), format!("{:?}", thread::current().id())));
}

// Retain observable, explicit close for connections not transferred into a worker.
struct StartupConnection { conn: Option<Connection>, close_failed: Arc<AtomicBool> }
impl StartupConnection {
    fn new(conn: Connection, close_failed: &Arc<AtomicBool>) -> Self { Self { conn: Some(conn), close_failed: close_failed.clone() } }
    fn take(mut self) -> Connection { self.conn.take().expect("startup connection present") }
}
impl Drop for StartupConnection {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            let closed = conn.close().is_ok();
            if !closed { self.close_failed.store(true, Ordering::Release); }
            #[cfg(feature = "test-api")]
            crate::failpoint::record_connection(if closed { "startup-connection-closed" } else { "startup-close-failed" });
        }
    }
}
fn spawn_worker(run: impl FnOnce() + Send + 'static, name: &str) -> std::io::Result<JoinHandle<()>> {
    #[cfg(feature = "test-api")]
    if crate::test_api::fail_worker_spawn() {
        drop(run);
        return Err(std::io::Error::other("test-api worker spawn failure"));
    }
    thread::Builder::new().name(name.into()).spawn(run)
}

fn reader_connection(path: &Path) -> Result<Connection, InstanceError> {
    let open = || -> Result<Connection, crate::StoreError> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_NOFOLLOW)?;
        conn.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)?;
        conn.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL; PRAGMA wal_autocheckpoint=1000;")?;
        conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
        conn.busy_timeout(Duration::from_millis(crate::BUSY_TIMEOUT_MS))?;
        let settings = crate::connection::read_settings(&conn)?;
        let trusted: i64 = conn.query_row("PRAGMA trusted_schema", [], |r| r.get(0))?;
        let vacuum: i64 = conn.query_row("PRAGMA auto_vacuum", [], |r| r.get(0))?;
        let checkpoint: i64 = conn.query_row("PRAGMA wal_autocheckpoint", [], |r| r.get(0))?;
        if !conn.is_readonly("main")? || settings.journal_mode.to_lowercase() != "wal"
            || settings.synchronous != 2 || settings.foreign_keys != 1
            || settings.busy_timeout != crate::BUSY_TIMEOUT_MS as i64
            || trusted != 0 || vacuum != 0 || checkpoint != 1000
            || !conn.db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE)?
            || !conn.db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE)? {
            return Err(crate::StoreError::Pragma { name: "reader hardening", expected: "verified read-only".into(), actual: "mismatch".into() });
        }
        Ok(conn)
    };
    open().map_err(|e| InstanceError::Store(e.to_string()))
}

impl Runtime {
    pub(crate) fn start(conn: Connection, root: &Path, instance_id: yokai_types::InstanceId, ownership: InstanceLock, options: StoreOptions) -> Result<Self, InstanceError> {
        let close_failed = Arc::new(AtomicBool::new(false));
        let conn = StartupConnection::new(conn, &close_failed);
        // No ownership release before the startup connection's confirmed close,
        // including defensive option validation and partial reader initialization.
        if let Err(error) = options.validate() {
            drop(conn);
            if close_failed.load(Ordering::Acquire) { std::mem::forget(ownership); } else { drop(ownership); }
            return Err(error);
        }
        let readers: Vec<_> = match (0..options.readers).map(|_| reader_connection(&root.join("instance.db"))
            .map(|conn| StartupConnection::new(conn, &close_failed))).collect::<Result<Vec<_>, _>>() {
            Ok(readers) => readers,
            Err(error) => {
                drop(conn);
                if close_failed.load(Ordering::Acquire) { std::mem::forget(ownership); } else { drop(ownership); }
                return Err(error);
            }
        };
        // Own the lock before any fallible thread creation. Drop handles partial startup too.
        let shared = Arc::new(Shared {
            instance_id,
            state: Mutex::new(State { status: StoreState::Running, degradation_generation: 0, next_id: Some(1), next_reader: 0, completed: VecDeque::new(), metrics: StoreMetrics::default() }), closing: AtomicBool::new(false),
            readers_alive: AtomicUsize::new(0), close_failed,
            workers: Mutex::new(Vec::new()),
            #[cfg(feature = "test-api")]
            skip_checkpoint: AtomicBool::new(false),
            #[cfg(feature = "test-api")]
            read_events: Mutex::new(VecDeque::new()),
        });
        let (sender, receiver) = mpsc::sync_channel(options.queue_capacity);
        let mut runtime = Self { handle: StoreHandle { shared: shared.clone(), writer: sender, readers: Vec::new() },
            threads: Vec::new(), ownership: Some(ownership), close_timeout: options.close_timeout };
        let s = shared.clone();
        let wal = root.join("instance.db-wal");
        match spawn_worker(move || writer::run(conn.take(), receiver, s, wal), "yokai-writer") {
            Ok(handle) => runtime.threads.push(handle),
            Err(error) => {
                // The failed spawn has dropped its RW closure. Close every RO connection
                // still here before Runtime::drop can release the persistent lock.
                drop(readers);
                return Err(InstanceError::Store(format!("start writer: {error}")));
            }
        }
        for conn in readers {
            let scope = Arc::new(reader::Scope::new(conn.conn.as_ref().expect("startup reader present").get_interrupt_handle()));
            let reader_scope = scope.clone();
            let (sender, receiver) = mpsc::sync_channel(options.queue_capacity);
            runtime.handle.readers.push(sender);
            let s = shared.clone();
            s.readers_alive.fetch_add(1, Ordering::AcqRel);
            let started = spawn_worker(move || reader::run(conn.take(), receiver, s, reader_scope), "yokai-reader");
            match started {
                Ok(handle) => runtime.threads.push(handle),
                Err(e) => {
                    shared.readers_alive.fetch_sub(1, Ordering::AcqRel);
                    return Err(InstanceError::Store(format!("start reader: {e}")));
                }
            }
            let s = shared.clone();
            runtime.threads.push(spawn_worker(move || reader::watchdog(s, scope), "yokai-reader-watchdog")
                .map_err(|e| InstanceError::Store(format!("start watchdog: {e}")))?);
        }
        let ready = Instant::now() + Duration::from_secs(10);
        while lock(&shared.workers).len() != 1 + 2 * options.readers {
            if Instant::now() >= ready || runtime.threads.iter().any(JoinHandle::is_finished) {
                return Err(InstanceError::Store("workers did not become ready".into()));
            }
            thread::sleep(Duration::from_millis(1));
        }
        Ok(runtime)
    }

    pub(crate) fn handle(&self) -> StoreHandle { self.handle.clone() }
    pub(crate) fn is_locked(&self) -> bool { self.ownership.as_ref().is_some_and(InstanceLock::is_locked) }

    pub(crate) fn close(&mut self) -> Result<(), InstanceError> {
        if self.ownership.is_none() { return Ok(()); }
        lock(&self.handle.shared.state).status = StoreState::ShuttingDown;
        self.handle.shared.closing.store(true, Ordering::Release);
        let deadline = Instant::now() + self.close_timeout;
        while self.threads.iter().any(|handle| !handle.is_finished()) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        if self.threads.iter().any(|handle| !handle.is_finished()) {
            if let Some(ownership) = self.ownership.take() { std::mem::forget(ownership); }
            return Err(InstanceError::CloseIncomplete);
        }
        let mut failed = self.handle.shared.close_failed.load(Ordering::Acquire);
        for handle in self.threads.drain(..) { failed |= handle.join().is_err(); }
        if failed {
            if let Some(ownership) = self.ownership.take() { std::mem::forget(ownership); }
            return Err(InstanceError::CloseIncomplete);
        }
        #[cfg(feature = "test-api")]
        crate::failpoint::record_connection("connection-closed");
        drop(self.ownership.take());
        lock(&self.handle.shared.state).status = StoreState::Closed;
        #[cfg(feature = "test-api")]
        crate::failpoint::record_connection("lock-released");
        Ok(())
    }

    #[cfg(feature = "test-api")]
    pub(crate) fn worker_threads(&self) -> Vec<(String, String)> { lock(&self.handle.shared.workers).clone() }
    #[cfg(feature = "test-api")]
    pub(crate) fn reader_events(&self) -> Vec<crate::test_api::ReaderEvent> { lock(&self.handle.shared.read_events).drain(..).collect() }
    #[cfg(feature = "test-api")]
    pub(crate) fn set_next_request_id(&self, id: u64) { lock(&self.handle.shared.state).next_id = Some(id); }

    #[cfg(feature = "test-api")]
    pub(crate) fn block_writer(&self) -> Result<crate::test_api::Pause, InstanceError> {
        let (tx, rx) = mpsc::sync_channel(1);
        let pause = crate::test_api::Pause::new();
        let released = pause.released.clone();
        self.handle.writer.try_send(WriterMessage::Control(Box::new(move |_| {
            let _ = tx.send(());
            while !released.load(Ordering::Acquire) { thread::sleep(Duration::from_millis(1)); }
        }))).map_err(|e| InstanceError::Store(e.to_string()))?;
        rx.recv_timeout(Duration::from_secs(2)).map_err(|e| InstanceError::Store(e.to_string()))?;
        Ok(pause)
    }

    #[cfg(feature = "test-api")]
    pub(crate) fn control<T: Send + 'static>(&self, run: impl FnOnce(&Connection) -> Result<T, String> + Send + 'static) -> Result<T, InstanceError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.handle.writer.try_send(WriterMessage::Control(Box::new(move |c| { let _ = tx.send(run(c)); })))
            .map_err(|e| InstanceError::Store(format!("test control admission: {e}")))?;
        rx.recv_timeout(Duration::from_secs(10)).map_err(|e| InstanceError::Store(e.to_string()))?
            .map_err(InstanceError::Store)
    }

    #[cfg(feature = "test-api")]
    pub(crate) fn no_checkpoint_on_close(&self) -> Result<(), InstanceError> {
        self.control(|c| c.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true).map(|_| ()).map_err(|e| e.to_string()))?;
        self.handle.shared.skip_checkpoint.store(true, Ordering::Release);
        Ok(())
    }
}

impl Drop for Runtime {
    fn drop(&mut self) { let _ = self.close(); }
}
