use std::collections::{BTreeMap, BTreeSet};
use yokai_desktop_contract::*;
use crate::{commands::Action, intent::{id, Intent, Phase, Record}};

pub const VIEW_MAX: usize = 64 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ticket { pub generation: u64, pub revision: u64 }

pub struct Response {
    pub ready: Option<Result<ViewSnapshot, String>>,
    pub waker: Option<std::task::Waker>,
}

#[derive(Default)]
pub struct OperationFacts {
    pub send_entered: bool,
    pub accepted_run: Option<i64>,
    pub committed: bool,
}

impl OperationFacts {
    pub fn certainty(&self, intent: Option<&Intent>) -> WriteCertainty {
        if self.committed || self.accepted_run.is_some() || intent.is_some_and(|i| i.accepted_run.is_some()) {
            WriteCertainty::DurablyAccepted
        } else if self.send_entered || intent.is_some_and(|i| !matches!(i.phase, Phase::Prepared)) {
            WriteCertainty::Unknown
        } else if intent.is_some() { WriteCertainty::DefinitelyNotWritten } else { WriteCertainty::NoMutation }
    }
}

pub fn reconciled(view: &ViewSnapshot, record: &Record, history: ConversationView) -> Result<ViewSnapshot, String> {
    let mut next = view.clone();
    next.draft = record.draft.clone();
    next.pending = record.intent.as_ref().map(|i| pending(&record.instance, i));
    next.conversation = Some(history);
    next.failure = None;
    yokai_ipc::encode(&next, VIEW_MAX - 1024).map_err(|_| "Combined replay view exceeds bound.")?;
    Ok(next)
}

pub struct BridgeState {
    pub view: ViewSnapshot,
    pub queued: Option<Action>,
    pub busy: bool,
    pub closing: bool,
    pub fatal: bool,
    pub native_ready: bool,
    pub has_runtime: bool,
    pub connected: bool,
    pub generation: u64,
    pub revision: u64,
    pub response: Option<Response>,
    pub io_inflight: bool,
    quit_pending: bool,
}

impl BridgeState {
    pub fn new(has_runtime: bool, draft: String, pending: Option<PendingIntent>) -> Self {
        Self { view: ViewSnapshot { generation: "0".into(), epoch: None, revision: "0".into(), connection: ConnectionState::Disconnected,
            availability: Availability { connect: false, submit: false, reconcile: false, save_draft: false }, draft, conversation: None, pending, failure: None },
            queued: None, busy: false, closing: false, fatal: false, native_ready: false, has_runtime, connected: false, generation: 0, revision: 0, response:None, io_inflight:false, quit_pending: false }
    }

    pub fn mark_native_ready(&mut self) -> Result<bool, String> {
        if self.fatal || self.closing {
            return Err("Desktop admission closed.".into());
        }
        if self.native_ready {
            return Ok(false);
        }
        self.native_ready = true;
        if self.view.conversation.is_some() && self.availability().connect {
            self.reserve(Action::Connect)?;
            return Ok(true);
        }
        self.bump()?;
        Ok(false)
    }

    pub fn bump(&mut self) -> Result<Ticket, String> {
        self.revision = self.revision.checked_add(1).ok_or("Revision exhausted.")?;
        self.view.revision = self.revision.to_string();
        self.view.generation = self.generation.to_string();
        Ok(Ticket { generation: self.generation, revision: self.revision })
    }
    pub fn matches(&self, t: Ticket) -> bool { !self.closing && !self.fatal && t.generation == self.generation && t.revision == self.revision }

    pub fn availability(&self) -> Availability {
        let idle = self.native_ready && self.has_runtime && !self.busy && self.response.is_none() && !self.closing && !self.fatal && !self.quit_pending;
        Availability {
            connect: idle && !self.connected,
            submit: idle && self.connected && self.view.pending.is_none() && self.view.failure.is_none() && self.view.conversation.as_ref().is_none_or(|c| c.at_history_end),
            reconcile: idle && self.connected && self.view.conversation.is_some(),
            save_draft: idle,
        }
    }

    pub fn snapshot(&self) -> Result<ViewSnapshot, String> {
        let mut view = self.view.clone(); view.availability = self.availability();
        yokai_ipc::encode(&view, VIEW_MAX).map_err(|_| "View exceeds 64 KiB.")?;
        Ok(view)
    }

