use std::{path::PathBuf, sync::{Arc, Condvar, Mutex}, thread::{self, JoinHandle}, time::{Duration, Instant}};
use serde::{Deserialize, Serialize};
use yokai_desktop_contract::*;
use yokai_ipc::{Endpoint, Enrollment, Remote, Reply, Request, RunStatus};
use yokai_platform::local_ipc::IoError;
use crate::auth_diagnostics::{AuthJournal, AuthTicket};
use crate::{commands::Action, intent::{Phase, Store}, projection::{self, BridgeState, OperationFacts, Replay, Response, Ticket}};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeInput { pub control: PathBuf, pub enrollment: Enrollment }

#[derive(Clone, Default, Serialize)]
pub struct NativeFacts {
    pub shutdown_acknowledged: bool,
    pub runtime_closed_reply: bool,
    pub retained_io: bool,
    pub authentication: AuthJournal,
}

pub struct Bridge {
    // The host retains this after the worker joins, until native browser closure.
    _guard: Option<Arc<yokai_platform::InstanceLock>>,
    shared: Arc<(Mutex<BridgeState>, Condvar)>,
    worker: Mutex<Option<JoinHandle<()>>>,
    facts: Arc<Mutex<NativeFacts>>,
}

impl Bridge {
    pub fn start(runtime: Option<RuntimeInput>, client: &std::path::Path) -> Result<Self, String> {
        let store = runtime.as_ref().map(|r| Store::open(client, &r.enrollment.instance)).transpose()?;
        // Even a WebView-only admission owns the client/profile reservation.
        let guard = Some(match &store { Some(s)=>s.guard.clone(), None=>crate::intent::acquire_guard(client)? });
        let pending = store.as_ref().and_then(|s| s.value.intent.as_ref().map(|i| projection::pending(&s.value.instance, i)));
        let mut state = BridgeState::new(runtime.is_some(), store.as_ref().map_or(String::new(), |s| s.value.draft.clone()), pending);
        if let Some(c) = store.as_ref().and_then(|s| s.value.conversation.clone()) {
            state.view.conversation = Some(ConversationView { conversation_id: c, committed: Vec::new(), history_cursor: None, at_history_end: false });
        }
        state.snapshot()?;
        let shared = Arc::new((Mutex::new(state), Condvar::new()));
        let facts = Arc::new(Mutex::new(NativeFacts::default()));
        let context = Worker {
            shared: shared.clone(),
            facts: facts.clone(),
            runtime,
            store,
            remote: None,
            authenticated_endpoint: None,
            operation: OperationFacts::default(),
        };
        let handle = thread::Builder::new().name("yokai-desktop-remote".into()).spawn(move || context.run()).map_err(|_| "Cannot start desktop worker.")?;
        Ok(Self { _guard: guard, shared, worker: Mutex::new(Some(handle)), facts })
    }

