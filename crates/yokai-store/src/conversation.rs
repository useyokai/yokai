//! Conversation transaction bodies. The storage workers own transaction certainty.
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::time::Instant;
use yokai_types::{
    CONVERSATION_OP_VERSION, CompleteFakeRun, ConversationDecision as Decision, ConversationEvent,
    ConversationEventKind as Event, ConversationId, ConversationMessage, ConversationState,
    CreateConversation, FakeFailure, FakeOutcome, InstanceId, MAX_ACTIVE_RUNS, MAX_REPLAY_BYTES, MessageId,
    ReplayCursor, ReplayPage, RunState, RunTicket, SubmitMessage,
};

use crate::{AcceptOutcome, CommandRequest, PendingWrite, WriteFailure, durable};

const CREATE: &str = "conversation.create";
const SUBMIT: &str = "conversation.submit";
const FAKE: &str = "conversation.fake";
const CREATED: &str = "conversation.created";
const SUBMITTED: &str = "conversation.submitted";
const COMPLETED: &str = "conversation.completed";
const FAILED: &str = "conversation.failed";
const HELD: &str = "run.held";

pub struct PendingConversation {
    pub(crate) inner: PendingWrite,
}

impl PendingConversation {
    pub fn operation_id(&self) -> u64 {
        self.inner.operation_id()
    }

    pub fn wait(self, deadline: Instant) -> Result<Decision, WriteFailure> {
        let id = self.inner.operation_id();
        self.inner.wait_value(deadline)?.conversation().ok_or_else(|| WriteFailure::missing_reply(id))
    }
}

pub(crate) enum ConversationWrite {
    Create(CreateConversation),
    Submit(SubmitMessage),
    Complete(CompleteFakeRun),
}

impl ConversationWrite {
    pub(crate) fn apply(&self, conn: &Connection, instance_id: InstanceId) -> rusqlite::Result<(Decision, bool)> {
        match self {
            Self::Create(request) => create(conn, request),
            Self::Submit(request) => submit(conn, instance_id, request),
            Self::Complete(request) => complete(conn, instance_id, request),
        }
    }
}

fn create(conn: &Connection, request: &CreateConversation) -> rusqlite::Result<(Decision, bool)> {
    let id = request.conversation_id.as_bytes().as_slice();
    let command = CommandRequest::new(request.command_id.clone(), CREATE, CONVERSATION_OP_VERSION, request.canonical_payload())
        .map_err(|_| rusqlite::Error::InvalidQuery)?;
    if let Some(existing) = durable::lookup(conn, &command)? {
        let decision = match existing {
            AcceptOutcome::Duplicate { run_id } if conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM conversations WHERE conversation_id=?1 AND create_run_id=?2)",
                params![id, run_id], |row| row.get(0),
            )? => Decision::Duplicate { run_id, state: RunState::Completed },
            _ => Decision::Conflict,
        };
        return Ok((decision, false));
    }
    if exists(conn, request.conversation_id)? {
        return Ok((Decision::Conflict, false));
    }
    let (run_id, epoch) = durable::insert(conn, &command)?;
    conn.execute("INSERT INTO conversations VALUES(?1,?2)", params![id, run_id])?;
    admit_records(conn, run_id, epoch, CREATE)?;
    finish_records(conn, run_id, "Completed", "Done")?;
    event(conn, run_id, CREATED, "", epoch)?;
    Ok((Decision::Created { run_id }, true))
}

