//! Typed transactions through the owned storage workers.
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};
use yokai_store::{self as store, Certainty, Instance, StoreHandle, StoreOptions, WriteReason};
use yokai_types::{AssistantResponse, CommandId, CompleteFakeRun, ConversationDecision as Decision,
    ConversationEventKind, ConversationId, CreateConversation, FakeOutcome, MessageId, ReplayCursor,
    RunState, RunTicket, SubmitMessage};

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

fn fixture(name: &str, queue_capacity: usize) -> Instance {
    let parent = PathBuf::from(std::env::var_os("YOKAI_TEST_DATA_DIR").expect("fresh fixture root"));
    assert!(parent.is_absolute() && parent.is_dir());
    let parent = parent.join(name);
    std::fs::create_dir(&parent).unwrap();
    store::create_instance_with_options(&parent, StoreOptions { queue_capacity, ..StoreOptions::default() }).unwrap()
}

fn id(n: u8) -> ConversationId { ConversationId::from_bytes([n; 16]) }
fn mid(n: u8) -> MessageId { MessageId::from_bytes([n; 16]) }
fn command(text: &str) -> CommandId { CommandId::new(text).unwrap() }

fn create(handle: &StoreHandle, conversation_id: ConversationId, name: &str) -> Decision {
    let request = CreateConversation { command_id: command(name), conversation_id };
    handle.create_conversation(request, deadline()).unwrap().wait(deadline()).unwrap()
}

fn request(conversation_id: ConversationId, message: u8, name: &str, text: &str) -> SubmitMessage {
    SubmitMessage::new(command(name), conversation_id, mid(message), text).unwrap()
}

fn submit(handle: &StoreHandle, request: SubmitMessage) -> Decision {
    handle.submit_message(request, deadline()).unwrap().wait(deadline()).unwrap()
}

fn accepted(decision: Decision) -> RunTicket {
    let Decision::Accepted(ticket) = decision else { panic!("expected accepted, got {decision:?}") };
    ticket
}

fn completion(ticket: RunTicket, message: u8, body: &str) -> CompleteFakeRun {
    CompleteFakeRun { ticket, outcome: FakeOutcome::Response(AssistantResponse::new(mid(message), body).unwrap()) }
}

fn complete(handle: &StoreHandle, request: CompleteFakeRun) -> Decision {
    handle.complete_fake_run(request, deadline()).unwrap().wait(deadline()).unwrap()
}

fn worker(handle: &StoreHandle, request: SubmitMessage) -> store::WorkerCompletion {
    match handle.submit_worker_message(request, deadline()).unwrap().wait(deadline()).unwrap() {
        store::FakeAdmission::Accepted(permit) => permit,
        store::FakeAdmission::Decision(decision) => panic!("expected a worker permit, got {decision:?}"),
    }
}

fn admission() {
    let instance = fixture("admission", 64);
    let handle = instance.store();
    let conversation = id(1);
    let Decision::Created { run_id } = create(&handle, conversation, "create") else { panic!("create") };
    assert_eq!(create(&handle, conversation, "create"), Decision::Duplicate { run_id, state: RunState::Completed });
    assert_eq!(create(&handle, id(2), "create"), Decision::Conflict);
    assert_eq!(create(&handle, conversation, "different-create"), Decision::Conflict);
    assert_eq!(submit(&handle, request(id(2), 3, "unknown", "hi")), Decision::NotFound);
    let first = request(conversation, 3, "submit", "é");
    let ticket = accepted(submit(&handle, first.clone()));
    let foreign = RunTicket { instance_id: yokai_types::InstanceId::from_bytes([0; 16]), ..ticket };
    assert_eq!(complete(&handle, completion(foreign, 5, "fake: é")), Decision::StaleRun);
    assert_eq!(submit(&handle, first), Decision::Duplicate { run_id: ticket.run_id, state: RunState::Open });
    assert_eq!(submit(&handle, request(conversation, 3, "submit", "e\u{301}")), Decision::Conflict);
    assert_eq!(submit(&handle, request(conversation, 4, "busy", "another")), Decision::Busy);
    let response = completion(ticket, 5, "fake: é");
    assert_eq!(complete(&handle, response.clone()), Decision::Completed { run_id: ticket.run_id });
    assert_eq!(complete(&handle, response), Decision::Duplicate { run_id: ticket.run_id, state: RunState::Completed });
    assert_eq!(complete(&handle, completion(ticket, 5, "different")), Decision::Conflict);
    assert_eq!(submit(&handle, request(conversation, 3, "same-message", "another")), Decision::Conflict);
    let page = handle.replay_events(conversation, ReplayCursor::start(instance.id(), conversation), 20, deadline())
        .unwrap().wait(deadline()).unwrap();
    assert_eq!(page.events.len(), 3);
    assert!(matches!(&page.events[2].kind, ConversationEventKind::Completed(message) if message.id == mid(5) && message.text == "fake: é"));
    let root = instance.root().to_owned();
    instance.close().unwrap();
    let conn = rusqlite::Connection::open_with_flags(root.join("instance.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let counts: (i64, i64, i64, i64, i64) = conn.query_row(
        "SELECT (SELECT count(*) FROM commands), (SELECT count(*) FROM runs), (SELECT count(*) FROM jobs), (SELECT count(*) FROM conversation_messages), (SELECT count(*) FROM conversation_runs WHERE active=1)",
        [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?))).unwrap();
    assert_eq!(counts, (2, 2, 2, 2, 0));
}

