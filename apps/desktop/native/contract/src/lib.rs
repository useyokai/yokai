//! Data exchanged between the native desktop host and its renderer.
//! Only the host can assign identifiers, decide eligibility, or commit a turn.

use serde::{Deserialize, Serialize};

pub const MAX_TEXT_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    RuntimeStopped,
    Blocked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum IntentPhase {
    CreatingConversation,
    SubmittingMessage,
    AcceptanceUnknown,
    Accepted,
    CompletionUnknown,
    DefinitelyRejected,
    Held,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    Authentication,
    Persistence,
    Transport,
    Timeout,
    ShutdownUnconfirmed,
    ProjectionLimit,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum WriteCertainty {
    NoMutation,
    DefinitelyNotWritten,
    Unknown,
    DurablyAccepted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(deny_unknown_fields)]
pub struct ViewFailure {
    pub kind: FailureKind,
    pub write_certainty: WriteCertainty,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(deny_unknown_fields)]
pub struct Availability {
    pub connect: bool,
    pub submit: bool,
    pub reconcile: bool,
    pub save_draft: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(deny_unknown_fields)]
pub struct PendingIntent {
    pub phase: IntentPhase,
    pub create_command_id: Option<String>,
    pub submit_command_id: String,
    pub instance_id: String,
    pub conversation_id: String,
    pub message_id: String,
    pub prompt: String,
    pub provisional_reply: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(deny_unknown_fields)]
pub struct CommittedTurn {
    pub message_id: String,
    pub prompt: String,
    pub reply: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(deny_unknown_fields)]
pub struct ConversationView {
    pub conversation_id: String,
    pub committed: Vec<CommittedTurn>,
    pub history_cursor: Option<String>,
    pub at_history_end: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(deny_unknown_fields)]
pub struct ViewSnapshot {
    pub generation: String,
    pub epoch: Option<String>,
    pub revision: String,
    pub connection: ConnectionState,
    pub availability: Availability,
    pub draft: String,
    pub conversation: Option<ConversationView>,
    pub pending: Option<PendingIntent>,
    pub failure: Option<ViewFailure>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct SubmitRequest {
    text: String,
}

impl SubmitRequest {
    pub fn text(&self) -> &str {
        &self.text
    }
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct SaveDraftRequest {
    text: String,
}

impl SaveDraftRequest {
    pub fn text(&self) -> &str {
        &self.text
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextRequest {
    text: String,
}

impl<'de> Deserialize<'de> for SubmitRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::try_from(TextRequest::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl TryFrom<TextRequest> for SubmitRequest {
    type Error = &'static str;

    fn try_from(request: TextRequest) -> Result<Self, Self::Error> {
        if request.text.trim().is_empty() {
            return Err("Enter a message.");
        }
        Ok(Self { text: bounded_text(request.text)? })
    }
}

impl<'de> Deserialize<'de> for SaveDraftRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::try_from(TextRequest::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl TryFrom<TextRequest> for SaveDraftRequest {
    type Error = &'static str;

    fn try_from(request: TextRequest) -> Result<Self, Self::Error> {
        Ok(Self { text: bounded_text(request.text)? })
    }
}

fn bounded_text(text: String) -> Result<String, &'static str> {
    if text.len() > MAX_TEXT_BYTES {
        return Err("Text exceeds 16 KiB UTF-8.");
    }
    Ok(text)
}