fn submit(conn: &Connection, instance_id: InstanceId, request: &SubmitMessage) -> rusqlite::Result<(Decision, bool)> {
    let id = request.conversation_id.as_bytes().as_slice();
    let command = CommandRequest::new(request.command_id.clone(), SUBMIT, CONVERSATION_OP_VERSION, &request.canonical_payload())
        .map_err(|_| rusqlite::Error::InvalidQuery)?;
    if let Some(existing) = durable::lookup(conn, &command)? {
        // An opaque command with the same encoding has no typed binding or execution authority.
        let decision = match existing {
            AcceptOutcome::Duplicate { run_id } => match run(conn, request.conversation_id, run_id)? {
                Some((state, _)) => Decision::Duplicate { run_id, state },
                None => Decision::Conflict,
            },
            _ => Decision::Conflict,
        };
        return Ok((decision, false));
    }
    if !exists(conn, request.conversation_id)? {
        return Ok((Decision::NotFound, false));
    }
    let active: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM conversation_runs WHERE conversation_id=?1 AND active=1)",
        [id], |row| row.get(0),
    )?;
    if active {
        return Ok((Decision::Busy, false));
    }
    // Held runs still own their conversation slot. Only this epoch's admitted runs
    // consume the global execution limit.
    let occupied: i64 = conn.query_row(
        "SELECT count(*) FROM conversation_runs AS cr JOIN runs AS r USING(run_id)
         WHERE cr.active=1 AND r.status='Open' AND r.admission='Admitted'
         AND r.admitted_epoch=(SELECT epoch FROM runtime_epoch WHERE id=1)",
        [], |row| row.get(0),
    )?;
    if occupied >= MAX_ACTIVE_RUNS as i64 {
        return Ok((Decision::Busy, false));
    }
    if message_exists(conn, request.message_id)? {
        return Ok((Decision::Conflict, false));
    }

    let (run_id, epoch) = durable::insert(conn, &command)?;
    conn.execute("INSERT INTO conversation_runs VALUES(?1,?2,1)", params![run_id, id])?;
    insert_message(conn, run_id, request.message_id, "user", request.text())?;
    admit_records(conn, run_id, epoch, FAKE)?;
    event(conn, run_id, SUBMITTED, "", epoch)?;
    Ok((Decision::Accepted(RunTicket { instance_id, conversation_id: request.conversation_id, run_id, epoch }), true))
}

fn complete(conn: &Connection, instance_id: InstanceId, request: &CompleteFakeRun) -> rusqlite::Result<(Decision, bool)> {
    let RunTicket { instance_id: source, conversation_id, run_id, epoch } = request.ticket;
    if source != instance_id {
        return Ok((Decision::StaleRun, false));
    }
    let Some((state, admitted_epoch)) = run(conn, conversation_id, run_id)? else {
        return Ok((Decision::NotFound, false));
    };
    if admitted_epoch != Some(epoch) || state == RunState::Held {
        return Ok((Decision::StaleRun, false));
    }
    if state != RunState::Open {
        let equivalent = match (&request.outcome, state) {
            (FakeOutcome::Response(response), RunState::Completed) => conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM conversation_messages
                 WHERE run_id=?1 AND role='assistant' AND message_id=?2 AND body=?3)",
                params![run_id, response.message_id.as_bytes().as_slice(), response.text()], |row| row.get(0),
            )?,
            (FakeOutcome::Failed(reason), RunState::Failed) => conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM events WHERE run_id=?1 AND kind=?2 AND detail=?3)",
                params![run_id, FAILED, failure_name(*reason)], |row| row.get(0),
            )?,
            _ => false,
        };
        return Ok((if equivalent { Decision::Duplicate { run_id, state } } else { Decision::Conflict }, false));
    }
    let current: i64 = conn.query_row("SELECT epoch FROM runtime_epoch WHERE id=1", [], |row| row.get(0))?;
    if current != epoch {
        return Ok((Decision::StaleRun, false));
    }

    let decision = match &request.outcome {
        FakeOutcome::Response(response) => {
            if message_exists(conn, response.message_id)? {
                return Ok((Decision::Conflict, false));
            }
            insert_message(conn, run_id, response.message_id, "assistant", response.text())?;
            finish_records(conn, run_id, "Completed", "Done")?;
            event(conn, run_id, COMPLETED, "", epoch)?;
            Decision::Completed { run_id }
        }
        FakeOutcome::Failed(reason) => {
            finish_records(conn, run_id, "Failed", "Cancelled")?;
            event(conn, run_id, FAILED, failure_name(*reason), epoch)?;
            Decision::Failed { run_id, reason: *reason }
        }
    };
    changed_one(conn.execute("UPDATE conversation_runs SET active=0 WHERE run_id=?1 AND active=1", [run_id])?)?;
    Ok((decision, true))
}