    pub fn admit(&self, action: Action) -> Result<ViewSnapshot, String> {
        let mut state = self.shared.0.lock().map_err(|_| "Desktop state unavailable.")?;
        if !matches!(action, Action::Snapshot) {
            state.reserve(action)?;
            self.shared.1.notify_one();
        }
        state.snapshot()
    }
    pub async fn call(&self, action: Action) -> Result<ViewSnapshot, String> {
        // One reply slot, using Tauri's existing executor. No UI-thread wait or second worker.
        let deferred=matches!(&action,Action::Submit(_)|Action::SaveDraft(_));
        if !deferred { return self.admit(action); }
        {
            let mut state=self.shared.0.lock().map_err(|_| "Desktop state unavailable.")?;
            state.reserve(action)?;
            state.response=Some(Response { ready:None,waker:None });
        }
        self.shared.1.notify_one();
        std::future::poll_fn(|cx| {
            let Ok(mut state)=self.shared.0.lock() else { return std::task::Poll::Ready(Err("Desktop state unavailable.".into())); };
            let Some(response)=state.response.as_mut() else { return std::task::Poll::Ready(Err("Command reply unavailable.".into())); };
            if let Some(value)=response.ready.take() {
                state.response=None;
                std::task::Poll::Ready(value)
            } else {
                response.waker=Some(cx.waker().clone());
                std::task::Poll::Pending
            }
        }).await
    }
    pub fn native_ready(&self) -> Result<(), String> {
        let mut state = self.shared.0.lock().map_err(|_| "Desktop state unavailable.")?;
        if state.mark_native_ready()? {
            self.shared.1.notify_one();
        }
        Ok(())
    }
    pub fn request_close(&self) {
        if let Ok(mut state) = self.shared.0.lock() { state.close(); }
        self.shared.1.notify_all();
    }
    pub fn request_quit(&self) -> Result<(), String> {
        let mut state = self.shared.0.lock().map_err(|_| "Desktop state unavailable.")?;
        if state.request_quit()? {
            self.shared.1.notify_one();
        }
        Ok(())
    }
    pub fn begin_window_close(&self, confirmed_quit_only: bool) -> Result<bool, String> {
        let mut state = self.shared.0.lock().map_err(|_| "Desktop state unavailable.")?;
        let closing = state.begin_window_close(confirmed_quit_only);
        if closing {
            self.shared.1.notify_all();
        }
        Ok(closing)
    }
    pub fn native_failure(&self) {
        if let Ok(mut state) = self.shared.0.lock() { state.fail(FailureKind::Transport, WriteCertainty::Unknown, true); state.close(); }
        self.shared.1.notify_all();
    }
    pub fn facts(&self) -> Result<NativeFacts, String> {
        let facts = self.facts.lock().map_err(|_| "Native facts unavailable.")?;
        if facts.authentication.faulted() { return Err("Authentication evidence capture failed.".into()); }
        Ok(facts.clone())
    }
    pub fn join_finished(&self) -> Result<bool, String> {
        if self.facts()?.retained_io { return Err("Unresolved I/O owner retained.".into()); }
        let mut slot = self.worker.lock().map_err(|_| "Worker ownership unavailable.")?;
        if slot.as_ref().is_some_and(|w| !w.is_finished()) { return Ok(false); }
        if let Some(worker) = slot.take() { worker.join().map_err(|_| "Worker panicked; close not verified.")?; }
        Ok(true)
    }
}

struct Worker {
    shared: Arc<(Mutex<BridgeState>, Condvar)>,
    facts: Arc<Mutex<NativeFacts>>,
    runtime: Option<RuntimeInput>,
    store: Option<Store>,
    remote: Option<Remote>,
    authenticated_endpoint: Option<Endpoint>,
    operation: OperationFacts,
}

#[derive(Clone, Copy)]
enum ConnectPurpose {
    RestoreConversation,
    Shutdown,
}

// Admission and close share one lock. An operation claimed before close may finish,
// but each subsequent read/write must claim again. A retained OVERLAPPED stays owned.
struct IoLease { shared:Arc<(Mutex<BridgeState>,Condvar)>, retained:bool }
impl Drop for IoLease {
    fn drop(&mut self) { if let Ok(mut state)=self.shared.0.lock() { state.finish_io(self.retained); } }
}

struct Failure { kind: FailureKind, retained: bool }
impl Failure {
    fn transport() -> Self {
        Self { kind: FailureKind::Transport, retained: false }
    }

    fn persistence() -> Self {
        Self { kind: FailureKind::Persistence, retained: false }
    }

    fn can_refresh_shutdown_session(&self) -> bool {
        !self.retained && matches!(self.kind, FailureKind::Transport | FailureKind::Timeout)
    }
}
impl From<IoError> for Failure {
    fn from(e: IoError) -> Self { Self { kind: if matches!(e, IoError::Deadline) { FailureKind::Timeout } else { FailureKind::Transport }, retained: matches!(e, IoError::PendingRetained) } }
}

