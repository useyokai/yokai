use crate::{ClientError, ConversationClient, Stream, Submission};
use std::time::Instant;
use yokai_ipc::{Event, Reply, Request, RunStatus};
use yokai_store::{Certainty, Phase, ReadError, StoreState, WriteFailure, WriteReason};
use yokai_types::{
    CommandId, ConversationDecision, ConversationEventKind, CreateConversation, MessageId,
    ReplayCursor, RunState, SubmitMessage,
};

pub(super) enum Action {
    Reply(Reply),
    Stream { stream: Stream, run: i64 },
    Stop,
}

pub(super) fn store_status(state: StoreState) -> &'static str {
    match state {
        StoreState::Running => "running",
        StoreState::Degraded => "degraded",
        StoreState::Failed => "failed",
        StoreState::ShuttingDown => "shutting_down",
        StoreState::Closed => "closed",
    }
}

fn status(state: RunState) -> RunStatus {
    match state {
        RunState::Open => RunStatus::Open,
        RunState::Held => RunStatus::Held,
        RunState::Completed => RunStatus::Completed,
        RunState::Failed => RunStatus::Failed,
    }
}

pub(super) fn result_reply(code: &str, run: Option<i64>, state: Option<RunStatus>) -> Reply {
    Reply::Decision { code: code.into(), run, state }
}

pub(super) fn decision_reply(decision: ConversationDecision) -> Reply {
    use ConversationDecision::*;
    let (code, run, state) = match decision {
        Created { run_id } => ("created", Some(run_id), None),
        Accepted(ticket) => ("accepted", Some(ticket.run_id), Some(RunStatus::Open)),
        Duplicate { run_id, state } => ("duplicate", Some(run_id), Some(status(state))),
        Completed { run_id } => ("completed", Some(run_id), Some(RunStatus::Completed)),
        Failed { run_id, .. } => ("failed", Some(run_id), Some(RunStatus::Failed)),
        Conflict => ("conflict", None, None),
        Busy => ("busy", None, None),
        NotFound => ("not_found", None, None),
        StaleRun => ("stale_run", None, None),
    };
    result_reply(code, run, state)
}

pub(super) fn write_error(error: WriteFailure) -> Reply {
    let code = match error.reason {
        WriteReason::Overloaded => "overloaded",
        WriteReason::Expired => "operation_expired",
        WriteReason::Degraded => "store_degraded",
        WriteReason::Failed => "store_failed",
        WriteReason::ShuttingDown => "shutting_down",
        WriteReason::Invalid => "invalid",
        WriteReason::BeginFailed => "begin_failed",
        WriteReason::Busy => "busy",
        WriteReason::Constraint => "constraint",
        WriteReason::StorageFull => "storage_full",
        WriteReason::StorageError => "storage_error",
        WriteReason::Corrupt => "corrupt",
        WriteReason::WriterGone => "writer_gone",
        WriteReason::WaitExpired => "operation_wait_expired",
        WriteReason::Refused => "refused",
    };
    let certainty = match error.certainty {
        Certainty::NotApplied => "not_applied",
        Certainty::OutcomeUnknown => "outcome_unknown",
    };
    let phase = match error.evidence.phase {
        Phase::Queued => "queued",
        Phase::Begun => "begun",
        Phase::CommitIssued => "commit_issued",
        Phase::Done => "done",
    };
    Reply::Error { code: code.into(), certainty: Some(certainty.into()), phase: Some(phase.into()) }
}

fn client_error(error: ClientError) -> Reply {
    match error {
        ClientError::Closing => Reply::error("shutting_down"),
        ClientError::Store(error) => write_error(error),
    }
}

fn read_error(error: ReadError) -> Reply {
    Reply::error(match error {
        ReadError::Overloaded => "overloaded",
        ReadError::Expired => "read_expired",
        ReadError::WaitExpired => "read_wait_expired",
        ReadError::Interrupted => "read_interrupted",
        ReadError::Failed => "store_failed",
        ReadError::ShuttingDown => "shutting_down",
        ReadError::StorageError => "storage_error",
        ReadError::Corrupt => "corrupt",
        ReadError::Invalid => "invalid",
        ReadError::NotFound => "not_found",
        ReadError::InvalidCursor => "invalid_cursor",
    })
}

pub(super) fn dispatch(client: &ConversationClient, request: Request, deadline: Instant) -> Action {
    let invalid = || Action::Reply(Reply::error("invalid_request"));
    let reply = match request {
        Request::Status => Reply::Status {
            lifecycle: "running".into(),
            store: store_status(client.store_state()).into(),
        },
        Request::Create { command, conversation } => {
            let (Ok(command_id), Ok(conversation_id)) = (CommandId::new(&command), conversation.parse()) else {
                return invalid();
            };
            client.create_conversation(CreateConversation { command_id, conversation_id }, deadline)
                .map_or_else(client_error, decision_reply)
        }
        Request::Submit { command, conversation, message, text } => {
            let (Ok(command_id), Ok(conversation), Ok(message)) =
                (CommandId::new(&command), conversation.parse(), message.parse()) else { return invalid() };
            let Ok(request) = SubmitMessage::new(command_id, conversation, message, &text) else { return invalid() };
            let Ok(id) = yokai_platform::random_instance_id() else {
                return Action::Reply(Reply::error("randomness_unavailable"));
            };
            match client.submit_message(request, MessageId::from_bytes(*id.as_bytes()), deadline) {
                Ok(Submission::Streaming { stream, ticket }) => return Action::Stream { stream, run: ticket.run_id },
                Ok(Submission::Decision(decision)) => decision_reply(decision),
                Ok(Submission::AcceptedWithoutDispatch { ticket, .. }) => {
                    result_reply("accepted_without_dispatch", Some(ticket.run_id), Some(RunStatus::Open))
                }
                Err(error) => client_error(error),
            }
        }
        Request::Read { conversation } => {
            let Ok(id) = conversation.parse() else { return invalid() };
            match client.read_conversation(id, deadline) {
                Ok(state) => Reply::State {
                    found: state.is_some(),
                    latest: state.and_then(|s| s.latest_run).map(|(run, s)| (run, status(s))),
                },
                Err(error) => read_error(error),
            }
        }
        Request::Replay { instance, conversation, after, limit } => {
            let (Ok(instance_id), Ok(conversation_id)) = (instance.parse(), conversation.parse()) else { return invalid() };
            let cursor = ReplayCursor { instance_id, conversation_id, after };
            match client.replay_events(conversation_id, cursor, limit, deadline) {
                Ok(page) => Reply::Replay {
                    events: page.events.into_iter().map(|event| {
                        let (kind, message) = match event.kind {
                            ConversationEventKind::Created => ("created", None),
                            ConversationEventKind::Submitted(message) => ("submitted", Some(message)),
                            ConversationEventKind::Completed(message) => ("completed", Some(message)),
                            ConversationEventKind::Failed(_) => ("failed", None),
                            ConversationEventKind::Held => ("held", None),
                        };
                        Event {
                            sequence: event.sequence,
                            run: event.run_id,
                            kind: kind.into(),
                            message: message.as_ref().map(|m| m.id.to_string()),
                            text: message.map(|m| m.text),
                        }
                    }).collect(),
                    after: page.cursor.after,
                    has_more: page.has_more,
                },
                Err(error) => read_error(error),
            }
        }
        Request::Shutdown => return Action::Stop,
    };
    Action::Reply(reply)
}
