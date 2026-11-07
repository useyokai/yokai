//! Headless fake streaming, bounded shutdown and logical restart.
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Barrier, mpsc};
use std::time::{Duration, Instant};
use yokai_exec::{ConversationClient, ConversationRuntime, Stream, StreamError, Submission, test_api::TestFake};
use yokai_store as store;
use yokai_types::{CommandId, ConversationDecision as Decision, ConversationEventKind as Event,
    ConversationId, CreateConversation, FakeFailure, MAX_CHUNK_BYTES, MAX_RESPONSE_BYTES,
    MAX_USER_BYTES, MessageId, PROGRESS_CAPACITY, ReplayCursor, RunState, RunTicket, SubmitMessage};

fn deadline() -> Instant { Instant::now() + Duration::from_secs(10) }
fn id(n: u8) -> ConversationId { ConversationId::from_bytes([n; 16]) }
fn mid(n: u8) -> MessageId { MessageId::from_bytes([n; 16]) }
fn command(name: &str) -> CommandId { CommandId::new(name).unwrap() }

fn instance(name: &str) -> store::Instance {
    let base = PathBuf::from(std::env::var_os("YOKAI_TEST_DATA_DIR").expect("fresh fixture root"));
    assert!(base.is_absolute() && base.is_dir());
    let parent = base.join(name);
    std::fs::create_dir(&parent).unwrap();
    store::create_instance(&parent).unwrap()
}

fn create(client: &ConversationClient, n: u8) {
    let request = CreateConversation { command_id: command(&format!("create-{n}")), conversation_id: id(n) };
    assert!(matches!(client.create_conversation(request, deadline()).unwrap(), Decision::Created { .. }));
}

fn request(n: u8, message: u8, text: &str) -> SubmitMessage {
    SubmitMessage::new(command(&format!("submit-{message}")), id(n), mid(message), text).unwrap()
}

fn streaming(submission: Submission) -> (RunTicket, Stream) {
    match submission {
        Submission::Streaming { ticket, stream } => (ticket, stream),
        _ => panic!("expected confirmed acceptance and stream"),
    }
}

