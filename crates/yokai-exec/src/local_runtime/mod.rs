//! Host one synthetic Instance and its bounded local connection.
mod service;
mod ingress;
#[cfg(test)]
mod relay_tests;
#[cfg(feature = "test-api")]
pub mod test_api;

use crate::{ConversationClient, ConversationRuntime, Stream, StreamError};
use service::{Action, dispatch, result_reply, store_status};
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use yokai_ipc::{
    Binding, CONTROL_MAX, Endpoint, Enrollment, Epoch, Frame, Hello, RESPONSE_MAX,
    Ready, Reply, Request, VERSION, invalid, nonce, publish_endpoint, read_frame,
    write_frame,
};
use yokai_platform::local_ipc::{ImagePin, IoError, Listener, Peer, Pipe, PipeName, current_identity};
use yokai_store::Instance;
use yokai_types::MAX_ACTIVE_RUNS;

type Notice = (u64, Reply);

struct Delivery {
    progress: SyncSender<Notice>,
    terminal: SyncSender<Notice>,
    detached: AtomicBool,
}

impl Delivery {
    fn send(&self, sender: &SyncSender<Notice>, notice: Notice) {
        if !self.detached.load(Ordering::Acquire) && sender.try_send(notice).is_err() {
            #[cfg(feature = "test-api")]
            test_api::event(if std::ptr::eq(sender, &self.progress) { "progress-queue-full-16" } else { "terminal-queue-full-1" });
            self.detached.store(true, Ordering::Release);
        }
    }
}

fn relay(stream: Stream, request: u64, run: i64, delivery: Arc<Delivery>, deadline: Instant) {
    while let Ok(chunk) = stream.progress.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        delivery.send(&delivery.progress, (request, Reply::Progress { run, index: chunk.index, text: chunk.text }));
    }
    let reply = match stream.completion.wait(deadline) {
        Ok(decision) => service::decision_reply(decision),
        Err(StreamError::Store(error)) => service::write_error(error),
        Err(StreamError::WaitExpired) => Reply::error("completion_wait_expired"),
        Err(StreamError::WorkerGone) => Reply::error("completion_unconfirmed"),
    };
    #[cfg(feature = "test-api")]
    if test_api::mode("loss") { test_api::gate("terminal-paused", deadline); }
    delivery.send(&delivery.terminal, (request, reply));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RelaySite {
    Idle,
    ActiveLoop,
    ActiveAdmission,
    Drain,
}

impl RelaySite {
    fn reason(self) -> &'static str {
        match self {
            Self::Idle => "progress relay failed",
            Self::ActiveLoop | Self::ActiveAdmission => "progress relay failed during active session",
            Self::Drain => "progress relay failed during drain",
        }
    }

    #[cfg(feature = "test-api")]
    fn record_active_fault(self) {
        if test_api::mode("loss") {
            match self {
                Self::ActiveLoop => test_api::event("relay-fault-active-loop"),
                Self::ActiveAdmission => test_api::event("relay-fault-active-admission"),
                Self::Idle | Self::Drain => {}
            }
        }
    }
}

struct Relays {
    workers: Vec<JoinHandle<()>>,
    failed: Option<RelaySite>,
}

impl Relays {
    fn new() -> Self {
        Self { workers: Vec::with_capacity(MAX_ACTIVE_RUNS), failed: None }
    }

    fn reap(&mut self, site: RelaySite) -> Result<(), RelaySite> {
        if let Some(failure) = self.failed {
            return Err(failure);
        }
        let mut index = 0;
        while index < self.workers.len() {
            if self.workers[index].is_finished() {
                if self.workers.swap_remove(index).join().is_err() {
                    self.failed = Some(site);
                    #[cfg(feature = "test-api")]
                    site.record_active_fault();
                    return Err(site);
                }
            } else {
                index += 1;
            }
        }
        Ok(())
    }

