//! The Instance owner coordinates fake workers after confirmed durable admission.
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use yokai_store::{FakeAdmission, Instance, InstanceError, ReadError, StoreHandle, WorkerCompletion, WriteFailure};
use yokai_types::{
    ConversationDecision as Decision, ConversationId, ConversationState,
    CreateConversation, MAX_ACTIVE_RUNS, MessageId, PROGRESS_CAPACITY,
    ReplayCursor, ReplayPage, RunTicket, SubmitMessage,
};

use crate::fake::{Chunk, FakeModel};

pub struct ConversationRuntime {
    instance: Option<Instance>,
    client: ConversationClient,
    close_failed: bool,
}

#[derive(Default)]
struct Dispatch {
    closing: bool,
    panicked: bool,
    workers: Vec<JoinHandle<()>>,
}

impl Dispatch {
    fn reap(&mut self) {
        let mut index = 0;
        while index < self.workers.len() {
            if self.workers[index].is_finished() {
                self.panicked |= self.workers.swap_remove(index).join().is_err();
            } else {
                index += 1;
            }
        }
    }
}

#[derive(Clone)]
pub struct ConversationClient {
    store: StoreHandle,
    dispatch: Arc<Mutex<Dispatch>>,
    model: FakeModel,
}

impl ConversationRuntime {
    pub fn new(instance: Instance) -> Self {
        Self::with_model(instance, FakeModel::default())
    }

    #[cfg(feature = "test-api")]
    pub fn with_test_fake(instance: Instance, test: crate::test_api::TestFake) -> Self {
        Self::with_model(instance, FakeModel { test: Some(test) })
    }

    fn with_model(instance: Instance, model: FakeModel) -> Self {
        let client = ConversationClient {
            store: instance.store(),
            dispatch: Arc::new(Mutex::new(Dispatch::default())),
            model,
        };
        Self { instance: Some(instance), client, close_failed: false }
    }

    pub fn handle(&self) -> ConversationClient {
        self.client.clone()
    }

    /// Stop admission and join fake workers before closing the store. If a fake
    /// worker misses the deadline, this owner retains the Instance for a later close.
    pub fn close(&mut self, deadline: Instant) -> Result<(), InstanceError> {
        if self.close_failed {
            return Err(InstanceError::CloseIncomplete);
        }
        if self.instance.is_none() {
            return Ok(());
        }
        loop {
            let mut dispatch = self.client.dispatch();
            dispatch.closing = true;
            dispatch.reap();
            if dispatch.panicked || Instant::now() >= deadline {
                return Err(InstanceError::CloseIncomplete);
            }
            if dispatch.workers.is_empty() {
                break;
            }
            drop(dispatch);
            thread::sleep(Duration::from_millis(1).min(deadline.saturating_duration_since(Instant::now())));
        }
        let result = self.instance.take().expect("owner checked").close_before(deadline);
        self.close_failed = result.is_err();
        result
    }
}

impl Drop for ConversationRuntime {
    fn drop(&mut self) {
        if self.instance.is_some() && self.close(Instant::now() + Duration::from_secs(10)).is_err() {
            // A failed worker join cannot authorize releasing the database lock.
            if let Some(instance) = self.instance.take() {
                std::mem::forget(instance);
            }
        }
    }
}

#[derive(Debug)]
pub enum ClientError {
    Closing,
    Store(WriteFailure),
}
impl From<WriteFailure> for ClientError {
    fn from(error: WriteFailure) -> Self { Self::Store(error) }
}
impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closing => f.write_str("conversation runtime is closing"),
            Self::Store(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for ClientError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DispatchStop {
    Closing,
    Deadline,
    WorkerUnavailable,
}

pub enum Submission {
    Decision(Decision),
    Streaming { ticket: RunTicket, stream: Stream },
    /// Durable acceptance is confirmed. The fake did not start; reopening will
    /// hold the run. A repeat of the command does not dispatch it.
    AcceptedWithoutDispatch { ticket: RunTicket, reason: DispatchStop },
}

pub struct Stream {
    pub progress: mpsc::Receiver<Chunk>,
    pub completion: PendingCompletion,
}

pub struct PendingCompletion(mpsc::Receiver<Result<Decision, WriteFailure>>);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamError {
    Store(WriteFailure),
    WaitExpired,
    WorkerGone,
}
impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => error.fmt(f),
            Self::WaitExpired => f.write_str("completion wait expired; resolve from durable state"),
            Self::WorkerGone => f.write_str("worker ended without a confirmed completion"),
        }
    }
}
impl std::error::Error for StreamError {}

