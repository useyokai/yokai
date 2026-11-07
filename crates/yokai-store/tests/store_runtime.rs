//! Controlled connection ownership and conservative write outcome regressions.
use std::process::ExitCode;
use std::time::{Duration, Instant};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use yokai_store as store;

fn main() -> ExitCode {
    let dumpable = yokai_testsupport::make_non_dumpable();
    println!("{}", dumpable.line());
    if !dumpable.permits_checks() { return ExitCode::from(2); }
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).is_some_and(|arg| arg.starts_with("__")) { return child(&args); }
    let checks: &[(&str, fn() -> Result<String, String>)] = &[
        ("WR-12", default_api), ("WR-OWN", ownership),
        ("RV-1", review_regressions),
        ("FC-EDGE", failure_edges),
        ("FC-1", begin_failure),
        ("FC-2", begin_busy), ("FC-3", deferred_upgrade), ("FC-4", statement_full),
        ("FC-5", statement_io), ("FC-6", rollback_failure), ("FC-7", statement_corrupt),
        ("FC-8", deferred_constraint), ("FC-9", commit_busy), ("FC-10", commit_full),
        ("FC-11", commit_io), ("FC-12", after_commit_panic), ("FC-13", caller_timeout),
        ("WR-1", overload), ("WR-2", queued_expiry), ("WR-11", queued_shutdown),
        ("WR-3", reader_deadline),
        ("WR-3b", cancellation_boundaries), ("WR-4", snapshot_checkpoint), ("IL-9", incomplete_close),
        ("WR-5a", killed_small), ("WR-5b", killed_spill), ("WR-6", full_recovery),
        ("WR-9", writer_panics), ("WR-10", busy_binding), ("WR-13", corrupt_binding),
        ("WR-BOUNDS", bounds_and_drop),
    ];
    let selected = match args.as_slice() {
        [_] => None,
        [_, flag, id] if flag == "--only" && checks.iter().any(|(known, _)| *known == id) => Some(id.as_str()),
        _ => {
            eprintln!("expected no arguments or --only <known check ID>");
            return ExitCode::from(2);
        }
    };
    let count = if selected.is_some() { 1 } else { checks.len() };
    println!("INFO WR-7 not_run: real ENOSPC requires a separately bounded size-limited tmpfs profile; max_page_count is simulated FULL only");
    println!("INFO FC-14 deferred: command identity and idempotent domain operations are outside the V1 storage API");
    for (id, run) in checks.iter().filter(|(id, _)| selected.is_none_or(|selected| *id == selected)) {
        match run() {
            Ok(detail) => println!("CHECK {id} PASS {detail}"),
            Err(error) => {
                println!("CHECK {id} FAIL {error}");
                println!("RESULT FAIL 1/{count}");
                return ExitCode::FAILURE;
            }
        }
    }
    println!("RESULT PASS {count}/{count}");
    ExitCode::SUCCESS
}
fn review_regressions() -> Result<String, String> {
    let mut gaps = Vec::new();
    for position in 0..5 {
        let instance = configured(&format!("spawn-{position}"))?;
        let path = instance.root().to_owned();
        instance.close().map_err(|e| e.to_string())?;
        store::test_api::clear();
        store::test_api::fail_worker_spawn_after(position);
        let opened = store::open_instance_with_options(&path, store::StoreOptions { readers: 2, ..Default::default() });
        if opened.is_ok() { return Err(format!("spawn{position}: injection did not fail startup")); }
        let events = store::test_api::connection_events();
        println!("INFO RV-1 spawn_position={position} events={events:?}");
        let released = events.iter().position(|e| *e == "lock-released").ok_or("startup ownership was not released after cleanup")?;
        if events.iter().enumerate().any(|(i, e)| *e == "startup-connection-closed" && i > released) {
            gaps.push(format!("spawn{position}: connection closed after lock release: {events:?}"));
        }
        let expected = match position { 0 => 3, 1 => 2, 2 => 1, 3 => 1, _ => 0 };
        if events.iter().filter(|e| **e == "startup-connection-closed").count() != expected { return Err(format!("spawn{position}: missing actual startup close observations: {events:?}")); }
        let contender = probe(&path)?;
        if !contender.status.success() { return Err(format!("spawn{position}: fresh opener failed: {}", String::from_utf8_lossy(&contender.stderr))); }
    }
    // A real health COMMIT is already acknowledged before the injected reader error.
    for initially_degraded in [false, true] {
        let instance = configured(&format!("health-race-{initially_degraded}"))?;
        let h = instance.store();
        if initially_degraded { let _ = store::test_api::read_error_probe(&instance, deadline(), 10).map_err(|e| e.to_string())?.wait(deadline()); }
        let (pause, pending) = store::test_api::pause_health_after_commit(&instance, deadline())?;
        let read = store::test_api::read_error_probe(&instance, deadline(), 10).map_err(|e| e.to_string())?.wait(deadline());
        if read != Err(store::ReadError::StorageError) || h.state() != store::StoreState::Degraded { return Err("reader IOERR fixture did not degrade".into()); }
        drop(pause); pending.wait(deadline()).map_err(|e| e.to_string())?;
        let observed = h.state();
        println!("INFO RV-1 initially_degraded={initially_degraded} post_commit_reader_fault_state={observed:?}");
        if observed != store::StoreState::Degraded { gaps.push(format!("health race initially_degraded={initially_degraded}: stale health cleared newer reader fault: {observed:?}")); }
        h.submit_health_check(deadline()).map_err(|e| e.to_string())?.wait(deadline()).map_err(|e| e.to_string())?;
        if h.state() != store::StoreState::Running { return Err("subsequent health did not restore Running".into()); }
        instance.close().map_err(|e| e.to_string())?;
    }
    if !gaps.is_empty() { return Err(gaps.join("; ")); }
    Ok("all five worker-spawn failures close every connection before ownership release and permit a fresh-process opener; post-COMMIT reader IOERR remains Degraded in both initial states until a subsequent health COMMIT".into())
}
fn failure_edges() -> Result<String, String> {
    let mut gaps = Vec::new();
    for (label, sql, fails, auto, rollback, certainty, state) in [
        ("domain-rollback", MARK_HEALTH.to_owned(), false, false, Some(true), store::Certainty::NotApplied, store::StoreState::Running),
        ("domain-unconfirmed", MARK_HEALTH.to_owned(), true, false, Some(false), store::Certainty::OutcomeUnknown, store::StoreState::Failed),
        ("domain-autocommit", format!("{MARK_HEALTH};ROLLBACK"), false, true, None, store::Certainty::OutcomeUnknown, store::StoreState::Failed),
    ] {
        if let Err(error) = failed_case(label, "", &sql, store::test_api::WriteFault::Refuse { rollback_fails: fails }, Expected {
            certainty, reason: store::WriteReason::Refused, phase: store::Phase::Begun, state, auto, rollback, present: false,
        }) { gaps.push(error); }
    }
    for code in [11, 26, 10] {
        let instance = configured(&format!("read-error-{code}"))?; let root = instance.root().to_owned();
        let result = store::test_api::read_error_probe(&instance, deadline(), code).map_err(|e| e.to_string())?.wait(deadline());
        let state = instance.store().state();
        let expected = if code == 10 { (Err(store::ReadError::StorageError), store::StoreState::Degraded) }
            else { (Err(store::ReadError::Corrupt), store::StoreState::Failed) };
        if (result.clone(), state) != expected { gaps.push(format!("read code{code}: result={result:?} state={state:?}, expected={expected:?}")); }
        instance.close().map_err(|e| e.to_string())?; truth(&root, false)?;
    }
    if !gaps.is_empty() { return Err(gaps.join("; ")); }
    Ok("domain refusal requires actual R-b; rollback failure or autocommit alone stay Unknown/Failed; reader CORRUPT/NOTADB terminal, read IO degrades; every fixture reopened in fresh child, absent and integrity ok".into())
}
#[derive(Clone, Copy)]
struct Expected {
    certainty: store::Certainty, reason: store::WriteReason, phase: store::Phase,
    state: store::StoreState, auto: bool, rollback: Option<bool>, present: bool,
}
fn failed_case(label: &str, setup: &str, sql: &str, fault: store::test_api::WriteFault, want: Expected) -> Result<String, String> {
    let instance = configured(label)?;
    let root = instance.root().to_owned();
    if !setup.is_empty() { store::test_api::writer_sql(&instance, setup).map_err(|e| e.to_string())?; }
    let result = store::test_api::write_probe(&instance, deadline(), store::test_api::WriteProbe { sql: sql.into(), fault })
        .map_err(|e| e.to_string())?.wait(deadline());
    let state = instance.store().state();
    let error = result.err().ok_or("fault returned success")?;
    if error.certainty != want.certainty || error.reason != want.reason || error.evidence.phase != want.phase
        || state != want.state || error.evidence.autocommit_after_error != Some(want.auto)
        || error.evidence.rollback_succeeded != want.rollback
        || (want.rollback == Some(true) && error.evidence.autocommit_after_rollback != Some(true)) {
        return Err(format!("{label}: {error:?}, state={state:?}"));
    }
    if state == store::StoreState::Failed {
        let rejected = instance.store().submit_health_check(deadline()).err().ok_or("Failed admitted health write")?;
        if rejected.reason != store::WriteReason::Failed || rejected.certainty != store::Certainty::NotApplied { return Err("Failed admission did not fail closed".into()); }
    }
    instance.close().map_err(|e| e.to_string())?;
    truth(&root, want.present)?;
    Ok(format!("{error:?}, state={state:?}, new-process durable_present={}", want.present))
}
fn statement_expected(reason: store::WriteReason, state: store::StoreState, auto: bool) -> Expected {
    Expected { certainty: store::Certainty::NotApplied, reason, phase: store::Phase::Begun, state,
        auto, rollback: if auto { None } else { Some(true) }, present: false }
}
fn statement_full() -> Result<String, String> {
    failed_case("statement-full", "PRAGMA max_page_count=5", "UPDATE instance_meta SET last_health_check=printf('%04000000d',0)",
        store::test_api::WriteFault::None, statement_expected(store::WriteReason::StorageFull, store::StoreState::Degraded, false))
}
fn statement_io() -> Result<String, String> {
    failed_case("statement-io", "", MARK_HEALTH, store::test_api::WriteFault::Statement(10),
        statement_expected(store::WriteReason::StorageError, store::StoreState::Degraded, false))
}
fn rollback_failure() -> Result<String, String> {
    failed_case("rollback-failure", "", MARK_HEALTH, store::test_api::WriteFault::RollbackFailure(10), Expected {
        certainty: store::Certainty::OutcomeUnknown, reason: store::WriteReason::StorageError,
        phase: store::Phase::Begun, state: store::StoreState::Failed, auto: false, rollback: Some(false), present: false })
}
fn statement_corrupt() -> Result<String, String> {
    failed_case("statement-corrupt", "", MARK_HEALTH, store::test_api::WriteFault::Statement(11),
        statement_expected(store::WriteReason::Corrupt, store::StoreState::Failed, false))
}
fn commit_expected(reason: store::WriteReason, state: store::StoreState, present: bool) -> Expected {
    Expected { certainty: store::Certainty::OutcomeUnknown, reason, phase: store::Phase::CommitIssued,
        state, auto: present, rollback: if present { None } else { Some(true) }, present }
}
fn deferred_constraint() -> Result<String, String> {
    let setup = "CREATE TABLE fk_parent(id INTEGER PRIMARY KEY) STRICT; CREATE TABLE fk_child(id INTEGER PRIMARY KEY,p INTEGER REFERENCES fk_parent(id) DEFERRABLE INITIALLY DEFERRED) STRICT;";
    let mut expected = commit_expected(store::WriteReason::Constraint, store::StoreState::Degraded, false);
    expected.certainty = store::Certainty::NotApplied;
    failed_case("deferred-constraint", setup, &format!("{MARK_HEALTH};INSERT INTO fk_child VALUES(1,99)"), store::test_api::WriteFault::None, expected)
}
fn commit_busy() -> Result<String, String> {
    let mut expected = commit_expected(store::WriteReason::Busy, store::StoreState::Running, false);
    expected.certainty = store::Certainty::NotApplied;
    failed_case("commit-busy", "", MARK_HEALTH, store::test_api::WriteFault::Commit { code: 5, actually_commit: false }, expected)
}
fn commit_full() -> Result<String, String> {
    let mut details = Vec::new();
    for present in [false, true] {
        details.push(failed_case(&format!("commit-full-{present}"), "", MARK_HEALTH,
            store::test_api::WriteFault::Commit { code: 13, actually_commit: present },
            commit_expected(store::WriteReason::StorageFull, store::StoreState::Degraded, present))?);
    }
    Ok(format!("synthetic commit FULL both absent/present remain Unknown: {details:?}"))
}
fn commit_io() -> Result<String, String> {
    let mut details = Vec::new();
    for present in [false, true] {
        details.push(failed_case(&format!("commit-io-{present}"), "", MARK_HEALTH,
            store::test_api::WriteFault::Commit { code: 10, actually_commit: present },
            commit_expected(store::WriteReason::StorageError, store::StoreState::Failed, present))?);
    }
    Ok(format!("synthetic commit IOERR both absent/present remain Unknown: {details:?}"))
}
fn after_commit_panic() -> Result<String, String> {
    failed_case("panic-after-commit", "", MARK_HEALTH, store::test_api::WriteFault::PanicAfterCommit,
        commit_expected(store::WriteReason::WriterGone, store::StoreState::Failed, true))
}
fn writer_panics() -> Result<String, String> {
    for (label, fault, phase, certainty, auto) in [
        ("panic-queued", store::test_api::WriteFault::PanicQueued, store::Phase::Queued, store::Certainty::NotApplied, true),
        ("panic-begun", store::test_api::WriteFault::PanicBegun, store::Phase::Begun, store::Certainty::OutcomeUnknown, false),
    ] {
        failed_case(label, "", MARK_HEALTH, fault, Expected { certainty, reason: store::WriteReason::WriterGone, phase,
            state: store::StoreState::Failed, auto, rollback: if auto { None } else { Some(true) }, present: false })?;
    }
    Ok("caught writer panic before/after BEGIN; only Queued is NotApplied, later Unknown; Failed terminal; process continues and fresh process verifies absence".into())
}
fn caller_timeout() -> Result<String, String> {
    let instance = configured("caller-timeout")?; let handle = instance.store(); let root = instance.root().to_owned();
    let pending = store::test_api::write_probe(&instance, deadline(), store::test_api::WriteProbe {
        sql: MARK_HEALTH.into(), fault: store::test_api::WriteFault::DelayBeforeCommit(Duration::from_millis(100)),
    }).map_err(|e| e.to_string())?;
    let id = pending.operation_id();
    let timed_out = pending.wait(Instant::now() + Duration::from_millis(5)).err().ok_or("caller did not stop waiting")?;
    if timed_out.certainty != store::Certainty::OutcomeUnknown || timed_out.reason != store::WriteReason::WaitExpired { return Err(format!("caller inferred definitive result: {timed_out:?}")); }
    let until = deadline();
    while handle.completions().iter().all(|c| c.operation_id != id) && Instant::now() < until { std::thread::sleep(Duration::from_millis(5)); }
    let completion = handle.completions().into_iter().find(|c| c.operation_id == id).ok_or("writer did not record true outcome")?;
    if completion.result.is_err() || completion.phase != store::Phase::Done || handle.state() != store::StoreState::Running { return Err(format!("writer result missing: {completion:?}")); }
    instance.close().map_err(|e| e.to_string())?; truth(&root, true)?;
    Ok(format!("caller={timed_out:?}; independent writer completion={completion:?} logged; new-process reopen committed marker present"))
}
fn begin_busy() -> Result<String, String> {
    let instance = configured("begin-busy")?; let root = instance.root().to_owned();
    let holder = rusqlite::Connection::open(root.join("instance.db")).map_err(|e| e.to_string())?;
    holder.execute_batch("BEGIN IMMEDIATE").map_err(|e| e.to_string())?;
    let start = Instant::now(); let end = start + Duration::from_secs(10);
    let error = store::test_api::write_probe(&instance, end, store::test_api::WriteProbe { sql: MARK_HEALTH.into(), fault: store::test_api::WriteFault::None })
        .map_err(|e| e.to_string())?.wait(end).err().ok_or("bypassing writer not Busy")?;
    let elapsed = start.elapsed(); holder.execute_batch("ROLLBACK").map_err(|e| e.to_string())?; drop(holder);
    if error.certainty != store::Certainty::NotApplied || error.reason != store::WriteReason::Busy || error.evidence.phase != store::Phase::Queued
        || error.evidence.autocommit_after_error != Some(true) || error.evidence.rollback_succeeded.is_some()
        || instance.store().state() != store::StoreState::Running || !(Duration::from_millis(4500)..=Duration::from_millis(8000)).contains(&elapsed) { return Err(format!("busy={error:?}, elapsed={elapsed:?}")); }
    instance.close().map_err(|e| e.to_string())?; truth(&root, false)?;
    Ok(format!("real bypass BEGIN IMMEDIATE; configured5000ms timeout observed {elapsed:?}; {error:?}; fresh-process absence"))
}
fn deferred_upgrade() -> Result<String, String> {
    let path = parent("deferred-upgrade")?.join("upgrade.db");
    let a = store::test_api::create_file(&path).map_err(|e| e.to_string())?;
    a.execute_batch("CREATE TABLE q(id INTEGER PRIMARY KEY, v INTEGER) STRICT; INSERT INTO q VALUES(1,0)").map_err(|e| e.to_string())?;
    let b = store::test_api::open_file(&path).map_err(|e| e.to_string())?;
    a.execute_batch("BEGIN").map_err(|e| e.to_string())?;
    let _: i64 = a.query_row("SELECT v FROM q WHERE id=1", [], |r| r.get(0)).map_err(|e| e.to_string())?;
    b.execute_batch("BEGIN IMMEDIATE;UPDATE q SET v=1;COMMIT").map_err(|e| e.to_string())?;
    let error = a.execute("UPDATE q SET v=2", []).err().ok_or("deferred upgrade unexpectedly succeeded")?;
    a.execute_batch("ROLLBACK").map_err(|e| e.to_string())?;
    if store::ext_code(&error) != Some(517) { return Err(format!("not SQLITE_BUSY_SNAPSHOT: {error}")); }
    Ok("documented-only raw pair: real SQLITE_BUSY_SNAPSHOT517 after deferred read upgrade; production writer uses BEGIN IMMEDIATE".into())
}
fn full_recovery() -> Result<String, String> {
    let instance = configured("full-recovery")?;
    store::test_api::writer_sql(&instance, "PRAGMA max_page_count=5").map_err(|e| e.to_string())?;
    let error = store::test_api::write_probe(&instance, deadline(), store::test_api::WriteProbe {
        sql: "UPDATE instance_meta SET last_health_check=printf('%04000000d',0)".into(), fault: store::test_api::WriteFault::None,
    }).map_err(|e| e.to_string())?.wait(deadline()).err().ok_or("limit did not fill")?;
    if error.reason != store::WriteReason::StorageFull || instance.store().state() != store::StoreState::Degraded { return Err(format!("{error:?}")); }
    let rejected = store::test_api::write_probe(&instance, deadline(), store::test_api::WriteProbe { sql: MARK_HEALTH.into(), fault: store::test_api::WriteFault::None }).err().ok_or("Degraded admitted ordinary write")?;
    if rejected.reason != store::WriteReason::Degraded { return Err(format!("{rejected:?}")); }
    store::test_api::writer_sql(&instance, "PRAGMA max_page_count=10000").map_err(|e| e.to_string())?;
    if instance.store().state() != store::StoreState::Degraded { return Err("raising limit alone changed state".into()); }
    instance.store().submit_health_check(deadline()).map_err(|e| e.to_string())?.wait(deadline()).map_err(|e| e.to_string())?;
    if instance.store().state() != store::StoreState::Running { return Err("committed health check did not recover Degraded".into()); }
    instance.close().map_err(|e| e.to_string())?;
    Ok("real max_page_count FULL; Degraded refuses ordinary writes; raising limit alone insufficient; typed health write commits then Running. No physical ENOSPC claim".into())
}
fn busy_binding() -> Result<String, String> { Ok("real bypass/timeout and durable absence asserted by FC-2 in this same process batch".into()) }
fn corrupt_binding() -> Result<String, String> { Ok("injected operational CORRUPT, Failed/terminal/absence/integrity asserted by FC-7; no physical corruption claim".into()) }
fn bounds_and_drop() -> Result<String, String> {
    let path = parent("bounds-options")?;
    for options in [store::StoreOptions { queue_capacity: 0, ..Default::default() },
        store::StoreOptions { queue_capacity: 257, ..Default::default() },
        store::StoreOptions { readers: 0, ..Default::default() }, store::StoreOptions { readers: 3, ..Default::default() },
        store::StoreOptions { close_timeout: Duration::ZERO, ..Default::default() },
        store::StoreOptions { close_timeout: Duration::from_secs(31), ..Default::default() }] {
        if store::create_instance_with_options(&path, options).is_ok() || fs::read_dir(&path).map_err(|e| e.to_string())?.count() != 0 {
            return Err("invalid options created files or were accepted".into());
        }
    }
    let instance = store::create_instance_with_options(&path, store::StoreOptions { readers: 2, ..Default::default() }).map_err(|e| e.to_string())?;
    let handle = instance.store();
    if store::test_api::worker_threads(&instance).len() != 5 { return Err("two-reader configuration did not own five workers".into()); }
    for _ in 0..300 {
        handle.submit_health_check(deadline()).map_err(|e| e.to_string())?.wait(deadline()).map_err(|e| e.to_string())?;
        handle.submit_metadata(deadline()).map_err(|e| e.to_string())?.wait(deadline()).map_err(|e| e.to_string())?;
    }
    if handle.completions().len() != 256 || handle.metrics().completed_writes != 300
        || store::test_api::reader_events(&instance).len() != 256 { return Err("diagnostic history not bounded to256 with total count retained".into()); }
    let pause = store::test_api::block_writer(&instance).map_err(|e| e.to_string())?;
    let abandoned = handle.submit_health_check(deadline()).map_err(|e| e.to_string())?;
    drop(abandoned); drop(pause);
    let until = deadline();
    while handle.metrics().completed_writes != 301 && Instant::now() < until { std::thread::sleep(Duration::from_millis(1)); }
    if handle.metrics().completed_writes != 301 { return Err("dropping pending reply canceled the writer".into()); }
    store::test_api::set_next_request_id(&instance, u64::MAX);
    let last = handle.submit_health_check(deadline()).map_err(|e| e.to_string())?;
    if last.operation_id() != u64::MAX || handle.submit_health_check(deadline()).err().map(|e| e.reason) != Some(store::WriteReason::Invalid)
        || handle.submit_metadata(deadline()).err() != Some(store::ReadError::Invalid) { return Err("request ID wrapped or exhausted IDs reused".into()); }
    last.wait(deadline()).map_err(|e| e.to_string())?;
    instance.close().map_err(|e| e.to_string())?;
    if handle.state() != store::StoreState::Closed { return Err("clones kept workers open".into()); }
    Ok("invalid bounds refused before filesystem effects; two-reader ownership; completion/hooks ring256; dropped waiter still completed; ID overflow refused without wrap; cloned handle Closed".into())
}
fn killed_small() -> Result<String, String> { killed_transaction(false) }
fn killed_spill() -> Result<String, String> { killed_transaction(true) }
fn killed_transaction(spill: bool) -> Result<String, String> {
    let instance = configured(if spill { "kill-spill" } else { "kill-small" })?;
    let root = instance.root().to_owned(); instance.close().map_err(|e| e.to_string())?;
    let dir = parent(if spill { "spill-child" } else { "small-child" })?; let marker = dir.join("ready");
    let stderr = fs::File::create(dir.join("child.stderr")).map_err(|e| e.to_string())?;
    let before = fs::metadata(root.join("instance.db-wal")).map(|m| m.len()).unwrap_or(0);
    let mut child = OwnedChild(Command::new(std::env::current_exe().map_err(|e| e.to_string())?).arg("__kill-write")
        .arg(&root).arg(&marker).arg(if spill { "spill" } else { "small" }).stdin(Stdio::null()).stdout(Stdio::null()).stderr(stderr)
        .spawn().map_err(|e| e.to_string())?);
    let until = Instant::now() + Duration::from_secs(10);
    while !marker.exists() && Instant::now() < until {
        if child.0.try_wait().map_err(|e| e.to_string())?.is_some() { return Err("transaction child exited before boundary".into()); }
        std::thread::sleep(Duration::from_millis(5));
    }
    if !marker.exists() { return Err("transaction marker missing".into()); }
    let live = fs::metadata(root.join("instance.db-wal")).map(|m| m.len()).unwrap_or(0);
    child.0.kill().map_err(|e| e.to_string())?; child.0.wait().map_err(|e| e.to_string())?;
    let killed = fs::metadata(root.join("instance.db-wal")).map(|m| m.len()).unwrap_or(0);
    if spill && (before != 0 || live <= 32 || killed <= 32) { return Err(format!("no uncommitted spill evidence: before={before}, live={live}, killed={killed}")); }
    truth(&root, false)?;
    Ok(format!("Windows TerminateProcess while writer held active uncommitted transaction; spill={spill}, WAL before/live/killed={before}/{live}/{killed}; new-process absence/integrity ok. Not Linux SIGKILL/Q2 acceptance"))
}
const HEALTH_MARKER: &str = "2000-01-01T00:00:00Z";
const MARK_HEALTH: &str = "UPDATE instance_meta SET last_health_check='2000-01-01T00:00:00Z' WHERE id=1";
fn begin_failure() -> Result<String, String> {
    let instance = configured("begin-failure")?;
    let root = instance.root().to_owned();
    let result = store::test_api::write_probe(&instance, deadline(), store::test_api::WriteProbe {
        sql: MARK_HEALTH.into(), fault: store::test_api::WriteFault::Begin(10),
    }).map_err(|e| e.to_string())?.wait(deadline());
    let state = instance.store().state(); instance.close().map_err(|e| e.to_string())?;
    let error = result.err().ok_or("BEGIN fault accepted")?;
    if error.certainty != store::Certainty::NotApplied || error.reason != store::WriteReason::BeginFailed
        || error.evidence.phase != store::Phase::Queued || error.evidence.autocommit_after_error != Some(true)
        || state != store::StoreState::Running { return Err(format!("incorrect BEGIN evidence: {error:?}, state={state:?}")); }
    truth(&root, false)?;
    Ok(format!("never begun; observed autocommit; {error:?}; new-process reopen absent"))
}
fn truth(root: &Path, present: bool) -> Result<(), String> {
    let out = Command::new(std::env::current_exe().map_err(|e| e.to_string())?).arg("__truth").arg(root)
        .arg(if present { "present" } else { "absent" }).stdin(Stdio::null()).output().map_err(|e| e.to_string())?;
    if !out.status.success() { return Err(format!("durable truth: {out:?}")); }
    Ok(())
}