fn concurrency() {
    let instance = fixture("concurrency", 64);
    let handle = instance.store();
    for n in 1..=5 {
        assert!(matches!(create(&handle, id(n), &format!("create-{n}")), Decision::Created { .. }));
    }
    let barrier = Arc::new(Barrier::new(8));
    let results = std::thread::scope(|scope| {
        let callers: Vec<_> = (0..8).map(|_| {
            let barrier = Arc::clone(&barrier);
            let handle = handle.clone();
            scope.spawn(move || {
                barrier.wait();
                submit(&handle, request(id(1), 1, "duplicate-race", "hello"))
            })
        }).collect();
        callers.into_iter().map(|thread| thread.join().unwrap()).collect::<Vec<_>>()
    });
    let tickets: Vec<_> = results.iter().filter_map(|result| match result {
        Decision::Accepted(ticket) => Some(*ticket),
        _ => None,
    }).collect();
    assert_eq!(tickets.len(), 1);
    let first = tickets[0];
    assert_eq!(results.iter().filter(|&&decision| decision == (Decision::Duplicate { run_id: first.run_id, state: RunState::Open })).count(), 7);
    let other = std::thread::scope(|scope| {
        let a = scope.spawn(|| submit(&handle, request(id(2), 2, "second-a", "hi")));
        let b = scope.spawn(|| submit(&handle, request(id(2), 6, "second-b", "hi")));
        [a.join().unwrap(), b.join().unwrap()]
    });
    assert_eq!(other.iter().filter(|d| matches!(d, Decision::Accepted(_))).count(), 1);
    assert_eq!(other.iter().filter(|&&d| d == Decision::Busy).count(), 1);
    for n in 3..=4 {
        accepted(submit(&handle, request(id(n), n, &format!("work-{n}"), "hello")));
    }
    assert_eq!(submit(&handle, request(id(5), 5, "fifth", "hello")), Decision::Busy);
    complete(&handle, completion(first, 7, "fake: hello"));
    let held = accepted(submit(&handle, request(id(5), 5, "fifth", "hello")));
    let root = instance.root().to_owned();
    instance.close().unwrap();
    let reopened = store::open_instance(&root).unwrap();
    assert_eq!(complete(&reopened.store(), completion(held, 8, "fake: hello")), Decision::StaleRun);
    reopened.close().unwrap();
}