    fn session_retention_reason<T>(&self, result: &Result<T, IoError>) -> Option<&'static str> {
        if matches!(result, Err(IoError::PendingRetained)) {
            return Some("session I/O unconfirmed");
        }
        // A joined reader may return an ordinary cleanup error over the body error.
        self.failed.map(RelaySite::reason)
    }
}

struct Session<'a> {
    pipe: &'a Pipe,
    peer: Arc<Peer>,
    binding: Binding,
    outgoing: u64,
    incoming: u64,
    delivery: Arc<Delivery>,
    progress: Receiver<Notice>,
    terminal: Receiver<Notice>,
}

impl<'a> Session<'a> {
    fn open(pipe: &'a Pipe, peer: Peer, epoch: &Epoch) -> Result<Self, IoError> {
        let deadline = Instant::now() + Duration::from_secs(3);
        let hello: Hello = read_frame(pipe, CONTROL_MAX, deadline, deadline)?;
        if hello.epoch != *epoch {
            return Err(invalid("stale hello").into());
        }
        PipeName::from_nonce(&hello.client_nonce)?;
        peer.check_alive()?;
        let session = nonce()?;
        let binding = Binding { epoch: epoch.clone(), session: session.clone() };
        write_frame(pipe, &Ready { hello, session }, CONTROL_MAX, deadline)?;
        let (progress_tx, progress) = mpsc::sync_channel(16);
        let (terminal_tx, terminal) = mpsc::sync_channel(1);
        let delivery = Arc::new(Delivery { progress: progress_tx, terminal: terminal_tx, detached: AtomicBool::new(false) });
        Ok(Self { pipe, peer: Arc::new(peer), binding, outgoing: 1, incoming: 1, delivery, progress, terminal })
    }

    fn reply(&mut self, request: u64, body: Reply) -> Result<(), IoError> {
        self.reply_before(request, body, Instant::now() + Duration::from_secs(2))
    }

    fn reply_before(&mut self, request: u64, body: Reply, end: Instant) -> Result<(), IoError> {
        let sequence = self.outgoing;
        self.outgoing = sequence.checked_add(1).ok_or_else(|| invalid("sequence exhausted"))?;
        let frame = Frame { binding: self.binding.clone(), sequence, request, body };
        write_frame(self.pipe, &frame, RESPONSE_MAX, end.min(Instant::now() + Duration::from_secs(2)))
    }

    fn flush_notices(&mut self, end: Instant) -> Result<(), IoError> {
        let terminal = self.terminal.try_recv().ok();
        // Every relay enqueues progress before its terminal notice. Bound work per turn.
        for _ in 0..16 {
            let Ok((request, notice)) = self.progress.try_recv() else { break };
            self.reply_before(request, notice, end)?;
        }
        if let Some((request, notice)) = terminal {
            self.reply_before(request, notice, end)?;
        }
        Ok(())
    }

    fn serve(&mut self, client: &ConversationClient, relays: &mut Relays, draining: bool, owner: &ConversationRuntime) -> Result<(u64, Instant), IoError> {
        let peer = self.peer.clone();
        let delivery = self.delivery.clone();
        ingress::with_reader(self.pipe, &peer, self.binding.clone(), self.incoming, &delivery.detached,
                             |reader| self.serve_inner(client, relays, reader, draining, owner))
    }