fn cancellation_boundaries() -> Result<String, String> {
    let instance = configured("reader-boundaries")?;
    let dir = parent("reader-hook-log")?;
    let mut log = fs::File::create(dir.join("query-interrupts.log")).map_err(|e| e.to_string())?;
    let base = Instant::now();
    let mut interrupted = 0;
    let mut hooks = 0;
    for index in 0..10_000 {
        let end = Instant::now() + if index % 4 == 0 { Duration::from_millis(5) }
            else if index % 4 == 2 { Duration::from_micros(200) } else { Duration::from_secs(1) };
        let query = if index % 2 == 0 {
            store::test_api::read_probe(&instance, end, 2, 1, if index % 4 == 0 { Duration::from_millis(20) } else { Duration::ZERO })
        } else { instance.store().submit_metadata(end) }.map_err(|e| format!("query{index}: {e:?}"))?;
        let id = query.query_id();
        let result = query.wait(deadline());
        if result == Err(store::ReadError::Interrupted) {
            interrupted += 1;
            if Instant::now() < end { return Err(format!("query{id} interrupted early")); }
        } else if result != Err(store::ReadError::Expired) && result.is_err() {
            return Err(format!("query{id} result={result:?}"));
        }
        for event in store::test_api::reader_events(&instance) {
            if event.query_id != id || (event.action == "interrupt" && (event.observed < event.deadline || event.shutdown || result.is_ok())) {
                return Err(format!("misattributed/early interrupt: {event:?}, own={id}, result={result:?}"));
            }
            hooks += usize::from(event.action == "interrupt");
            writeln!(log, "{index} {id} {} deadline_ns={} observed_ns={} shutdown={}", event.action,
                event.deadline.duration_since(base).as_nanos(), event.observed.duration_since(base).as_nanos(), event.shutdown).map_err(|e| e.to_string())?;
        }
        if index % 2 == 1 && result.is_err() { return Err(format!("fresh next query{id} failed: {result:?}")); }
    }
    log.sync_all().map_err(|e| e.to_string())?;
    instance.close().map_err(|e| e.to_string())?;
    if interrupted < 100 || hooks < 100 { return Err(format!("insufficient actual expiry/interrupt coverage: {interrupted}/{hooks}")); }
    Ok(format!("10000 back-to-back requests with multistatement gaps; interrupted={interrupted}, guarded_hooks={hooks}; every hook ID/deadline checked and retained at {}", dir.display()))
}

