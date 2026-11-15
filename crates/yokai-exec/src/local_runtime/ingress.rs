//! One bounded reader; the session owner remains the only dispatcher and writer.
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};
use yokai_ipc::{Binding, Frame, REQUEST_MAX, Request, invalid, read_frame};
use yokai_platform::local_ipc::{IoError, Peer, Pipe};

const JOIN_RESERVE: Duration = Duration::from_millis(2250);

struct State<'a> {
    stop: AtomicBool,
    pending: AtomicBool,
    panicked: AtomicBool,
    fault: Mutex<Option<IoError>>,
    faulted: AtomicBool,
    detached: &'a AtomicBool,
}

impl<'a> State<'a> {
    fn new(detached: &'a AtomicBool) -> Self {
        Self {
            stop: AtomicBool::new(false), pending: AtomicBool::new(false),
            panicked: AtomicBool::new(false), fault: Mutex::new(None), faulted: AtomicBool::new(false), detached,
        }
    }

    fn fail(&self, error: IoError) {
        if matches!(error, IoError::PendingRetained) {
            self.pending.store(true, Ordering::Release);
        }
        match self.fault.lock() {
            Ok(mut fault) => {
                if !self.faulted.load(Ordering::Acquire) { *fault = Some(error); }
                self.faulted.store(true, Ordering::Release);
            }
            Err(_) => { self.panicked.store(true, Ordering::Release); }
        }
        self.detached.store(true, Ordering::Release);
    }

    fn check(&self) -> Result<(), IoError> {
        if self.pending.load(Ordering::Acquire) || self.panicked.load(Ordering::Acquire) {
            return Err(IoError::PendingRetained);
        }
        match self.fault.lock() {
            Ok(mut fault) => match fault.take() {
                Some(error) => Err(error),
                None if self.faulted.load(Ordering::Acquire) => Err(io::Error::other("request reader failed").into()),
                None => Ok(()),
            },
            Err(_) => Err(IoError::PendingRetained),
        }
    }
}

fn validate(frame: &Frame<Request>, binding: &Binding, incoming: &mut u64) -> Result<(), IoError> {
    if frame.binding != *binding || frame.sequence != *incoming || frame.request != frame.sequence {
        return Err(invalid("stale or invalid request binding").into());
    }
    *incoming = incoming.checked_add(1).ok_or_else(|| invalid("sequence exhausted"))?;
    Ok(())
}

// Returning false means owner stop was observed; no new queue admission is made.
fn publish<T>(state: &State<'_>, sender: &SyncSender<T>, value: T) -> Result<bool, IoError> {
    if state.stop.load(Ordering::Acquire) { return Ok(false); }
    sender.try_send(value).map_err(|_| {
        #[cfg(feature = "test-api")]
        super::test_api::event("ingress-queue-full-1");
        IoError::System(io::Error::new(io::ErrorKind::WouldBlock, "request ingress backpressure"))
    })?;
    Ok(true)
}

pub(super) struct Ingress<'a, 'b> {
    requests: Receiver<Frame<Request>>,
    state: &'a State<'b>,
}

impl Ingress<'_, '_> {
    pub(super) fn check(&self) -> Result<(), IoError> { self.state.check() }

    pub(super) fn next(&self) -> Result<Option<Frame<Request>>, IoError> {
        self.check()?;
        match self.requests.try_recv() {
            Ok(frame) => {
                // A failure observed after dequeue still bars dispatch of this frame.
                self.check()?;
                Ok(Some(frame))
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(io::Error::other("request reader ended").into()),
        }
    }
}

fn read_loop(
    pipe: &Pipe, peer: &Peer, binding: &Binding, mut incoming: u64,
    sender: SyncSender<Frame<Request>>, state: &State<'_>,
) -> Result<(), IoError> {
    while !state.stop.load(Ordering::Acquire) {
        if pipe.available()? == 0 {
            thread::sleep(Duration::from_millis(1));
            continue;
        }
        if state.stop.load(Ordering::Acquire) { return Ok(()); }
        let deadline = Instant::now() + Duration::from_secs(2);
        let frame: Frame<Request> = read_frame(pipe, REQUEST_MAX, deadline, deadline)?;
        peer.check_alive()?;
        validate(&frame, binding, &mut incoming)?;
        let shutdown = matches!(frame.body, Request::Shutdown);
        if !publish(state, &sender, frame)? || shutdown {
            return Ok(());
        }
    }
    Ok(())
}

fn capture_reader(state: &State<'_>, read: impl FnOnce() -> Result<(), IoError>) {
    match catch_unwind(AssertUnwindSafe(read)) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => state.fail(error),
        Err(_) => {
            state.panicked.store(true, Ordering::Release);
            state.detached.store(true, Ordering::Release);
        }
    }
}

fn retention_required(
    finished: bool, joined: bool, pending: bool, panicked: bool, writer_pending: bool,
) -> bool {
    !finished || !joined || pending || panicked || writer_pending
}

#[cfg(feature = "test-api")]
fn trace_id() -> Option<usize> {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    (id < 32).then_some(id)
}
#[cfg(not(feature = "test-api"))]
fn trace_id() -> Option<usize> { None }

fn trace(id: Option<usize>, event: &str) {
    if let Some(id) = id { eprintln!("IPC_READER_{event} {id}"); }
}

pub(super) fn with_reader<T>(
    pipe: &Pipe, peer: &Peer, binding: Binding, incoming: u64, detached: &AtomicBool,
    body: impl FnOnce(&Ingress<'_, '_>) -> Result<T, IoError>,
) -> Result<T, IoError> {
    let state = State::new(detached);
    let (sender, requests) = mpsc::sync_channel(1);
    thread::scope(|scope| {
        let worker = thread::Builder::new().name("yokai-ingress".into()).spawn_scoped(
            scope, || capture_reader(&state, || read_loop(pipe, peer, &binding, incoming, sender, &state)),
        )?;
        let id = trace_id();
        trace(id, "STARTED");
        // Catch the owner unwind before scoped-thread cleanup could implicitly join.
        let outcome = catch_unwind(AssertUnwindSafe(|| body(&Ingress { requests, state: &state })));
        state.stop.store(true, Ordering::Release);
        let end = Instant::now() + JOIN_RESERVE;
        while !worker.is_finished() {
            if Instant::now() >= end { super::retain("request reader completion unconfirmed"); }
            thread::sleep(Duration::from_millis(1));
        }
        let joined = worker.join().is_ok();
        let writer_pending = matches!(&outcome, Ok(Err(IoError::PendingRetained)));
        if retention_required(true, joined, state.pending.load(Ordering::Acquire) || pipe.retained(),
                              state.panicked.load(Ordering::Acquire) || outcome.is_err(), writer_pending) {
            super::retain("request reader or session ownership unconfirmed");
        }
        trace(id, "JOINED");
        // An ordinary write failure must not hide a later reader PendingRetained result.
        state.check()?;
        match outcome {
            Ok(result) => result,
            Err(_) => super::retain("session owner panicked"),
        }
    })
}

#[cfg(test)]
#[path = "ingress_tests.rs"]
mod tests;