    fn serve_inner(&mut self, client: &ConversationClient, relays: &mut Relays,
                   reader: &ingress::Ingress<'_, '_>, draining: bool, _owner: &ConversationRuntime) -> Result<(u64, Instant), IoError> {
        let idle_duration = Duration::from_secs(30);
        #[cfg(feature = "test-api")]
        let idle_duration = if test_api::mode("backpressure") { Duration::from_millis(300) } else { idle_duration };
        let mut idle = Instant::now() + idle_duration;
        #[cfg(feature = "test-api")]
        if test_api::mode("loss") { test_api::event("relay-active-session"); }
        loop {
            relays.reap(RelaySite::ActiveLoop).map_err(|site| io::Error::other(site.reason()))?;
            reader.check()?;
            if self.delivery.detached.load(Ordering::Acquire) {
                return Err(io::Error::other("subscriber backpressure").into());
            }
            self.flush_notices(Instant::now() + Duration::from_secs(2))?;
            if Instant::now() >= idle {
                return Err(IoError::Deadline);
            }
            let Some(frame) = reader.next()? else {
                thread::sleep(Duration::from_millis(1));
                continue;
            };
            self.peer.check_alive()?;
            if frame.binding != self.binding || frame.sequence != self.incoming || frame.request != frame.sequence {
                return Err(invalid("stale or invalid request binding").into());
            }
            self.incoming = self.incoming.checked_add(1).ok_or_else(|| invalid("sequence exhausted"))?;
            idle = Instant::now() + idle_duration;
            let request = frame.request;
            let deadline = Instant::now() + Duration::from_secs(10);
            if draining && !matches!(frame.body, Request::Shutdown) {
                let reply = if matches!(frame.body, Request::Status) {
                    Reply::Status { lifecycle: "close_incomplete".into(), store: store_status(client.store_state()).into() }
                } else {
                    Reply::error("shutting_down")
                };
                self.reply(request, reply)?;
                continue;
            }
            #[cfg(feature = "test-api")]
            if let Some(Action::Reply(reply)) = test_api::fault_reply(_owner, &frame.body, deadline) {
                self.reply(request, reply)?;
                continue;
            }
            match dispatch(client, frame.body, deadline) {
                Action::Reply(body) => self.reply(request, body)?,
                Action::Stop => {
                    #[cfg(feature = "test-api")]
                    let duration = test_api::shutdown_duration();
                    #[cfg(not(feature = "test-api"))]
                    let duration = Duration::from_secs(15);
                    return Ok((request, Instant::now() + duration));
                },
                Action::Stream { stream, run } => {
                    while relays.workers.len() >= MAX_ACTIVE_RUNS {
                        relays.reap(RelaySite::ActiveAdmission).map_err(|site| io::Error::other(site.reason()))?;
                        if Instant::now() >= deadline {
                            return Err(io::Error::other("relay admission unconfirmed").into());
                        }
                        thread::sleep(Duration::from_millis(1));
                    }
                    let delivery = self.delivery.clone();
                    relays.workers.push(thread::Builder::new().name("yokai-progress".into())
                        .spawn(move || relay(stream, request, run, delivery, deadline))?);
                    self.reply(request, result_reply("accepted", Some(run), Some(yokai_ipc::RunStatus::Open)))?;
                    #[cfg(feature = "test-api")]
                    if test_api::mode("backpressure") { test_api::gate("delivery-paused", deadline); }
                }
            }
        }
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        self.delivery.detached.store(true, Ordering::Release);
    }
}

fn retain(reason: &str) -> ! {
    eprintln!("LIFECYCLE CloseIncomplete {reason}");
    loop {
        thread::park();
    }
}

