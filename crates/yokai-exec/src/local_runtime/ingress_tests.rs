use super::*;
use yokai_ipc::Epoch;

fn binding() -> Binding {
    Binding { epoch: Epoch { version: 1, instance: "instance".into(), runtime_epoch: 1, nonce: "epoch".into() },
              session: "session".into() }
}
fn request(sequence: u64) -> Frame<Request> {
    Frame { binding: binding(), sequence, request: sequence, body: Request::Status }
}

#[test]
fn one_queued_request_and_full_queue_refusal() {
    let detached = AtomicBool::new(false);
    let state = State::new(&detached);
    let (tx, rx) = mpsc::sync_channel(1);
    assert!(publish(&state, &tx, request(1)).unwrap());
    let error = publish(&state, &tx, request(2)).unwrap_err();
    assert!(matches!(&error, IoError::System(e) if e.kind() == io::ErrorKind::WouldBlock));
    state.fail(error);
    let ingress = Ingress { requests: rx, state: &state };
    assert!(ingress.next().is_err()); // queued work does not escape the first fault
    assert!(ingress.next().is_err()); // consuming the error cannot revive admission
    assert!(detached.load(Ordering::Acquire));
}

#[test]
fn stop_observed_before_enqueue() {
    let detached = AtomicBool::new(false);
    let state = State::new(&detached);
    let (tx, rx) = mpsc::sync_channel(1);
    state.stop.store(true, Ordering::Release);
    assert!(!publish(&state, &tx, request(1)).unwrap());
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn rejects_binding_sequence_and_request_before_admission() {
    let mut incoming = 1;
    let mut frame = request(1);
    frame.binding.session = "wrong".into();
    assert!(validate(&frame, &binding(), &mut incoming).is_err());
    assert_eq!(incoming, 1);
    assert!(validate(&request(2), &binding(), &mut incoming).is_err());
    frame = request(1);
    frame.request = 2;
    assert!(validate(&frame, &binding(), &mut incoming).is_err());
    assert_eq!(incoming, 1);
    validate(&request(1), &binding(), &mut incoming).unwrap();
    assert_eq!(incoming, 2);
}

#[test]
fn sequence_exhaustion_is_refused() {
    let mut incoming = u64::MAX;
    assert!(validate(&request(u64::MAX), &binding(), &mut incoming).is_err());
}

#[test]
fn shutdown_barrier_remains_deliverable_after_reader_finishes() {
    let detached = AtomicBool::new(false);
    let state = State::new(&detached);
    let (tx, rx) = mpsc::sync_channel(1);
    let mut frame = request(1);
    frame.body = Request::Shutdown;
    publish(&state, &tx, frame).unwrap();
    drop(tx);
    let ingress = Ingress { requests: rx, state: &state };
    assert!(matches!(ingress.next().unwrap().unwrap().body, Request::Shutdown));
    assert!(ingress.next().is_err()); // no phantom second barrier after completion
}

#[test]
fn first_ordinary_fault_is_preserved() {
    let detached = AtomicBool::new(false);
    let state = State::new(&detached);
    state.fail(IoError::Deadline);
    state.fail(io::Error::other("later ordinary fault").into());
    assert!(matches!(state.check(), Err(IoError::Deadline)));
}

#[test]
fn pending_retention_overrides_ordinary_fault_and_queued_work() {
    let detached = AtomicBool::new(false);
    let state = State::new(&detached);
    state.fail(IoError::Deadline);
    state.fail(IoError::PendingRetained);
    assert!(matches!(state.check(), Err(IoError::PendingRetained)));
    assert!(matches!(state.check(), Err(IoError::PendingRetained)));
}

#[test]
fn reader_panic_is_caught_and_requires_retention() {
    let detached = AtomicBool::new(false);
    let state = State::new(&detached);
    capture_reader(&state, || panic!("owned reader model"));
    assert!(state.panicked.load(Ordering::Acquire));
    assert!(detached.load(Ordering::Acquire));
    assert!(matches!(state.check(), Err(IoError::PendingRetained)));
}

#[test]
fn no_release_for_unfinished_unjoined_pending_or_panicked_owner() {
    assert!(!retention_required(true, true, false, false, false));
    for args in [(false,true,false,false,false), (true,false,false,false,false),
                 (true,true,true,false,false), (true,true,false,true,false),
                 (true,true,false,false,true)] {
        assert!(retention_required(args.0,args.1,args.2,args.3,args.4));
    }
}

#[test]
fn receive_and_completion_faults_remain_separate() {
    let detached = AtomicBool::new(false);
    let state = State::new(&detached);
    capture_reader(&state, || Err(IoError::Deadline));
    assert!(matches!(state.check(), Err(IoError::Deadline)));
    assert!(!state.pending.load(Ordering::Acquire));
    // Models do not create an unresolved native operation merely to exercise retention.
    assert!(retention_required(true, true, false, false, true));
}