fn certainty() {
    let instance = fixture("certainty", 1);
    let handle = instance.store();
    let conversation = id(1);
    create(&handle, conversation, "create");
    let request = request(conversation, 2, "write", "hi");
    let blocker = store::test_api::block_writer(&instance).unwrap();
    let expired = handle.submit_message(request.clone(), Instant::now()).unwrap();
    let overloaded = handle.submit_message(request.clone(), deadline()).err().unwrap();
    assert_eq!((overloaded.certainty, overloaded.reason), (Certainty::NotApplied, WriteReason::Overloaded));
    drop(blocker);
    let expired = expired.wait(deadline()).unwrap_err();
    assert_eq!((expired.certainty, expired.reason), (Certainty::NotApplied, WriteReason::Expired));

    let fault = |fault| store::test_api::submit_message_fault(&instance, request.clone(), deadline(), fault)
        .unwrap().wait(deadline()).unwrap_err();
    let error = fault(store::test_api::WriteFault::Statement(13));
    assert_eq!((error.certainty, error.reason), (Certainty::NotApplied, WriteReason::StorageFull));
    assert_eq!(handle.submit_message(request.clone(), deadline()).err().unwrap().reason, WriteReason::Degraded);
    let recover = || handle.submit_health_check(deadline()).unwrap().wait(deadline()).unwrap();
    recover();
    let error = fault(store::test_api::WriteFault::Commit { code: 13, actually_commit: false });
    assert_eq!(error.certainty, Certainty::OutcomeUnknown);
    assert_eq!(error.evidence.rollback_succeeded, Some(true));
    assert_eq!(handle.read_conversation(conversation, deadline()).unwrap().wait(deadline()).unwrap().unwrap().latest_run, None);
    recover();
    let error = fault(store::test_api::WriteFault::Commit { code: 13, actually_commit: true });
    assert_eq!(error.certainty, Certainty::OutcomeUnknown);
    assert_eq!(error.evidence.autocommit_after_error, Some(true));
    recover();
    let Decision::Duplicate { run_id, state: RunState::Open } = submit(&handle, request.clone()) else { panic!("durable identity missing") };
    let ticket = RunTicket { instance_id: instance.id(), conversation_id: conversation, run_id, epoch: 1 };
    let completion = completion(ticket, 3, "fake: hi");
    let fault_completion = |actually_commit| store::test_api::complete_fake_run_fault(&instance, completion.clone(), deadline(),
        store::test_api::WriteFault::Commit { code: 13, actually_commit }).unwrap().wait(deadline()).unwrap_err();
    assert_eq!(fault_completion(false).certainty, Certainty::OutcomeUnknown);
    recover();
    assert_eq!(submit(&handle, request.clone()), Decision::Duplicate { run_id, state: RunState::Open });
    assert_eq!(fault_completion(true).certainty, Certainty::OutcomeUnknown);
    recover();
    assert_eq!(submit(&handle, request), Decision::Duplicate { run_id, state: RunState::Completed });
    assert_eq!(complete(&handle, completion), Decision::Duplicate { run_id, state: RunState::Completed });
    instance.close().unwrap();
}