impl Worker {
    fn certainty(&self) -> WriteCertainty {
        self.operation.certainty(self.store.as_ref().and_then(|s|s.value.intent.as_ref()))
    }
    fn claim_io(&self) -> Result<IoLease,Failure> {
        self.shared.0.lock().map_err(|_|Failure::transport())?.claim_io().map_err(|_|Failure::transport())?;
        Ok(IoLease { shared:self.shared.clone(),retained:false })
    }
    fn acknowledge(&self) -> Result<(),Failure> {
        let waker={
            let mut state=self.shared.0.lock().map_err(|_|Failure::transport())?;
            let value=state.snapshot();
            state.response_ready(value)
        };
        if let Some(waker)=waker { waker.wake(); }
        Ok(())
    }
    fn open(&self) -> Result<(), Failure> {
        let s = self.shared.0.lock().map_err(|_| Failure::transport())?;
        if s.closing || s.fatal { Err(Failure::transport()) } else { Ok(()) }
    }
    fn publish(&self, ticket: &mut Ticket, update: impl FnOnce(&mut BridgeState)) -> Result<(), Failure> {
        let mut state = self.shared.0.lock().map_err(|_| Failure::transport())?;
        if !state.matches(*ticket) { return Err(Failure::transport()); }
        // Reserve room for later bounded failure/availability fields before publication.
        let old = state.view.clone();
        update(&mut state);
        if yokai_ipc::encode(&state.view, projection::VIEW_MAX - 1024).is_err() {
            state.view = old;
            return Err(Failure { kind: FailureKind::ProjectionLimit, retained: false });
        }
        *ticket = state.bump().map_err(|_| Failure::transport())?;
        Ok(())
    }
    fn publish_record(&self, ticket: &mut Ticket) -> Result<(), Failure> {
        let s = self.store.as_ref().ok_or_else(Failure::persistence)?;
        self.publish(ticket, |state| {
            state.view.draft = s.value.draft.clone();
            state.view.pending = s.value.intent.as_ref().map(|i| projection::pending(&s.value.instance, i));
            if state.view.conversation.is_none() {
                state.view.conversation = s.value.conversation.as_ref().map(|c| ConversationView { conversation_id:c.clone(), committed:Vec::new(), history_cursor:None, at_history_end:false });
            }
        })
    }
    fn phase(&mut self, phase: Phase, run: Option<i64>, ticket: &mut Ticket) -> Result<(), Failure> {
        if let Some(run)=run {
            if self.operation.accepted_run.is_some_and(|old|old!=run) { return Err(Failure::transport()); }
            // A local persistence failure cannot undo a valid native acknowledgement.
            self.operation.accepted_run=Some(run);
        }
        let s = self.store.as_mut().ok_or_else(Failure::persistence)?;
        let mut record = s.value.clone();
        let i = record.intent.as_mut().ok_or_else(Failure::persistence)?;
        i.phase = phase;
        if run.is_some() { i.accepted_run = run; }
        s.save(record).map_err(|_| Failure::persistence())?;
        self.publish_record(ticket)
    }
    fn send(&mut self, request: Request) -> Result<u64, Failure> {
        let mut lease=self.claim_io()?;
        let remote=self.remote.as_mut().ok_or_else(Failure::transport)?;
        if matches!(&request,Request::Create {..}|Request::Submit {..}) { self.operation.send_entered=true; }
        let result=remote.send(request);
        lease.retained=matches!(&result,Err(IoError::PendingRetained));
        result.map_err(Into::into)
    }
    fn receive(&mut self, sequence: u64, end: Instant) -> Result<Reply, Failure> {
        let mut lease=self.claim_io()?;
        let result=self.remote.as_mut().ok_or_else(Failure::transport)?.receive(end);
        lease.retained=matches!(&result,Err(IoError::PendingRetained));
        let frame=result?;
        if frame.request != sequence { return Err(Failure::transport()); }
        Ok(frame.body)
    }
    fn exchange(&mut self, request: Request) -> Result<Reply, Failure> {
        let sequence = self.send(request)?;
        self.receive(sequence, Instant::now()+Duration::from_secs(18))
    }
    fn connect(&mut self, ticket: &mut Ticket, purpose: ConnectPurpose) -> Result<(), Failure> {
        // Facts guards are released before IPC or any BridgeState operation.
        let auth = self.facts.lock().map_err(|_| Failure::transport())?
            .authentication.begin(ticket.generation).map_err(|_| Failure::transport())?;
        let outcome = self.connect_observed(ticket, auth, purpose);
        let failure = outcome.as_ref().err().map(|f| (f.kind.clone(), f.retained));
        // Publish the terminal native fact before run() publishes a UI failure.
        let recorded = self.facts.lock().map_err(|_| Failure::transport())
            .and_then(|mut facts| facts.authentication.finish(auth, failure).map_err(|_| Failure::transport()));
        if recorded.is_err() {
            // A diagnostic failure must never erase pending-I/O owner retention.
            return Err(Failure { kind: FailureKind::Transport,
                retained: outcome.as_ref().err().is_some_and(|f| f.retained) });
        }
        outcome
    }
    fn connect_observed(
        &mut self,
        ticket: &mut Ticket,
        auth: AuthTicket,
        purpose: ConnectPurpose,
    ) -> Result<(), Failure> {
        self.open()?;
        let input = self.runtime.as_ref().ok_or_else(Failure::transport)?;
        let endpoint = match purpose {
            ConnectPurpose::RestoreConversation => {
                crate::intent::ordinary(&input.control, true).map_err(|_| Failure::transport())?;
                let endpoint_path = input.control.join("endpoint.json");
                crate::intent::ordinary(&endpoint_path, false).map_err(|_| Failure::transport())?;
                yokai_ipc::read_control(&endpoint_path).map_err(|_| Failure::transport())?
            }
            // An explicit shutdown may refresh the same authenticated runtime only.
            ConnectPurpose::Shutdown => self.authenticated_endpoint.clone().ok_or_else(Failure::transport)?,
        };
        {
            let mut lease=self.claim_io()?;
            self.facts.lock().map_err(|_| Failure::transport())?
                .authentication.remote_entered(auth).map_err(|_| Failure::transport())?;
            let result=Remote::connect(&endpoint, &input.enrollment);
            lease.retained=matches!(&result,Err(IoError::PendingRetained));
            self.remote = Some(result.map_err(|e| Failure { kind: FailureKind::Authentication, retained: matches!(e, IoError::PendingRetained) })?);
        }
        self.facts.lock().map_err(|_| Failure::transport())?
            .authentication.remote_succeeded(auth).map_err(|_| Failure::transport())?;
        match self.exchange(Request::Status)? {
            Reply::Status { lifecycle, store } if lifecycle=="running" && store=="running" => (),
            _ => return Err(Failure::transport()),
        }
        self.facts.lock().map_err(|_| Failure::transport())?
            .authentication.status_confirmed(auth).map_err(|_| Failure::transport())?;
        if matches!(purpose, ConnectPurpose::RestoreConversation) {
            self.publish(ticket, |state| {
                state.connected = true;
                state.view.connection = ConnectionState::Connected;
                state.view.epoch = Some(endpoint.epoch.runtime_epoch.to_string());
                state.view.failure = None;
            })?;
            if self.store.as_ref().is_some_and(|store| store.value.conversation.is_some()) {
                self.reconcile(ticket)?;
            }
            self.authenticated_endpoint = Some(endpoint);
        }
        Ok(())
    }
    fn reconcile(&mut self, ticket: &mut Ticket) -> Result<(), Failure> {
        let record = self.store.as_ref().ok_or_else(Failure::persistence)?.value.clone();
        let conversation = record.conversation.clone().ok_or_else(Failure::transport)?;
        // Read is useful context, never evidence that a missing mutation was not applied.
        match self.exchange(Request::Read { conversation: conversation.clone() })? {
            Reply::State { found: true, latest } if latest.is_none_or(|(run,_)| run>0) => (),
            _ => return Err(Failure::transport()),
        }
        let mut replay = Replay::new(conversation.clone());
        for _ in 0..8 {
            let reply = self.exchange(Request::Replay { instance: record.instance.clone(), conversation: conversation.clone(), after: replay.after, limit: 32 })?;
            match reply {
                Reply::Replay { events, after, has_more } => {
                    replay.page(events,after,has_more).map_err(|_| Failure { kind: FailureKind::ProjectionLimit, retained:false })?;
                    if !has_more { break; }
                }
                _ => return Err(Failure::transport()),
            }
        }
        if !replay.view.at_history_end { return Err(Failure { kind: FailureKind::ProjectionLimit, retained:false }); }
        let mut next=record.clone();
        if let Some(i)=next.intent.as_mut() {
            if let Some(run)=self.operation.accepted_run {
                if i.accepted_run.is_some_and(|old|old!=run) { return Err(Failure::transport()); }
                i.accepted_run=Some(run); if !matches!(i.phase,Phase::Held) { i.phase=Phase::Accepted; }
            }
            else if matches!(i.phase,Phase::Prepared|Phase::Creating|Phase::Created|Phase::Submitting) { i.phase=Phase::Unknown; }
        }
        if let Some(i) = &record.intent {
            if let Some((run,(_,text))) = replay.runs.iter().find(|(_, (m,_))| m==&i.message) {
                if text!=&i.text || i.accepted_run.is_some_and(|r| r!=*run) { return Err(Failure::transport()); }
                self.operation.accepted_run=Some(*run);
                if replay.completed_runs.contains(run) {
                    self.operation.committed=true;
                    next.intent=None;
                } else {
                    let phase = if replay.held_runs.contains(run) || replay.failed_runs.contains(run) { Phase::Held } else { Phase::Accepted };
                    let pending=next.intent.as_mut().unwrap(); pending.phase=phase; pending.accepted_run=Some(*run);
                }
            }
            // No matching replay is not permission to resend or replace an original ID.
        }
        // Validate the complete replacement before retiring the original durable intent.
        let candidate={
            let state=self.shared.0.lock().map_err(|_|Failure::transport())?;
            if !state.matches(*ticket) { return Err(Failure::transport()); }
            projection::reconciled(&state.view,&next,replay.view).map_err(|_|Failure {kind:FailureKind::ProjectionLimit,retained:false})?
        };
        self.store.as_mut().unwrap().save(next).map_err(|_|Failure::persistence())?;
        self.publish(ticket,|s|s.view=candidate)
    }
    fn submit(&mut self, text: String, ticket: &mut Ticket) -> Result<(), Failure> {
        self.open()?;
        self.operation=OperationFacts::default();
        let s=self.store.as_mut().ok_or_else(Failure::persistence)?;
        let mut record=s.value.clone(); record.begin(text).map_err(|_| Failure::persistence())?;
        s.save(record).map_err(|_| Failure::persistence())?;
        self.publish_record(ticket)?;
        self.acknowledge()?;
        let intent=self.store.as_ref().unwrap().value.intent.clone().unwrap();
        if let Some(command)=intent.create_command.clone() {
            self.phase(Phase::Creating,None,ticket)?;
            match self.exchange(Request::Create { command, conversation:intent.conversation.clone() })? {
                Reply::Decision { code, run:Some(run), state:None } if code=="created" && run>0 => self.phase(Phase::Created,None,ticket)?,
                other => return self.reject(other,ticket),
            }
        }
        self.open()?;
        if !self.store.as_ref().and_then(|s|s.value.intent.as_ref()).is_some_and(|i|i.ready_to_submit()) { return Err(Failure::transport()); }
        self.phase(Phase::Submitting,None,ticket)?;
        let request=self.send(Request::Submit { command:intent.submit_command, conversation:intent.conversation, message:intent.message, text:intent.text })?;
        let end=Instant::now()+Duration::from_secs(18);
        let mut accepted=None; let mut progress_index=None;
        loop {
            match self.receive(request,end)? {
                Reply::Decision { code, run:Some(run), state:Some(RunStatus::Open) } if code=="accepted" && run>0 && accepted.is_none() => {
                    accepted=Some(run); self.phase(Phase::Accepted,Some(run),ticket)?;
                }
                Reply::Progress { run,index,text } if accepted==Some(run) && progress_index.is_none_or(|last| index>last) && text.len()<=1024 => {
                    progress_index=Some(index);
                    self.publish(ticket, |s| { if let Some(p)=&mut s.view.pending { p.provisional_reply=Some(text); } })?;
                }
                Reply::Decision { code, run:Some(run), state:Some(state) } if run>0 && matches!(code.as_str(),"completed"|"failed"|"duplicate"|"accepted_without_dispatch") => {
                    if accepted.is_some_and(|r| r!=run) || !matches!((code.as_str(),state), ("completed",RunStatus::Completed)|("failed",RunStatus::Failed)|("duplicate",_)|("accepted_without_dispatch",RunStatus::Open)) { return Err(Failure::transport()); }
                    self.phase(Phase::Accepted,Some(run),ticket)?;
                    return self.reconcile(ticket);
                }
                other => return self.reject(other,ticket),
            }
            self.open()?;
        }
    }
    fn reject(&mut self, reply: Reply, ticket: &mut Ticket) -> Result<(), Failure> {
        let explicit = match reply {
            Reply::Decision { code,run:None,state:None } => matches!(code.as_str(),"conflict"|"busy"|"not_found"|"stale_run"),
            Reply::Error { code,certainty:Some(certainty),phase:Some(phase) } => {
                let known=matches!(code.as_str(),"overloaded"|"operation_expired"|"store_degraded"|"store_failed"|"shutting_down"|"invalid"|"begin_failed"|"busy"|"constraint"|"storage_full"|"storage_error"|"corrupt"|"writer_gone"|"operation_wait_expired"|"refused");
                known && certainty=="not_applied" && matches!(phase.as_str(),"queued"|"begun"|"commit_issued"|"done")
            }
            _ => false,
        };
        if explicit && self.store.as_ref().and_then(|s|s.value.intent.as_ref()).is_some_and(|i|i.accepted_run.is_none()) {
            self.phase(Phase::Rejected,None,ticket)?;
            Err(Failure { kind:FailureKind::Rejected,retained:false })
        } else { Err(Failure::transport()) }
    }
    fn shutdown(&mut self, ticket: &mut Ticket) -> Result<(), Failure> {
        // A failed Status probe is read-only. Refresh a settled session before
        // sending Shutdown, and never retry Shutdown after transmission begins.
        match self.exchange(Request::Status) {
            Ok(Reply::Status { lifecycle, .. }) if lifecycle == "running" => (),
            Err(failure) if failure.can_refresh_shutdown_session() => {
                self.open()?;
                self.remote = None;
                self.connect(ticket, ConnectPurpose::Shutdown)?;
            }
            Err(failure) => return Err(failure),
            Ok(_) => return Err(Failure::transport()),
        }
        self.send_shutdown().map_err(|failure| Failure {
            kind: FailureKind::ShutdownUnconfirmed,
            retained: failure.retained,
        })?;
        self.authenticated_endpoint = None;
        self.publish(ticket, BridgeState::confirm_runtime_stopped)
    }