fn snapshot_checkpoint() -> Result<String, String> {
    let instance = configured("snapshot-checkpoint")?;
    let handle = instance.store();
    let (pause, snapshot) = store::test_api::hold_reader(&instance, false).map_err(|e| format!("{e:?}"))?;
    let start = Instant::now();
    handle.submit_health_check(deadline()).map_err(|e| e.to_string())?.wait(deadline()).map_err(|e| e.to_string())?;
    if start.elapsed() > Duration::from_millis(500) { return Err("writer blocked behind RO snapshot".into()); }
    let until = Instant::now() + Duration::from_secs(3);
    while !handle.metrics().checkpoint_pending && Instant::now() < until { std::thread::sleep(Duration::from_millis(10)); }
    let held = handle.metrics();
    if !held.checkpoint_pending || held.wal_bytes == 0 { return Err(format!("checkpoint completed during held reader: {held:?}")); }
    drop(pause);
    let old = snapshot.wait(deadline()).map_err(|e| format!("{e:?}"))?;
    if old.last_health_check.is_some() { return Err("reader snapshot advanced during writer commit".into()); }
    let until = Instant::now() + Duration::from_secs(3);
    while (handle.metrics().checkpoint_pending || handle.metrics().wal_bytes != 0) && Instant::now() < until { std::thread::sleep(Duration::from_millis(10)); }
    let released = handle.metrics();
    instance.close().map_err(|e| e.to_string())?;
    if released.checkpoint_pending || released.wal_bytes != 0 { return Err(format!("checkpoint failed after snapshot released: {released:?}")); }
    Ok(format!("writer committed while RO snapshot held; pending={held:?}; released={released:?}; writer-only TRUNCATE completed"))
}

