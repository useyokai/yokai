use super::*;
use std::sync::Condvar;
use std::panic::{catch_unwind, AssertUnwindSafe};

struct Active { id: u64, deadline: Instant, cancelled: bool, shutdown: bool }
struct ScopeState { current: Option<Active>, done: bool }
pub(super) struct Scope {
    state: Mutex<ScopeState>,
    changed: Condvar,
    interrupt: rusqlite::InterruptHandle,
}
impl Scope {
    pub(super) fn new(interrupt: rusqlite::InterruptHandle) -> Self {
        Self { state: Mutex::new(ScopeState { current: None, done: false }), changed: Condvar::new(), interrupt }
    }
    fn check(&self, id: u64) -> Result<(), ReadError> {
        let state = lock(&self.state);
        let active = state.current.as_ref().filter(|active| active.id == id).ok_or(ReadError::Failed)?;
        if active.shutdown { Err(ReadError::ShuttingDown) }
        else if active.cancelled || Instant::now() >= active.deadline { Err(ReadError::Interrupted) }
        else { Ok(()) }
    }
}
fn event(shared: &Shared, active: &Active, action: &'static str) {
    #[cfg(feature = "test-api")]
    {
        let mut events = lock(&shared.read_events);
        if events.len() == 256 { events.pop_front(); }
        events.push_back(crate::test_api::ReaderEvent { query_id: active.id, action, deadline: active.deadline,
            observed: Instant::now(), shutdown: active.shutdown });
    }
    #[cfg(not(feature = "test-api"))]
    let _ = (shared, active, action);
}
pub(super) fn watchdog(shared: Arc<Shared>, scope: Arc<Scope>) {
    worker(&shared, "watchdog");
    let mut state = lock(&scope.state);
    loop {
        let now = Instant::now();
        let closing = shared.closing.load(Ordering::Acquire);
        let expired = state.current.as_ref().filter(|active| closing || now >= active.deadline).map(|active| active.id);
        if let Some(target) = expired {
            if let Some(active) = state.current.as_mut().filter(|active| active.id == target) {
                active.cancelled = true;
                active.shutdown |= closing;
                // Both the ID equality check and interrupt occur while holding the same mutex
                // needed to clear this query and start the next one.
                scope.interrupt.interrupt();
                event(&shared, active, "interrupt");
            }
        }
        if state.done { break; }
        let wait = state.current.as_ref().filter(|active| !active.cancelled)
            .map(|active| active.deadline.saturating_duration_since(now).min(Duration::from_millis(10)))
            .unwrap_or(Duration::from_millis(10));
        state = scope.changed.wait_timeout(state, wait).unwrap_or_else(|poison| poison.into_inner()).0;
    }
}
fn sqlite_error(shared: &Shared, error: rusqlite::Error) -> ReadError {
    let code = crate::ext_code(&error);
    if code.map(|code| code & 0xff) == Some(9) { return ReadError::Interrupted; }
    use std::io::Write;
    let _ = writeln!(std::io::stderr().lock(), "INFO store_read_error instance_id={} error_code={code:?}", shared.instance_id);
    match code.map(|code| code & 0xff) {
        Some(11 | 26) => { shared.change(StoreState::Failed); ReadError::Corrupt }
        Some(7 | 10 | 13 | 14) => { shared.change(StoreState::Degraded); ReadError::StorageError }
        Some(5 | 6) => ReadError::StorageError,
        _ => { shared.change(StoreState::Failed); ReadError::StorageError }
    }
}
fn work(conn: &Connection, request: &ReadRequest, scope: &Scope, shared: &Shared) -> Result<InstanceMetadata, ReadError> {
    match &request.work {
        ReadWork::Metadata => {},
        #[cfg(feature = "test-api")]
        ReadWork::Error(code) => {
            let error = rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(*code), Some("explicit read test-api fault".into()));
            return Err(sqlite_error(shared, error));
        }
        #[cfg(feature = "test-api")]
        ReadWork::Compute { steps, rows, gap } => {
            for step in 0..*steps {
                if step != 0 { thread::sleep(*gap); }
                scope.check(request.id)?;
                conn.query_row("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<?1) SELECT sum(x) FROM n", [*rows], |r| r.get::<_, i64>(0))
                    .map_err(|e| sqlite_error(shared, e))?;
            }
        }
        #[cfg(feature = "test-api")]
        ReadWork::Hold { ready, released, uninterruptible } => {
            conn.execute_batch("BEGIN").map_err(|e| sqlite_error(shared, e))?;
            let snapshot = metadata(conn, shared)?;
            let _ = ready.send(());
            while !released.load(Ordering::Acquire) {
                if !uninterruptible { scope.check(request.id)?; }
                thread::sleep(Duration::from_millis(1));
            }
            conn.execute_batch("ROLLBACK").map_err(|e| sqlite_error(shared, e))?;
            return Ok(snapshot);
        }
    }
    scope.check(request.id)?;
    metadata(conn, shared)
}
fn metadata(conn: &Connection, shared: &Shared) -> Result<InstanceMetadata, ReadError> {
    conn.query_row("SELECT instance_id,format,created_at,last_health_check FROM instance_meta WHERE id=1", [], |r| {
        let bytes: Vec<u8> = r.get(0)?;
        let bytes: [u8; 16] = bytes.try_into().map_err(|_| rusqlite::Error::InvalidQuery)?;
        Ok(InstanceMetadata { instance_id: yokai_types::InstanceId::from_bytes(bytes), format: r.get(1)?,
            created_at: r.get(2)?, last_health_check: r.get(3)? })
    }).map_err(|e| sqlite_error(shared, e))
}
pub(super) fn run(conn: Connection, receiver: mpsc::Receiver<ReadRequest>, shared: Arc<Shared>, scope: Arc<Scope>) {
    worker(&shared, "reader");
    while !shared.closing.load(Ordering::Acquire) {
        match receiver.recv_timeout(Duration::from_millis(10)) {
            Ok(request) => {
                let refusal = {
                    // Register the active ID before close can change admission state.
                    let admission = lock(&shared.state);
                    let refusal = match admission.status {
                        StoreState::ShuttingDown | StoreState::Closed => Some(ReadError::ShuttingDown),
                        StoreState::Failed => Some(ReadError::Failed),
                        _ if Instant::now() >= request.deadline => Some(ReadError::Expired),
                        _ => None,
                    };
                    if refusal.is_none() {
                        let mut state = lock(&scope.state);
                        let active = Active { id: request.id, deadline: request.deadline, cancelled: false, shutdown: false };
                        event(&shared, &active, "start");
                        state.current = Some(active);
                        scope.changed.notify_all();
                    }
                    refusal
                };
                let result = if let Some(error) = refusal { Err(error) } else {
                    // All statements and row iterators are finalized before the ID is cleared.
                    let mut result = catch_unwind(AssertUnwindSafe(|| work(&conn, &request, &scope, &shared))).unwrap_or_else(|_| {
                        shared.change(StoreState::Failed); Err(ReadError::Failed)
                    });
                    if !conn.is_autocommit() && conn.execute_batch("ROLLBACK").is_err() {
                        shared.change(StoreState::Failed); result = Err(ReadError::Failed);
                    }
                    let mut state = lock(&scope.state);
                    if let Some(active) = state.current.take() {
                        if active.shutdown { result = Err(ReadError::ShuttingDown); }
                        else if active.cancelled || Instant::now() >= active.deadline { result = Err(ReadError::Interrupted); }
                        event(&shared, &active, "end");
                    }
                    scope.changed.notify_all();
                    result
                };
                let _ = request.reply.send(result);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {},
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    while let Ok(request) = receiver.try_recv() { let _ = request.reply.send(Err(ReadError::ShuttingDown)); }
    if conn.close().is_err() { shared.close_failed.store(true, Ordering::Release); }
    { let mut state = lock(&scope.state); state.done = true; scope.changed.notify_all(); }
    shared.readers_alive.fetch_sub(1, Ordering::AcqRel);
}