fn chunks(progress: &mpsc::Receiver<yokai_exec::Chunk>) -> Vec<yokai_exec::Chunk> {
    let end = deadline();
    let mut result = Vec::new();
    loop {
        match progress.recv_timeout(end.saturating_duration_since(Instant::now())) {
            Ok(chunk) => {
                assert_eq!(chunk.index, result.len());
                assert!(!chunk.text.is_empty() && chunk.text.len() <= MAX_CHUNK_BYTES);
                result.push(chunk);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return result,
            Err(mpsc::RecvTimeoutError::Timeout) => panic!("chunk wait expired"),
        }
    }
}

fn duplicates_and_persistence() {
    let instance = instance("duplicates");
    let root = instance.root().to_owned();
    let iid = instance.id();
    let (fake, pause) = TestFake::paused();
    let mut runtime = ConversationRuntime::with_test_fake(instance, fake.clone());
    let client = runtime.handle();
    create(&client, 1);
    let text = "a".repeat(MAX_CHUNK_BYTES - 7) + "🦊é";
    let request = request(1, 1, &text);
    let barrier = Arc::new(Barrier::new(8));
    let submissions = std::thread::scope(|scope| {
        let threads: Vec<_> = (0..8).map(|_| {
            let client = client.clone();
            let request = request.clone();
            let barrier = barrier.clone();
            scope.spawn(move || {
                barrier.wait();
                client.submit_message(request, mid(2), deadline()).unwrap()
            })
        }).collect();
        threads.into_iter().map(|thread| thread.join().unwrap()).collect::<Vec<_>>()
    });
    let mut accepted = None;
    let mut duplicates = Vec::new();
    for submission in submissions {
        match submission {
            Submission::Streaming { ticket, stream } => {
                assert!(accepted.replace((ticket, stream)).is_none());
            }
            Submission::Decision(Decision::Duplicate { run_id, state: RunState::Open }) => duplicates.push(run_id),
            _ => panic!("unexpected submission result"),
        }
    }
    let (ticket, stream) = accepted.unwrap();
    assert_eq!(duplicates, [ticket.run_id; 7]);
    assert_eq!(fake.invocations(), 0);
    assert!(matches!(client.read_conversation(id(1), deadline()).unwrap().unwrap().latest_run, Some((_, RunState::Open))));
    pause.resume();
    let progress = chunks(&stream.progress);
    assert_eq!(progress[0].text.len(), MAX_CHUNK_BYTES - 1);
    assert_eq!(progress[1].text, "🦊é");
    let reply: String = progress.iter().map(|chunk| chunk.text.as_str()).collect();
    assert_eq!(reply, format!("fake: {text}"));
    assert_eq!(stream.completion.wait(deadline()).unwrap(), Decision::Completed { run_id: ticket.run_id });
    assert_eq!(fake.invocations(), 1);
    let cursor = ReplayCursor::start(iid, id(1));
    let first = client.replay_events(id(1), cursor, 20, deadline()).unwrap();
    assert_eq!(first.events.len(), 3);
    assert!(matches!(&first.events[1].kind, Event::Submitted(message) if message.id == mid(1) && message.text == text));
    assert!(matches!(&first.events[2].kind, Event::Completed(message) if message.id == mid(2) && message.text == reply));
    runtime.close(deadline()).unwrap();
    let mut reopened = ConversationRuntime::with_test_fake(store::open_instance(&root).unwrap(), fake.clone());
    let client = reopened.handle();
    assert_eq!(client.replay_events(id(1), cursor, 20, deadline()).unwrap(), first);
    assert!(matches!(client.submit_message(request, mid(2), deadline()).unwrap(),
        Submission::Decision(Decision::Duplicate { run_id, state: RunState::Completed }) if run_id == ticket.run_id));
    assert_eq!(fake.invocations(), 1);
    reopened.close(deadline()).unwrap();
}

fn terminal_failure(client: &ConversationClient, instance_id: yokai_types::InstanceId, ticket: RunTicket, expected: FakeFailure) {
    let events = client.replay_events(ticket.conversation_id, ReplayCursor::start(instance_id, ticket.conversation_id), 20, deadline()).unwrap();
    assert_eq!(events.events.len(), 3);
    assert_eq!(events.events[2].kind, Event::Failed(expected));
    assert_eq!(client.read_conversation(ticket.conversation_id, deadline()).unwrap().unwrap().latest_run, Some((ticket.run_id, RunState::Failed)));
}

fn bounded_progress() {
    let instance = instance("full");
    let iid = instance.id();
    let mut runtime = ConversationRuntime::new(instance);
    let client = runtime.handle();
    create(&client, 1);
    let (ticket, stream) = streaming(client.submit_message(request(1, 1, &"x".repeat(MAX_USER_BYTES)), mid(2), deadline()).unwrap());
    assert_eq!(stream.completion.wait(deadline()).unwrap(), Decision::Failed { run_id: ticket.run_id, reason: FakeFailure::ProgressFull });
    assert_eq!(chunks(&stream.progress).len(), PROGRESS_CAPACITY);
    terminal_failure(&client, iid, ticket, FakeFailure::ProgressFull);
    runtime.close(deadline()).unwrap();
    assert!(TestFake::with_reply(&"x".repeat(MAX_RESPONSE_BYTES + 1)).is_err());
}

fn adapter_and_disconnect() {
    let instance = instance("adapter");
    let iid = instance.id();
    let mut runtime = ConversationRuntime::with_test_fake(instance, TestFake::failing_after(2));
    let client = runtime.handle();
    create(&client, 1);
    let (ticket, stream) = streaming(client.submit_message(request(1, 1, &"x".repeat(3 * MAX_CHUNK_BYTES)), mid(2), deadline()).unwrap());
    assert_eq!(chunks(&stream.progress).len(), 2);
    assert_eq!(stream.completion.wait(deadline()).unwrap(), Decision::Failed { run_id: ticket.run_id, reason: FakeFailure::Adapter });
    terminal_failure(&client, iid, ticket, FakeFailure::Adapter);
    runtime.close(deadline()).unwrap();

    let other = self::instance("disconnect");
    let iid = other.id();
    let (fake, pause) = TestFake::paused();
    let mut runtime = ConversationRuntime::with_test_fake(other, fake);
    let client = runtime.handle();
    create(&client, 1);
    let (ticket, Stream { progress, completion }) = streaming(client.submit_message(request(1, 1, "x"), mid(2), deadline()).unwrap());
    drop(progress);
    pause.resume();
    assert_eq!(completion.wait(deadline()).unwrap(), Decision::Failed { run_id: ticket.run_id, reason: FakeFailure::ProgressDisconnected });
    terminal_failure(&client, iid, ticket, FakeFailure::ProgressDisconnected);
    runtime.close(deadline()).unwrap();
}

fn hold_and_reopen() {
    let instance = instance("interruption");
    let root = instance.root().to_owned();
    let iid = instance.id();
    let (fake, pause) = TestFake::paused();
    let mut runtime = ConversationRuntime::with_test_fake(instance, fake.clone());
    let client = runtime.handle();
    create(&client, 1);
    let submission = request(1, 1, "first");
    let (ticket, stream) = streaming(client.submit_message(submission.clone(), mid(2), deadline()).unwrap());
    assert!(matches!(runtime.close(Instant::now()), Err(store::InstanceError::CloseIncomplete)));
    assert!(matches!(store::open_instance(&root), Err(store::InstanceError::Busy(_))));
    pause.interrupt();
    assert_eq!(stream.completion.wait(deadline()), Err(StreamError::WorkerGone));
    assert_eq!(fake.invocations(), 0);
    runtime.close(deadline()).unwrap();

    let mut reopened = ConversationRuntime::with_test_fake(store::open_instance(&root).unwrap(), fake.clone());
    let client = reopened.handle();
    assert_eq!(client.read_conversation(id(1), deadline()).unwrap().unwrap().latest_run, Some((ticket.run_id, RunState::Held)));
    assert!(matches!(client.submit_message(submission, mid(2), deadline()).unwrap(),
        Submission::Decision(Decision::Duplicate { state: RunState::Held, .. })));
    assert!(matches!(client.submit_message(request(1, 3, "next"), mid(4), deadline()).unwrap(), Submission::Decision(Decision::Busy)));
    let page = client.replay_events(id(1), ReplayCursor::start(iid, id(1)), 20, deadline()).unwrap();
    assert_eq!(page.events.last().unwrap().kind, Event::Held);
    assert_eq!(fake.invocations(), 0);
    reopened.close(deadline()).unwrap();
}

fn main() -> ExitCode {
    let protection = yokai_testsupport::make_non_dumpable();
    println!("{}", protection.line());
    if !protection.permits_checks() { return ExitCode::FAILURE; }
    for (id, check) in [
        ("M1-STREAM-PERSIST", duplicates_and_persistence as fn()),
        ("M1-STREAM-BOUND", bounded_progress),
        ("M1-STREAM-FAULT", adapter_and_disconnect),
        ("M1-STREAM-HELD", hold_and_reopen),
    ] {
        if std::panic::catch_unwind(check).is_err() {
            println!("CHECK {id} FAIL");
            return ExitCode::FAILURE;
        }
        println!("CHECK {id} PASS");
    }
    ExitCode::SUCCESS
}