fn admit_records(conn: &Connection, run_id: i64, epoch: i64, owner: &str) -> rusqlite::Result<()> {
    changed_one(conn.execute(
        "UPDATE runs SET admission='Admitted',admitted_epoch=?2 WHERE run_id=?1 AND admission='Queued' AND status='Open'",
        params![run_id, epoch],
    )?)?;
    changed_one(conn.execute(
        "UPDATE jobs SET state='Claimed',claimed_by=?2,claim_epoch=?3 WHERE run_id=?1 AND job_no=1 AND state='Pending'",
        params![run_id, owner, epoch],
    )?)
}

fn finish_records(conn: &Connection, run_id: i64, status: &str, job_state: &str) -> rusqlite::Result<()> {
    changed_one(conn.execute("UPDATE runs SET status=?2 WHERE run_id=?1 AND status='Open' AND admission='Admitted'", params![run_id, status])?)?;
    changed_one(conn.execute("UPDATE jobs SET state=?2 WHERE run_id=?1 AND job_no=1 AND state='Claimed'", params![run_id, job_state])?)
}

fn changed_one(count: usize) -> rusqlite::Result<()> {
    if count == 1 { Ok(()) } else { Err(rusqlite::Error::InvalidQuery) }
}

fn event(conn: &Connection, run_id: i64, kind: &str, detail: &str, epoch: i64) -> rusqlite::Result<()> {
    conn.execute("INSERT INTO events(run_id,kind,detail,epoch) VALUES(?1,?2,?3,?4)", params![run_id, kind, detail, epoch])?;
    Ok(())
}

fn insert_message(conn: &Connection, run_id: i64, id: MessageId, role: &str, body: &str) -> rusqlite::Result<()> {
    conn.execute("INSERT INTO conversation_messages VALUES(?1,?2,?3,?4)", params![id.as_bytes().as_slice(), run_id, role, body])?;
    Ok(())
}

fn exists(conn: &Connection, id: ConversationId) -> rusqlite::Result<bool> {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM conversations WHERE conversation_id=?1)",
        [id.as_bytes().as_slice()], |row| row.get(0))
}

fn message_exists(conn: &Connection, id: MessageId) -> rusqlite::Result<bool> {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM conversation_messages WHERE message_id=?1)",
        [id.as_bytes().as_slice()], |row| row.get(0))
}

fn run(conn: &Connection, id: ConversationId, run_id: i64) -> rusqlite::Result<Option<(RunState, Option<i64>)>> {
    conn.query_row(
        "SELECT r.status,r.admission,r.admitted_epoch FROM conversation_runs AS cr JOIN runs AS r USING(run_id)
         WHERE cr.conversation_id=?1 AND cr.run_id=?2",
        params![id.as_bytes().as_slice(), run_id], |row| Ok((run_state(row, 0, 1)?, row.get(2)?)),
    ).optional()
}