    pub fn reserve(&mut self, action: Action) -> Result<Ticket, String> {
        let allowed = self.availability();
        let eligible = match &action {
            Action::Connect => allowed.connect, Action::Submit(_) => allowed.submit,
            Action::Reconcile => allowed.reconcile, Action::SaveDraft(_) => allowed.save_draft,
            Action::Shutdown => self.connected && self.can_shutdown(),
            Action::Snapshot => false,
        };
        if !eligible || self.queued.is_some() { return Err("Action is not currently available.".into()); }
        if matches!(action, Action::Connect | Action::Shutdown) {
            self.generation = self.generation.checked_add(1).ok_or("Generation exhausted.")?;
        }
        if matches!(action, Action::Connect) {
            self.view.connection = ConnectionState::Connecting;
        }
        self.busy = true; self.queued = Some(action); self.bump()
    }

    fn can_shutdown(&self) -> bool {
        self.native_ready && !self.busy && self.response.is_none() && !self.closing
            && !self.fatal && !self.quit_pending && self.view.pending.is_none()
    }

    /// Returns whether the existing worker needs to be woken for Shutdown.
    pub fn request_quit(&mut self) -> Result<bool, String> {
        if self.quit_pending {
            return Ok(false);
        }
        let already_stopped = matches!(self.view.connection, ConnectionState::RuntimeStopped);
        if already_stopped && self.can_shutdown() {
            self.quit_pending = true;
            return Ok(false);
        }
        self.reserve(Action::Shutdown)?;
        self.quit_pending = true;
        Ok(true)
    }

    pub fn begin_window_close(&mut self, confirmed_quit_only: bool) -> bool {
        let confirmed_quit = self.quit_pending && !self.busy
            && matches!(self.view.connection, ConnectionState::RuntimeStopped);
        // A concurrent window close must let an admitted shutdown settle.
        if self.closing || ((self.quit_pending || confirmed_quit_only) && !confirmed_quit) {
            return false;
        }
        self.close();
        true
    }

    pub fn confirm_runtime_stopped(&mut self) {
        self.connected = false;
        self.view.connection = ConnectionState::RuntimeStopped;
        self.view.failure = None;
    }

    pub fn claim_io(&mut self) -> Result<(), String> {
        if self.closing || self.fatal || self.io_inflight { return Err("I/O admission closed.".into()); }
        self.io_inflight = true; Ok(())
    }
    pub fn finish_io(&mut self, retained: bool) {
        if retained { self.fatal = true; } else { self.io_inflight = false; }
    }
    pub fn response_ready(&mut self, value: Result<ViewSnapshot, String>) -> Option<std::task::Waker> {
        if let Some(response) = &mut self.response {
            if response.ready.is_none() { response.ready=Some(value); return response.waker.take(); }
        }
        None
    }
    pub fn close(&mut self) {
        self.quit_pending = false;
        self.closing = true; self.queued = None; let _ = self.bump();
        if let Some(waker)=self.response_ready(Err("Desktop is closing; preserve original intent.".into())) { waker.wake(); }
    }
    pub fn fail(&mut self, kind: FailureKind, certainty: WriteCertainty, fatal: bool) {
        self.quit_pending = false;
        let certainty=if self.view.pending.as_ref().is_some_and(|p|matches!(p.phase,IntentPhase::Accepted|IntentPhase::Held|IntentPhase::CompletionUnknown))
            || self.view.failure.as_ref().is_some_and(|f|matches!(f.write_certainty,WriteCertainty::DurablyAccepted)) {
            WriteCertainty::DurablyAccepted
        } else { certainty };
        self.fatal |= fatal; self.connected = false;
        self.view.connection = ConnectionState::Blocked;
        self.view.failure = Some(ViewFailure { kind, write_certainty: certainty });
        self.queued = None; self.busy = false; let _ = self.bump();
    }
}

