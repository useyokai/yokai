use super::*;
use std::panic::{catch_unwind, AssertUnwindSafe};
#[cfg(feature = "test-api")]
use crate::test_api::WriteFault;
#[cfg(feature = "test-api")]
fn injected(code: i32) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), Some("explicit test-api fault".into()))
}

pub(super) fn checkpoint(conn: &Connection, shared: &Shared, wal: &Path) {
    let result = (|| -> rusqlite::Result<bool> {
        conn.busy_timeout(Duration::ZERO)?;
        let result = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get::<_, i64>(0));
        conn.busy_timeout(Duration::from_millis(crate::BUSY_TIMEOUT_MS))?;
        result.map(|busy| busy != 0)
    })();
    if let Err(error) = &result {
        use std::io::Write;
        let code = crate::ext_code(error);
        let _ = writeln!(std::io::stderr().lock(), "INFO store_checkpoint instance_id={} error_code={code:?}", shared.instance_id);
        match code.map(|code| code & 0xff) {
            Some(5 | 6) => {},
            Some(13) => shared.change(StoreState::Degraded),
            _ => shared.change(StoreState::Failed),
        }
    }
    let mut state = lock(&shared.state);
    state.metrics.last_checkpoint_error = result.as_ref().err().and_then(crate::ext_code);
    state.metrics.checkpoint_pending = result.unwrap_or(true);
    drop(state);
    observe_wal(shared, wal);
}
fn observe_wal(shared: &Shared, wal: &Path) {
    let sample = match std::fs::metadata(wal) {
        Ok(meta) => (meta.len(), true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (0, true),
        Err(_) => (0, false),
    };
    let mut state = lock(&shared.state);
    state.metrics.wal_bytes = sample.0;
    state.metrics.wal_size_verified = sample.1;
    state.metrics.wal_sampled_at = Some(Instant::now());
}
fn execute_write(conn: &Connection, shared: &Shared, request: &WriteRequest) -> Result<WriteValue, WriteFailure> {
    let health_generation = {
        // This short state-locked decision reserves the dequeued request as current work.
        // Close may then let it finish; requests still queued are rejected. Never hold the
        // admission mutex across SQLite's potentially blocking BEGIN/busy handler.
        let state = lock(&shared.state);
        if let Some(reason) = write_refusal(state.status) { return Err(WriteFailure::queued(Some(request.id), reason)); }
        if state.status == StoreState::Degraded && !request.work.is_health() {
            return Err(WriteFailure::queued(Some(request.id), WriteReason::Degraded));
        }
        if Instant::now() >= request.deadline { return Err(WriteFailure::queued(Some(request.id), WriteReason::Expired)); }
        state.degradation_generation
    };
    let execute = || -> Result<WriteValue, OperationError> {
        #[cfg(feature = "test-api")]
        if let Some(fault) = request.work.fault() {
            match fault { WriteFault::PanicQueued => panic!("test-api panic before BEGIN"),
                WriteFault::Begin(code) => return Err(injected(*code).into()), _ => {} }
        }
        if matches!(&request.work,WriteWork::Accept{..}){crate::durable::point("accept-before-begin")?;}
        conn.execute_batch("BEGIN IMMEDIATE")?;
        lock(&request.progress).phase = Phase::Begun;
        #[cfg(feature="test-api")]
        if matches!(request.work.fault(),Some(WriteFault::PanicBegun)){panic!("test-api panic after BEGIN");}
        let (value, commit) = match &request.work {
            WriteWork::Accept { command, .. } => {
                let (outcome, commit) = crate::durable::accept(conn, command)?;
                (WriteValue::Command(outcome), commit)
            }
            WriteWork::Conversation { request, .. } => {
                let (outcome, commit) = request.apply(conn, shared.instance_id)?;
                (WriteValue::Conversation(outcome), commit)
            }
            _ => (WriteValue::Maintenance, true),
        };
        if !commit {
            #[cfg(feature="test-api")]
            if let Some(WriteFault::RollbackFailure(code))=request.work.fault(){return Err(injected(*code).into());}
            conn.execute_batch("ROLLBACK")?;
            if !conn.is_autocommit(){return Err(rusqlite::Error::InvalidQuery.into());}
            return Ok(value);
        }
        if request.work.is_health() {
                let stamp = crate::instance::utc_timestamp().map_err(|_| rusqlite::Error::InvalidQuery)?;
                if conn.execute("UPDATE instance_meta SET last_health_check=?1 WHERE id=1", [stamp])? != 1 {
                    return Err(rusqlite::Error::QueryReturnedNoRows.into());
                }
        }
        #[cfg(feature = "test-api")]
        if let WriteWork::Probe(probe) = &request.work {
                conn.execute_batch(&probe.sql)?;
        }
        #[cfg(feature="test-api")]
        if let Some(fault)=request.work.fault(){
                match fault {
                    WriteFault::Refuse { .. } => return Err(OperationError::Refused),
                    WriteFault::Statement(code) | WriteFault::RollbackFailure(code) => return Err(injected(*code).into()),
                    WriteFault::DelayBeforeCommit(delay) => thread::sleep(*delay),
                    WriteFault::HoldBeforeCommit { ready, released } => {
                        let _ = ready.send(());
                        while !released.load(Ordering::Acquire) { thread::sleep(Duration::from_millis(1)); }
                    }
                    _ => {},
                }
        }
        lock(&request.progress).phase = Phase::CommitIssued;
        #[cfg(feature = "test-api")]
        if let Some(WriteFault::Commit { code, actually_commit }) = request.work.fault() {
                if *actually_commit { conn.execute_batch("COMMIT")?; }
                return Err(injected(*code).into());
        }
        conn.execute_batch("COMMIT")?;
        if matches!(&request.work,WriteWork::Accept{..}){crate::durable::point("accept-after-commit")?;}
        #[cfg(feature = "test-api")]
        if let WriteWork::HealthPaused { ready, released } = &request.work {
            let _ = ready.send(());
            while !released.load(Ordering::Acquire) { thread::sleep(Duration::from_millis(1)); }
        }
        #[cfg(feature = "test-api")]
        if matches!(request.work.fault(),Some(WriteFault::PanicAfterCommit)){panic!("test-api panic after acknowledged COMMIT before reply");}
        Ok(value)
    };
    match execute() {
        Ok(value) => {
            if request.work.is_health() {
                // This commit can recover only the generation it checked. A reader fault
                // during or after the transaction requires a subsequent health commit.
                let mut state = lock(&shared.state);
                if state.status == StoreState::Degraded && state.degradation_generation == health_generation {
                    state.status = StoreState::Running;
                }
            }
            Ok(value) }
        Err(error) => Err(failure::classify(conn, shared, request, &error)),
    }
}
fn message(conn: &Connection, shared: &Shared, message: WriterMessage) {
    match message {
        WriterMessage::Health(request) => {
            let result = catch_unwind(AssertUnwindSafe(|| execute_write(conn, shared, &request))).unwrap_or_else(|_| {
                shared.change(StoreState::Failed);
                let mut evidence = lock(&request.progress).clone();
                evidence.autocommit_after_error = Some(conn.is_autocommit());
                if !conn.is_autocommit() {
                    evidence.rollback_succeeded = Some(conn.execute_batch("ROLLBACK").is_ok());
                    evidence.autocommit_after_rollback = Some(conn.is_autocommit());
                }
                Err(WriteFailure { operation_id: Some(request.id),
                    certainty: if evidence.phase == Phase::Queued { Certainty::NotApplied } else { Certainty::OutcomeUnknown },
                    reason: WriteReason::WriterGone, evidence })
            });
            shared.complete(request.id, result.as_ref().map(|value| value.success(request.id)).map_err(Clone::clone));
            lock(&request.progress).phase = Phase::Done;
            let _ = request.reply.send(result);
        }
        #[cfg(feature = "test-api")]
        WriterMessage::Control(run) => {
            if catch_unwind(AssertUnwindSafe(|| run(conn))).is_err() { shared.change(StoreState::Failed); }
        }
    }
}
fn next_write(
    receiver: &mpsc::Receiver<WriterMessage>,
    terminal: &mpsc::Receiver<WriterMessage>,
    shared: &Shared,
) -> Option<WriterMessage> {
    // Producers enqueue and signal under the same mutex. Checking both queues
    // before atomically entering the wait prevents a missed terminal wakeup.
    let state = lock(&shared.state);
    if shared.closing.load(Ordering::Acquire) { return None; }
    let take = || terminal.try_recv().or_else(|_| receiver.try_recv()).ok();
    if let Some(request) = take() {
        return Some(request);
    }
    let (_state, _) = shared.writer_ready.wait_timeout(state, shared.writer_idle_wait())
        .unwrap_or_else(|poison| poison.into_inner());
    if shared.closing.load(Ordering::Acquire) { return None; }
    take()
}

pub(super) fn run(
    conn: Connection,
    receiver: mpsc::Receiver<WriterMessage>,
    terminal: mpsc::Receiver<WriterMessage>,
    shared: Arc<Shared>,
    wal: std::path::PathBuf,
) {
    worker(&shared, "writer");
    let mut idle = Instant::now();
    while !shared.closing.load(Ordering::Acquire) {
        match next_write(&receiver, &terminal, &shared) {
            Some(request) => { message(&conn, &shared, request); observe_wal(&shared, &wal); idle = Instant::now(); }
            None => {
                if shared.closing.load(Ordering::Acquire) { break; }
                if idle.elapsed() >= Duration::from_secs(1) && shared.status() != StoreState::Failed {
                    checkpoint(&conn, &shared, &wal); idle = Instant::now();
                }
            }
        }
    }
    while let Ok(request) = terminal.try_recv() { message(&conn, &shared, request); }
    while let Ok(request) = receiver.try_recv() { message(&conn, &shared, request); }
    while shared.readers_alive.load(Ordering::Acquire) != 0 { thread::sleep(Duration::from_millis(1)); }
    let final_checkpoint = {
        #[cfg(feature = "test-api")]
        { !shared.skip_checkpoint.load(Ordering::Acquire) }
        #[cfg(not(feature = "test-api"))]
        { true }
    };
    if final_checkpoint { checkpoint(&conn, &shared, &wal); }
    if conn.close().is_err() { shared.close_failed.store(true, Ordering::Release); }
}