    fn send_shutdown(&mut self) -> Result<(), Failure> {
        let sequence=self.send(Request::Shutdown)?;
        let end=Instant::now()+Duration::from_secs(18);
        match self.receive(sequence,end)? { Reply::ShutdownAccepted => self.facts.lock().map_err(|_| Failure::transport())?.shutdown_acknowledged=true, _ => return Err(Failure::transport()) }
        match self.receive(sequence,end)? { Reply::Closed => self.facts.lock().map_err(|_| Failure::transport())?.runtime_closed_reply=true, _ => return Err(Failure::transport()) }
        self.remote=None; Ok(())
    }
    fn run(mut self) {
        loop {
            let next={
                let Ok(mut state)=self.shared.0.lock() else { return; };
                while state.queued.is_none() && !state.closing && !state.fatal {
                    state=match self.shared.1.wait(state) { Ok(s)=>s,Err(_)=>return };
                }
                if state.closing || state.fatal { return; }
                let ticket=Ticket { generation:state.generation,revision:state.revision };
                state.queued.take().map(|a|(a,ticket))
            };
            let Some((action,mut ticket))=next else { continue; };
            if self.store.as_ref().is_none_or(|s|s.value.intent.is_none()) { self.operation=OperationFacts::default(); }
            let outcome=match action {
                Action::Connect=>self.connect(&mut ticket, ConnectPurpose::RestoreConversation),
                Action::Reconcile=>self.reconcile(&mut ticket),
                Action::Submit(text)=>self.submit(text,&mut ticket),
                Action::SaveDraft(text)=>{
                    let result=if let Some(s)=self.store.as_mut() { let mut r=s.value.clone(); r.draft=text; s.save(r).map_err(|_|Failure::persistence()) } else { Err(Failure::persistence()) };
                    result.and_then(|_| self.publish_record(&mut ticket)).and_then(|_|self.acknowledge())
                }
                Action::Shutdown=>self.shutdown(&mut ticket),
                Action::Snapshot=>Err(Failure::transport()),
            };
            if let Err(failure)=outcome {
                if failure.retained {
                    if let Ok(mut f)=self.facts.lock() { f.retained_io=true; }
                }
                if let Ok(mut state)=self.shared.0.lock() {
                    if !state.closing {
                        if let Some(pending)=state.view.pending.as_mut() {
                            if self.operation.accepted_run.is_some() && !matches!(pending.phase,IntentPhase::Held) { pending.phase=IntentPhase::Accepted; }
                            else if !matches!(pending.phase,IntentPhase::DefinitelyRejected|IntentPhase::Accepted|IntentPhase::Held) { pending.phase=IntentPhase::AcceptanceUnknown; }
                        }
                        state.fail(failure.kind,self.certainty(),failure.retained);
                        let value=state.snapshot();
                        if let Some(waker)=state.response_ready(value) { waker.wake(); }
                    }
                }
                if failure.retained {
                    // Keep Remote, restoration guard and worker owner alive through uncertainty.
                    loop { thread::park(); }
                }
                self.remote=None;
            } else if let Ok(mut state)=self.shared.0.lock() {
                if state.matches(ticket) { state.busy=false; let _=state.bump(); }
            }
        }
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::{Failure, FailureKind};

    #[test]
    fn only_settled_preflight_transport_failures_allow_shutdown_refresh() {
        for kind in [FailureKind::Transport, FailureKind::Timeout] {
            assert!(Failure { kind: kind.clone(), retained: false }.can_refresh_shutdown_session());
            assert!(!Failure { kind, retained: true }.can_refresh_shutdown_session());
        }
        assert!(!Failure { kind: FailureKind::Authentication, retained: false }.can_refresh_shutdown_session());
        assert!(!Failure { kind: FailureKind::ShutdownUnconfirmed, retained: false }.can_refresh_shutdown_session());
    }
}