pub fn pending(instance: &str, i: &Intent) -> PendingIntent {
    let phase = match i.phase {
        Phase::Prepared | Phase::Creating => IntentPhase::CreatingConversation,
        Phase::Created | Phase::Submitting => IntentPhase::SubmittingMessage,
        Phase::Accepted => IntentPhase::Accepted, Phase::Unknown => IntentPhase::AcceptanceUnknown,
        Phase::Held => IntentPhase::Held, Phase::Rejected => IntentPhase::DefinitelyRejected,
    };
    PendingIntent { phase, create_command_id: i.create_command.clone(), submit_command_id: i.submit_command.clone(),
        instance_id: instance.into(), conversation_id: i.conversation.clone(), message_id: i.message.clone(), prompt: i.text.clone(), provisional_reply: None }
}

pub struct Replay {
    pub view: ConversationView,
    pub after: i64,
    pub runs: BTreeMap<i64, (String, String)>,
    pub completed_runs: BTreeSet<i64>,
    pub held_runs: BTreeSet<i64>,
    pub failed_runs: BTreeSet<i64>,
    pages: usize,
    events: usize,
    bytes: usize,
    created: bool,
    message_ids: BTreeSet<String>,
}

impl Replay {
    pub fn new(conversation: String) -> Self {
        Self { view: ConversationView { conversation_id: conversation, committed: Vec::new(), history_cursor: None, at_history_end: false }, after: 0,
            runs: BTreeMap::new(), completed_runs: BTreeSet::new(), held_runs: BTreeSet::new(), failed_runs: BTreeSet::new(), pages: 0, events: 0, bytes: 0, created: false, message_ids: BTreeSet::new() }
    }

    pub fn page(&mut self, events: Vec<yokai_ipc::Event>, after: i64, has_more: bool) -> Result<(), String> {
        if self.pages >= 8 || events.len() > 32 || self.events + events.len() > 256 || (has_more && events.is_empty()) { return Err("Replay limit or nonprogress.".into()); }
        self.pages += 1;
        for event in events {
            if event.sequence <= self.after || event.run <= 0 { return Err("Invalid replay order.".into()); }
            self.events += 1; self.after = event.sequence;
            self.bytes += event.text.as_ref().map_or(0, String::len);
            if self.bytes > VIEW_MAX { return Err("Replay text exceeds view budget.".into()); }
            match event.kind.as_str() {
                "created" if !self.created && self.events == 1 && event.message.is_none() && event.text.is_none() => { self.created = true; },
                "submitted" if self.created => {
                    let m = event.message.ok_or("Missing user message identity.")?; id(&m)?;
                    let text = event.text.ok_or("Missing user text.")?;
                    if text.trim().is_empty() || text.len() > MAX_TEXT_BYTES || !self.message_ids.insert(m.clone()) || self.runs.contains_key(&event.run) { return Err("Invalid submitted replay.".into()); }
                    self.runs.insert(event.run, (m,text));
                }
                "completed" if self.created => {
                    let assistant = event.message.ok_or("Missing assistant identity.")?; id(&assistant)?;
                    let reply = event.text.ok_or("Missing committed reply.")?;
                    let (message_id,prompt) = self.runs.get(&event.run).ok_or("Completion without original submission.")?;
                    if !self.message_ids.insert(assistant) || !self.completed_runs.insert(event.run) || self.failed_runs.contains(&event.run) || self.held_runs.contains(&event.run) { return Err("Duplicate or contradictory completion.".into()); }
                    self.view.committed.push(CommittedTurn { message_id: message_id.clone(), prompt: prompt.clone(), reply });
                }
                "failed" | "held" if self.created && self.runs.contains_key(&event.run) && event.message.is_none() && event.text.is_none() => {
                    if self.completed_runs.contains(&event.run) || self.failed_runs.contains(&event.run) || self.held_runs.contains(&event.run) { return Err("Contradictory terminal replay.".into()); }
                    if event.kind=="held" { self.held_runs.insert(event.run); } else { self.failed_runs.insert(event.run); }
                }
                _ => return Err("Unknown or inconsistent replay event.".into()),
            }
            yokai_ipc::encode(&self.view, VIEW_MAX).map_err(|_| "Projected history exceeds bound.")?;
        }
        if after != self.after { return Err("Replay cursor mismatch.".into()); }
        self.view.history_cursor = Some(after.to_string()); self.view.at_history_end = !has_more;
        if has_more && self.pages == 8 { return Err("Replay pagination unfinished at bound.".into()); }
        Ok(())
    }
}