fn run_state(row: &Row<'_>, status: usize, admission: usize) -> rusqlite::Result<RunState> {
    match (row.get_ref(status)?.as_str()?, row.get_ref(admission)?.as_str()?) {
        ("Open", "Held") => Ok(RunState::Held),
        ("Open", "Admitted") => Ok(RunState::Open),
        ("Completed", "Admitted") => Ok(RunState::Completed),
        ("Failed", "Admitted") => Ok(RunState::Failed),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

fn failure_name(reason: FakeFailure) -> &'static str {
    match reason {
        FakeFailure::Adapter => "adapter",
        FakeFailure::ProgressFull => "progress-full",
        FakeFailure::ProgressDisconnected => "progress-disconnected",
        FakeFailure::InvalidResponse => "invalid-response",
    }
}

fn parse_failure(detail: &str) -> rusqlite::Result<FakeFailure> {
    match detail {
        "adapter" => Ok(FakeFailure::Adapter),
        "progress-full" => Ok(FakeFailure::ProgressFull),
        "progress-disconnected" => Ok(FakeFailure::ProgressDisconnected),
        "invalid-response" => Ok(FakeFailure::InvalidResponse),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

pub(crate) fn read_state(conn: &Connection, id: ConversationId) -> rusqlite::Result<Option<ConversationState>> {
    conn.query_row(
        "SELECT cr.run_id,r.status,r.admission
         FROM conversations AS c
         LEFT JOIN conversation_runs AS cr ON cr.run_id=(
             SELECT run_id FROM conversation_runs WHERE conversation_id=c.conversation_id ORDER BY run_id DESC LIMIT 1
         )
         LEFT JOIN runs AS r ON r.run_id=cr.run_id WHERE c.conversation_id=?1",
        [id.as_bytes().as_slice()], |row| {
            let latest_run = match row.get::<_, Option<i64>>(0)? {
                Some(run_id) => Some((run_id, run_state(row, 1, 2)?)),
                None => None,
            };
            Ok(ConversationState { id, latest_run })
        },
    ).optional()
}

pub(crate) enum ReplayRead {
    Page(ReplayPage),
    NotFound,
    InvalidCursor,
}

// Domain events use the existing sequence. Bookkeeping and unrelated run events
// are excluded even when they use the same run ID.
const REPLAY_FROM: &str =
    "FROM events AS e
     LEFT JOIN conversations AS c ON c.create_run_id=e.run_id
     LEFT JOIN conversation_runs AS cr ON cr.run_id=e.run_id
     LEFT JOIN conversation_messages AS m ON m.run_id=e.run_id AND m.role=CASE e.kind
         WHEN 'conversation.submitted' THEN 'user' WHEN 'conversation.completed' THEN 'assistant' END
     WHERE ((c.conversation_id=?1 AND e.kind='conversation.created')
         OR (cr.conversation_id=?1 AND e.kind IN ('conversation.submitted','conversation.completed','conversation.failed','run.held')))";

pub(crate) fn replay(conn: &Connection, cursor: ReplayCursor, limit: usize) -> rusqlite::Result<ReplayRead> {
    // The reader rolls this transaction back after the request. Cursor validation
    // and row selection must use the same committed snapshot.
    conn.execute_batch("BEGIN")?;
    if !exists(conn, cursor.conversation_id)? {
        return Ok(ReplayRead::NotFound);
    }
    let id = cursor.conversation_id.as_bytes().as_slice();
    if cursor.after != 0 {
        let sql = format!("SELECT EXISTS(SELECT 1 {REPLAY_FROM} AND e.seq=?2)");
        let valid: bool = conn.query_row(&sql, params![id, cursor.after], |row| row.get(0))?;
        if !valid {
            return Ok(ReplayRead::InvalidCursor);
        }
    }
    let sql = format!("SELECT e.seq,e.run_id,e.kind,e.detail,m.message_id,m.body {REPLAY_FROM} AND e.seq>?2 ORDER BY e.seq LIMIT ?3");
    let mut statement = conn.prepare_cached(&sql)?;
    let mut rows = statement.query(params![id, cursor.after, (limit + 1) as i64])?;
    let mut events = Vec::with_capacity(limit);
    let mut bytes = 0;
    let mut has_more = false;
    while let Some(row) = rows.next()? {
        if events.len() == limit {
            has_more = true;
            break;
        }
        let entry = replay_event(row)?;
        let entry_bytes = entry.replay_bytes();
        if bytes + entry_bytes > MAX_REPLAY_BYTES {
            has_more = true;
            break;
        }
        bytes += entry_bytes;
        events.push(entry);
    }
    let next = ReplayCursor { after: events.last().map_or(cursor.after, |event| event.sequence), ..cursor };
    Ok(ReplayRead::Page(ReplayPage { events, cursor: next, has_more }))
}

fn replay_event(row: &Row<'_>) -> rusqlite::Result<ConversationEvent> {
    let kind = match row.get_ref(2)?.as_str()? {
        CREATED => Event::Created,
        SUBMITTED => Event::Submitted(read_message(row)?),
        COMPLETED => Event::Completed(read_message(row)?),
        FAILED => Event::Failed(parse_failure(row.get_ref(3)?.as_str()?)?),
        HELD => Event::Held,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    Ok(ConversationEvent { sequence: row.get(0)?, run_id: row.get(1)?, kind })
}

fn read_message(row: &Row<'_>) -> rusqlite::Result<ConversationMessage> {
    let bytes = row.get::<_, Vec<u8>>(4)?.try_into().map_err(|_| rusqlite::Error::InvalidQuery)?;
    Ok(ConversationMessage { id: MessageId::from_bytes(bytes), text: row.get(5)? })
}