pub fn run(instance: Instance, control: &Path, config: &Enrollment) -> Result<(), Box<dyn std::error::Error>> {
    let root = instance.root().to_owned();
    let epoch_nonce = nonce()?;
    #[cfg(feature = "test-api")]
    let epoch_nonce = test_api::pipe_nonce().unwrap_or(epoch_nonce);
    let epoch = Epoch {
        version: VERSION,
        instance: instance.id().to_string(),
        runtime_epoch: instance.startup_report().ok_or_else(|| invalid("missing startup epoch"))?.epoch,
        nonce: epoch_nonce,
    };
    #[cfg(feature = "test-api")]
    test_api::bind(control, &epoch)?;
    let mut owner = ConversationRuntime::new(instance);
    let pin = ImagePin::open(&config.peer_image, &config.peer_sha256)?;
    if !config.instance.is_empty() && config.instance != epoch.instance {
        return Err(invalid("unexpected Instance identity").into());
    }
    let listener = Listener::new(&PipeName::from_nonce(&epoch.nonce)?)?;
    let process = current_identity()?;
    let endpoint = Endpoint {
        epoch: epoch.clone(), pid: process.pid, creation_time: process.creation_time,
        build: env!("CARGO_PKG_VERSION").into(),
    };
    publish_endpoint(control, &endpoint)?;
    println!("LIFECYCLE Running {}", root.display());
    let client = owner.handle();
    let mut relays = Relays::new();
    let mut draining = false;
    loop {
        if let Err(site) = relays.reap(RelaySite::Idle) {
            retain(site.reason());
        }
        match listener.accept(Instant::now() + Duration::from_secs(2)) {
            Ok(()) => {}
            Err(IoError::Deadline) => continue,
            Err(IoError::PendingRetained) => retain("accept completion unknown"),
            Err(error) => {
                if draining { retain("accept failure after drain began"); }
                if owner.close(Instant::now() + Duration::from_secs(15)).is_err() {
                    retain("store close after accept failure");
                }
                return Err(error.into());
            }
        }
        let session = listener.authenticate(&pin).map_err(IoError::from)
            .and_then(|peer| Session::open(listener.pipe(), peer, &epoch));
        match session {
            Ok(mut session) => {
                let result = session.serve(&client, &mut relays, draining, &owner);
                // serve keeps ingress ownership until its reader is safely joined.
                if let Some(reason) = relays.session_retention_reason(&result) {
                    retain(reason);
                }
                if let Ok((request, deadline)) = result {
                    draining = true;
                    owner.begin_draining();
                    println!("LIFECYCLE Draining");
                    #[cfg(feature = "test-api")]
                    test_api::event("draining");
                    if session.reply_before(request, Reply::ShutdownAccepted, deadline).is_err() {
                        session.delivery.detached.store(true, Ordering::Release);
                    }
                    if listener.pipe().retained() {
                        retain("shutdown reply I/O unconfirmed");
                    }
                    while !relays.workers.is_empty() {
                        if let Err(site) = relays.reap(RelaySite::Drain) {
                            retain(site.reason());
                        }
                        if Instant::now() >= deadline { break; }
                        if !session.delivery.detached.load(Ordering::Acquire) {
                            if session.flush_notices(deadline).is_err() {
                                session.delivery.detached.store(true, Ordering::Release);
                            }
                            if listener.pipe().retained() {
                                retain("drain reply I/O unconfirmed");
                            }
                        }
                        thread::sleep(Duration::from_millis(1));
                    }
                    let closed = if relays.workers.is_empty() {
                        match owner.close(deadline) {
                            Ok(()) => true,
                            Err(_) if owner.drain_retryable() => false,
                            Err(_) => retain("store close or worker ownership incomplete"),
                        }
                    } else {
                        false
                    };
                    if closed {
                        // Preserve the stale locator; another epoch may already own the store.
                        let _ = session.reply_before(request, Reply::Closed, deadline);
                        if listener.pipe().retained() {
                            retain("final pipe I/O unconfirmed after store close");
                        }
                        println!("LIFECYCLE Closed");
                        return Ok(());
                    }
                    // The ingress reader has joined before serve returns. Retain the
                    // unconsumed Instance, disconnect, then accept only Status/Shutdown.
                    println!("LIFECYCLE CloseIncomplete pre-store drain");
                    #[cfg(feature = "test-api")]
                    test_api::event("close-incomplete");

                }
            }
            Err(IoError::PendingRetained) => retain("handshake I/O unconfirmed"),
            Err(_) => eprintln!("IPC peer or handshake refused"),
        }
        match listener.disconnect() {
            Ok(()) => {}
            Err(IoError::PendingRetained) => retain("disconnect ownership unconfirmed"),
            Err(IoError::System(error)) if error.raw_os_error() == Some(233) => {}
            Err(_) => retain("disconnect failed"),
        }
    }
}