impl PendingCompletion {
    pub fn wait(self, deadline: Instant) -> Result<Decision, StreamError> {
        match self.0.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(result) => result.map_err(StreamError::Store),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(StreamError::WaitExpired),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(StreamError::WorkerGone),
        }
    }
}

impl ConversationClient {
    fn dispatch(&self) -> MutexGuard<'_, Dispatch> {
        self.dispatch.lock().unwrap_or_else(|error| error.into_inner())
    }

    pub fn create_conversation(&self, request: CreateConversation, deadline: Instant) -> Result<Decision, ClientError> {
        let gate = self.dispatch();
        if gate.closing || gate.panicked {
            return Err(ClientError::Closing);
        }
        let pending = self.store.create_conversation(request, deadline)?;
        drop(gate);
        Ok(pending.wait(deadline)?)
    }

    /// Use the same command and user message IDs when resolving uncertain
    /// acceptance. The assistant ID is used only for a newly admitted run.
    pub fn submit_message(&self, request: SubmitMessage, assistant_id: MessageId, deadline: Instant) -> Result<Submission, ClientError> {
        let gate = self.dispatch();
        if gate.closing || gate.panicked {
            return Err(ClientError::Closing);
        }
        let pending = self.store.submit_worker_message(request.clone(), deadline)?;
        drop(gate);
        Ok(match pending.wait(deadline)? {
            FakeAdmission::Accepted(worker) => self.start(worker, request, assistant_id, deadline),
            // Duplicate and refusal decisions carry no authority to start a fake.
            FakeAdmission::Decision(decision) => Submission::Decision(decision),
        })
    }

    fn start(&self, completion_permit: WorkerCompletion, request: SubmitMessage, assistant_id: MessageId, deadline: Instant) -> Submission {
        let ticket = completion_permit.ticket();
        let mut gate = loop {
            let mut gate = self.dispatch();
            gate.reap();
            if gate.closing || gate.panicked {
                return Submission::AcceptedWithoutDispatch { ticket, reason: DispatchStop::Closing };
            }
            if Instant::now() >= deadline {
                return Submission::AcceptedWithoutDispatch { ticket, reason: DispatchStop::Deadline };
            }
            if gate.workers.len() < MAX_ACTIVE_RUNS {
                break gate;
            }
            // The writer already reserved this run. A prior worker can briefly
            // remain alive after releasing its durable slot; join it before spawn.
            drop(gate);
            thread::sleep(Duration::from_millis(1).min(deadline.saturating_duration_since(Instant::now())));
        };

        let (chunks, progress) = mpsc::sync_channel(PROGRESS_CAPACITY);
        let (terminal, completion) = mpsc::sync_channel(1);
        let model = self.model.clone();
        let worker = thread::Builder::new().name(format!("yokai-fake-{}", ticket.run_id)).spawn(move || {
            let outcome = model.run(&request, assistant_id, &chunks, deadline);
            drop(chunks);
            let Some(outcome) = outcome else { return; };
            let result = completion_permit.submit(outcome, deadline).and_then(|pending| pending.wait(deadline));
            let _ = terminal.send(result);
        });
        match worker {
            Ok(worker) => {
                gate.workers.push(worker);
                Submission::Streaming { ticket, stream: Stream { progress, completion: PendingCompletion(completion) } }
            }
            Err(_) => Submission::AcceptedWithoutDispatch { ticket, reason: DispatchStop::WorkerUnavailable },
        }
    }

    pub fn read_conversation(&self, id: ConversationId, deadline: Instant) -> Result<Option<ConversationState>, ReadError> {
        self.store.read_conversation(id, deadline)?.wait(deadline)
    }

    pub fn replay_events(&self, id: ConversationId, cursor: ReplayCursor, limit: usize, deadline: Instant) -> Result<ReplayPage, ReadError> {
        self.store.replay_events(id, cursor, limit, deadline)?.wait(deadline)
    }
}