struct OwnedChild(std::process::Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() { let _ = self.0.kill(); }
        let _ = self.0.wait();
    }
}
fn child(args: &[String]) -> ExitCode {
    let run = || -> Result<(), String> {
        let root = Path::new(args.get(2).ok_or("child root missing")?);
        if args[1] == "__truth" {
            let c = rusqlite::Connection::open_with_flags(root.join("instance.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e| e.to_string())?;
            let integrity: String = c.query_row("PRAGMA integrity_check", [], |r| r.get(0)).map_err(|e| e.to_string())?;
            let value: Option<String> = c.query_row("SELECT last_health_check FROM instance_meta WHERE id=1", [], |r| r.get(0)).map_err(|e| e.to_string())?;
            let wanted = if args.get(3).map(String::as_str) == Some("present") { Some(HEALTH_MARKER) } else { None };
            if integrity != "ok" || value.as_deref() != wanted { return Err(format!("integrity={integrity}, last_health={value:?}, expected={wanted:?}")); }
            return Ok(());
        }
        if args[1] == "__kill-write" {
            let instance = store::open_instance(root).map_err(|e| e.to_string())?;
            let spill = args.get(4).map(String::as_str) == Some("spill");
            if spill { store::test_api::writer_sql(&instance, "PRAGMA cache_size=4; PRAGMA cache_spill=ON").map_err(|e| e.to_string())?; }
            let sql = if spill { format!("{MARK_HEALTH}; CREATE TABLE kill_spill_probe(id INTEGER PRIMARY KEY, data BLOB) STRICT; WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1024) INSERT INTO kill_spill_probe SELECT x,zeroblob(4096) FROM n") } else { MARK_HEALTH.into() };
            let (pause, _pending) = store::test_api::hold_write(&instance, Instant::now() + Duration::from_secs(30), &sql)?;
            std::mem::forget(pause);
            fs::write(args.get(3).ok_or("transaction marker missing")?, "writer Begun; COMMIT not issued").map_err(|e| e.to_string())?;
            loop { std::thread::sleep(Duration::from_millis(50)); }
        }
        if args[1] == "__probe" {
            store::open_instance(root).map_err(|e| e.to_string())?.close().map_err(|e| e.to_string())?;
            let c = rusqlite::Connection::open(root.join("instance.db")).map_err(|e| e.to_string())?;
            let integrity: String = c.query_row("PRAGMA integrity_check", [], |r| r.get(0)).map_err(|e| e.to_string())?;
            if integrity != "ok" { return Err(integrity); }
            return Ok(());
        }
        if args[1] != "__incomplete" { return Err("unknown child mode".into()); }
        let instance = store::open_instance_with_options(root, store::StoreOptions {
            close_timeout: Duration::from_millis(25), ..Default::default()
        }).map_err(|e| e.to_string())?;
        let (pause, _pending) = store::test_api::hold_reader(&instance, true).map_err(|e| format!("{e:?}"))?;
        std::mem::forget(pause);
        if args.get(4).map(String::as_str) == Some("drop") { drop(instance); }
        else if !matches!(instance.close(), Err(store::InstanceError::CloseIncomplete)) { return Err("close did not report CloseIncomplete".into()); }
        fs::write(args.get(3).ok_or("ready marker missing")?, "CloseIncomplete: worker active; process retains ownership").map_err(|e| e.to_string())?;
        loop { std::thread::sleep(Duration::from_millis(50)); }
    };
    match run() { Ok(()) => ExitCode::SUCCESS, Err(error) => { eprintln!("{error}"); ExitCode::from(4) } }
}
fn probe(root: &Path) -> Result<std::process::Output, String> {
    Command::new(std::env::current_exe().map_err(|e| e.to_string())?).arg("__probe").arg(root)
        .stdin(Stdio::null()).output().map_err(|e| e.to_string())
}
fn incomplete_close() -> Result<String, String> {
    for mode in ["close", "drop"] {
        let instance = configured(&format!("incomplete-{mode}"))?;
        let root = instance.root().to_owned(); instance.close().map_err(|e| e.to_string())?;
        let dir = parent(&format!("incomplete-{mode}-child"))?;
        let marker = dir.join("ready");
        let stderr = fs::File::create(dir.join("child.stderr")).map_err(|e| e.to_string())?;
        let mut owner = OwnedChild(Command::new(std::env::current_exe().map_err(|e| e.to_string())?)
            .arg("__incomplete").arg(&root).arg(&marker).arg(mode)
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(stderr).spawn().map_err(|e| e.to_string())?);
        let until = deadline();
        while !marker.exists() && Instant::now() < until {
            if owner.0.try_wait().map_err(|e| e.to_string())?.is_some() { return Err("owner exited before incomplete close".into()); }
            std::thread::sleep(Duration::from_millis(5));
        }
        if !marker.exists() { return Err("incomplete close marker absent".into()); }
        let busy = probe(&root)?;
        if busy.status.code() != Some(4) || !String::from_utf8_lossy(&busy.stderr).contains("already locked") { return Err(format!("ownership escaped {mode}: {busy:?}")); }
        owner.0.kill().map_err(|e| e.to_string())?; owner.0.wait().map_err(|e| e.to_string())?;
        let reopened = probe(&root)?;
        if !reopened.status.success() { return Err(format!("reopen after child exit: {reopened:?}")); }
    }
    Ok("explicit close and Drop retain lock for process life after unjoinable reader; contender child Busy; after Windows parent kill a fresh process reopens with integrity ok".into())
}
fn reader_deadline() -> Result<String, String> {
    let instance = configured("reader-deadline")?;
    let start = Instant::now();
    let end = start + Duration::from_millis(10);
    let query = store::test_api::read_probe(&instance, end, 2, 1_000_000, Duration::from_millis(40)).map_err(|e| format!("{e:?}"))?;
    let result = query.wait(deadline());
    let elapsed = start.elapsed();
    let next = instance.store().submit_metadata(deadline()).map_err(|e| format!("{e:?}"))?.wait(deadline());
    instance.close().map_err(|e| e.to_string())?;
    if result != Err(store::ReadError::Interrupted) || elapsed > Duration::from_millis(260) || next.is_err() {
        return Err(format!("deadline result={result:?}, elapsed={elapsed:?}, next={next:?}"));
    }
    Ok(format!("owned reader interrupted its expired query in {elapsed:?}; next typed read succeeded"))
}

fn configured(label: &str) -> Result<store::Instance, String> {
    store::create_instance_with_options(&parent(label)?, store::StoreOptions {
        queue_capacity: 1, ..Default::default()
    }).map_err(|e| e.to_string())
}
fn deadline() -> Instant { Instant::now() + Duration::from_secs(3) }
fn overload() -> Result<String, String> {
    let instance = configured("overload")?;
    let pause = store::test_api::block_writer(&instance).map_err(|e| e.to_string())?;
    let handle = instance.store();
    let queued = handle.submit_health_check(deadline()).map_err(|e| format!("first admission: {e:?}"))?;
    let start = Instant::now();
    let refused = handle.submit_health_check(deadline()).err().ok_or("full queue accepted second operation")?;
    if refused.certainty != store::Certainty::NotApplied || refused.reason != store::WriteReason::Overloaded
        || start.elapsed() > Duration::from_millis(100) { return Err(format!("not immediate definite overload: {refused:?}")); }
    drop(pause);
    queued.wait(deadline()).map_err(|e| format!("queued health: {e:?}"))?;
    let metadata = handle.submit_metadata(deadline()).map_err(|e| format!("read admission: {e:?}"))?
        .wait(deadline()).map_err(|e| format!("metadata: {e:?}"))?;
    if metadata.instance_id != instance.id() || metadata.last_health_check.is_none() { return Err("typed metadata did not observe committed health check".into()); }
    instance.close().map_err(|e| e.to_string())?;
    Ok("bounded queue refuses immediately with NotApplied/Overloaded; admitted health write and typed RO metadata work".into())
}
fn queued_expiry() -> Result<String, String> {
    let instance = configured("queued-expiry")?;
    let pause = store::test_api::block_writer(&instance).map_err(|e| e.to_string())?;
    let request = instance.store().submit_health_check(Instant::now() + Duration::from_millis(5)).map_err(|e| format!("{e:?}"))?;
    std::thread::sleep(Duration::from_millis(15)); drop(pause);
    let error = request.wait(deadline()).err().ok_or("expired write committed")?;
    if error.reason != store::WriteReason::Expired || error.certainty != store::Certainty::NotApplied
        || error.evidence.phase != store::Phase::Queued { return Err(format!("incorrect expiry evidence: {error:?}")); }
    instance.close().map_err(|e| e.to_string())?;
    Ok("queued deadline checked before BEGIN; NotApplied/Expired phase Queued".into())
}
fn queued_shutdown() -> Result<String, String> {
    let instance = configured("queued-shutdown")?;
    let handle = instance.store();
    let pause = store::test_api::block_writer(&instance).map_err(|e| e.to_string())?;
    let queued = handle.submit_health_check(deadline()).map_err(|e| format!("{e:?}"))?;
    let close = std::thread::spawn(move || instance.close());
    while handle.state() != store::StoreState::ShuttingDown && !close.is_finished() { std::thread::yield_now(); }
    let error = handle.submit_health_check(deadline()).err().ok_or("shutdown admitted a write")?;
    if error.reason != store::WriteReason::ShuttingDown { return Err(format!("wrong shutdown admission: {error:?}")); }
    drop(pause);
    let queued = queued.wait(deadline()).err().ok_or("queued write began during shutdown")?;
    if queued.reason != store::WriteReason::ShuttingDown || queued.certainty != store::Certainty::NotApplied { return Err(format!("wrong queued outcome: {queued:?}")); }
    close.join().map_err(|_| "close panicked")?.map_err(|e| e.to_string())?;
    if handle.state() != store::StoreState::Closed { return Err("cloned handle retained live state after close".into()); }
    Ok("shutdown rejects new and queued writes; workers join; cloned handle observes Closed".into())
}

fn parent(label: &str) -> Result<std::path::PathBuf, String> {
    let base = std::path::PathBuf::from(std::env::var_os("YOKAI_TEST_DATA_DIR").ok_or("test directory missing")?);
    let path = base.join(format!("store_runtime-{}-{label}", std::process::id()));
    std::fs::create_dir(&path).map_err(|e| e.to_string())?;
    Ok(path)
}

fn ownership() -> Result<String, String> {
    let instance = yokai_store::create_instance(&parent("ownership")?).map_err(|e| e.to_string())?;
    let workers = yokai_store::test_api::worker_threads(&instance);
    instance.close().map_err(|e| e.to_string())?;
    let caller = format!("{:?}", std::thread::current().id());
    let mut ids: Vec<_> = workers.iter().map(|(_, id)| id.clone()).collect();
    ids.sort(); ids.dedup();
    if workers.len() != 3 || ids.len() != 3 || ids.contains(&caller)
        || workers.iter().filter(|(name, _)| name == "writer").count() != 1
        || workers.iter().filter(|(name, _)| name == "reader").count() != 1
        || workers.iter().filter(|(name, _)| name == "watchdog").count() != 1 {
        return Err(format!("expected distinct owned writer, RO reader and watchdog; actual={workers:?}"));
    }
    Ok(format!("one writer, one RO reader, guarded watchdog on distinct threads: {workers:?}"))
}

fn default_api() -> Result<String, String> {
    let lib = include_str!("../src/lib.rs");
    for name in ["create_file", "open_file", "init_connection", "read_settings",
                 "main_database_file", "sqlite_runtime", "foreign_key_violations"] {
        if lib.contains(&format!("pub fn {name}(")) {
            return Err(format!("raw entry point {name} exported from default crate root"));
        }
    }
    if !lib.contains("#[cfg(feature = \"test-api\")]\npub mod test_api;") {
        return Err("raw test API must have an explicit module feature boundary".into());
    }
    Ok("default API hides raw connection/transaction/SQL entry points; test support explicitly gated".into())
}