fn completion_capacity() {
    let instance = fixture("completion-capacity", 4);
    let handle = instance.store();
    for n in 1..=6 {
        assert!(matches!(create(&handle, id(n), &format!("create-{n}")), Decision::Created { .. }));
    }
    let retired = accepted(submit(&handle, request(id(6), 6, "retired", "hi")));
    let retired_response = completion(retired, 16, "fake: hi");
    assert_eq!(complete(&handle, retired_response.clone()), Decision::Completed { run_id: retired.run_id });
    let permits: Vec<_> = (1..=4).map(|n| {
        worker(&handle, request(id(n), n, &format!("submit-{n}"), "hello"))
    }).collect();
    let duplicate = handle.submit_worker_message(request(id(1), 1, "submit-1", "hello"), deadline())
        .unwrap().wait(deadline()).unwrap();
    assert!(matches!(duplicate, store::FakeAdmission::Decision(Decision::Duplicate { state: RunState::Open, .. })));
    assert_eq!(submit(&handle, request(id(5), 5, "fifth", "hi")), Decision::Busy);

    let blocker = store::test_api::block_writer(&instance).unwrap();
    let retries: Vec<_> = (0..4).map(|_| {
        handle.complete_fake_run(retired_response.clone(), deadline()).unwrap()
    }).collect();
    let completions: Vec<_> = permits.into_iter().enumerate().map(|(n, permit)| {
        let ticket = permit.ticket();
        let assistant = 10 + n as u8;
        let pending = permit.submit(completion(ticket, assistant, "fake: hello").outcome, deadline()).unwrap();
        (ticket, mid(assistant), pending)
    }).collect();
    let excess = handle.submit_health_check(deadline()).err().unwrap();
    assert_eq!((excess.certainty, excess.reason), (Certainty::NotApplied, WriteReason::Overloaded));
    drop(blocker);
    let mut completed_runs = Vec::new();
    for (ticket, assistant, pending) in completions {
        assert_eq!(pending.wait(deadline()).unwrap(), Decision::Completed { run_id: ticket.run_id });
        completed_runs.push((ticket.run_id, assistant));
    }
    for retry in retries {
        assert_eq!(retry.wait(deadline()).unwrap(), Decision::Duplicate { run_id: retired.run_id, state: RunState::Completed });
    }
    let permit = worker(&handle, request(id(5), 5, "fifth", "hi"));
    let ticket = permit.ticket();
    let collision = completion(ticket, 16, "fake: hi").outcome;
    assert_eq!(permit.submit(collision, deadline()).unwrap().wait(deadline()).unwrap(),
        Decision::Failed { run_id: ticket.run_id, reason: yokai_types::FakeFailure::InvalidResponse });
    accepted(submit(&handle, request(id(5), 7, "after-collision", "hi")));
    let root = instance.root().to_owned();
    instance.close().unwrap();
    let conn = rusqlite::Connection::open_with_flags(root.join("instance.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    for (run_id, assistant) in completed_runs {
        let (status, active, message_id, reply): (String, i64, Vec<u8>, String) = conn.query_row(
            "SELECT r.status, cr.active, m.message_id, m.body FROM runs r
             JOIN conversation_runs cr USING (run_id) JOIN conversation_messages m USING (run_id)
             WHERE r.run_id = ?1 AND m.role = 'assistant'",
            [run_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ).unwrap_or_else(|error| panic!("read durable completion for run {run_id}: {error}"));
        assert_eq!((status.as_str(), active, message_id.as_slice(), reply.as_str()),
            ("Completed", 0, assistant.as_bytes().as_slice(), "fake: hello"), "durable completion for run {run_id}");
    }
}

fn writer_wakes_for_both_queues() {
    let instance = fixture("writer-wakeup", 4);
    let handle = instance.store();
    let conversation = id(1);
    create(&handle, conversation, "create");
    let permit = worker(&handle, request(conversation, 2, "submit", "hi"));
    let ticket = permit.ticket();

    store::test_api::observe_writer_idle(&instance).unwrap();
    let operation_deadline = Instant::now() + Duration::from_millis(500);
    handle.submit_health_check(operation_deadline).unwrap().wait(deadline()).unwrap();

    store::test_api::observe_writer_idle(&instance).unwrap();
    let operation_deadline = Instant::now() + Duration::from_millis(500);
    let done = permit.submit(completion(ticket, 3, "fake: hi").outcome, operation_deadline)
        .unwrap().wait(deadline()).unwrap();
    assert_eq!(done, Decision::Completed { run_id: ticket.run_id });
    instance.close().unwrap();
}

fn wait_timeout() {
    let instance = fixture("timeout", 64);
    let handle = instance.store();
    let conversation = id(1);
    create(&handle, conversation, "create");
    let request = request(conversation, 2, "submit", "hi");
    let pending = store::test_api::submit_message_fault(&instance, request.clone(), deadline(),
        store::test_api::WriteFault::DelayBeforeCommit(Duration::from_millis(150))).unwrap();
    let error = pending.wait(Instant::now()).unwrap_err();
    assert_eq!((error.certainty, error.reason), (Certainty::OutcomeUnknown, WriteReason::WaitExpired));
    assert!(matches!(submit(&handle, request), Decision::Duplicate { state: RunState::Open, .. }));
    instance.close().unwrap();
}

fn replay_pages() {
    let instance = fixture("replay", 64);
    let handle = instance.store();
    let iid = instance.id();
    let large = id(1);
    let many = id(2);
    create(&handle, large, "create-large");
    create(&handle, many, "create-many");

    let message = |number: u128| MessageId::from_bytes(number.to_be_bytes());
    for (conversation, start, count, body) in [
        (large, 0, 5, "🦊".repeat(yokai_types::MAX_RESPONSE_BYTES / 4)),
        (many, 1000, 130, "a".into()),
    ] {
        for n in 0..count {
            let number = start + n * 2;
            let request = SubmitMessage::new(command(&format!("page-{number}")), conversation, message(number), "u").unwrap();
            let ticket = accepted(submit(&handle, request));
            let response = AssistantResponse::new(message(number + 1), &body).unwrap();
            assert_eq!(complete(&handle, CompleteFakeRun { ticket, outcome: FakeOutcome::Response(response) }),
                Decision::Completed { run_id: ticket.run_id });
        }
        let mut cursor = ReplayCursor::start(iid, conversation);
        let mut events = Vec::new();
        let mut sizes = Vec::new();
        loop {
            let page = handle.replay_events(conversation, cursor, yokai_types::MAX_REPLAY_EVENTS, deadline())
                .unwrap().wait(deadline()).unwrap();
            assert!(!page.events.is_empty());
            assert!(page.events.iter().map(|event| event.replay_bytes()).sum::<usize>() <= yokai_types::MAX_REPLAY_BYTES);
            assert_eq!(page.cursor.after, page.events.last().unwrap().sequence);
            assert!(page.cursor.after > cursor.after);
            sizes.push(page.events.len());
            events.extend(page.events);
            cursor = page.cursor;
            if !page.has_more { break; }
        }
        assert_eq!(events.len(), 1 + 2 * count as usize);
        assert!(events.windows(2).all(|pair| pair[0].sequence < pair[1].sequence));
        assert_eq!(events[0].kind, ConversationEventKind::Created);
        for (n, pair) in events[1..].chunks_exact(2).enumerate() {
            let number = start + n as u128 * 2;
            assert_eq!(pair[0].run_id, pair[1].run_id);
            assert!(matches!(&pair[0].kind, ConversationEventKind::Submitted(value) if value.id == message(number) && value.text == "u"));
            assert!(matches!(&pair[1].kind, ConversationEventKind::Completed(value) if value.id == message(number+1) && value.text == body));
        }
        if conversation == large {
            assert_eq!(sizes, [8, 3]);
        } else {
            assert_eq!(sizes, [256, 5]);
        }
        let empty = handle.replay_events(conversation, cursor, 1, deadline()).unwrap().wait(deadline()).unwrap();
        assert!(empty.events.is_empty() && !empty.has_more && empty.cursor == cursor);
    }

    let cursor = ReplayCursor::start(iid, large);
    let wrong_instance = ReplayCursor { instance_id: yokai_types::InstanceId::from_bytes([0; 16]), ..cursor };
    assert_eq!(handle.replay_events(large, wrong_instance, 1, deadline()).err(), Some(store::ReadError::InvalidCursor));
    assert_eq!(handle.replay_events(many, cursor, 1, deadline()).err(), Some(store::ReadError::InvalidCursor));
    assert_eq!(handle.replay_events(large, cursor, 0, deadline()).err(), Some(store::ReadError::Invalid));
    assert_eq!(handle.replay_events(large, cursor, yokai_types::MAX_REPLAY_EVENTS + 1, deadline()).err(), Some(store::ReadError::Invalid));
    let page = handle.replay_events(many, ReplayCursor::start(iid, many), 1, deadline()).unwrap().wait(deadline()).unwrap();
    for after in [page.cursor.after, i64::MAX] {
        let future_or_foreign = ReplayCursor { after, ..cursor };
        let result = handle.replay_events(large, future_or_foreign, 1, deadline()).unwrap().wait(deadline());
        assert_eq!(result, Err(store::ReadError::InvalidCursor));
    }
    instance.close().unwrap();
}

fn main() -> ExitCode {
    let protection = yokai_testsupport::make_non_dumpable();
    println!("{}", protection.line());
    if !protection.permits_checks() { return ExitCode::FAILURE; }
    let mut failed = false;
    for (id, check) in [("M1-ADMIT", admission as fn()), ("M1-RACE", concurrency), ("M1-CERTAINTY", certainty), ("M1-COMPLETION-CAPACITY", completion_capacity), ("M1-WRITER-WAKEUP", writer_wakes_for_both_queues), ("M1-TIMEOUT", wait_timeout), ("M1-REPLAY", replay_pages)] {
        if std::panic::catch_unwind(check).is_err() {
            println!("CHECK {id} FAIL");
            failed = true;
        } else {
            println!("CHECK {id} PASS");
        }
    }
    if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}
