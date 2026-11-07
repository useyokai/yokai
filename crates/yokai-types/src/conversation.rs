//! Bounded request and replay values for the headless conversation service.
use crate::{CommandId, InstanceId, InvalidId};
use std::{fmt, str::FromStr};

macro_rules! conversation_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(InstanceId);

        impl $name {
            pub const fn from_bytes(bytes: [u8; 16]) -> Self {
                Self(InstanceId::from_bytes(bytes))
            }

            pub const fn as_bytes(&self) -> &[u8; 16] {
                self.0.as_bytes()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl FromStr for $name {
            type Err = InvalidId;

            fn from_str(text: &str) -> Result<Self, Self::Err> {
                text.parse().map(Self).map_err(|_| InvalidId { kind: stringify!($name) })
            }
        }
    };
}

conversation_id!(ConversationId);
conversation_id!(MessageId);

pub const CONVERSATION_OP_VERSION: u32 = 1;
pub const MAX_USER_BYTES: usize = 16_384;
pub const MAX_RESPONSE_BYTES: usize = 65_536;
pub const MAX_CHUNK_BYTES: usize = 1_024;
pub const PROGRESS_CAPACITY: usize = 16;
pub const MAX_ACTIVE_RUNS: usize = 4;
pub const MAX_REPLAY_EVENTS: usize = 256;
pub const MAX_REPLAY_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextLength;

impl fmt::Display for TextLength {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("message text is outside the allowed UTF-8 byte range")
    }
}

impl std::error::Error for TextLength {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateConversation {
    pub command_id: CommandId,
    pub conversation_id: ConversationId,
}

impl CreateConversation {
    pub fn canonical_payload(&self) -> &[u8; 16] {
        self.conversation_id.as_bytes()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmitMessage {
    pub command_id: CommandId,
    pub conversation_id: ConversationId,
    pub message_id: MessageId,
    text: String,
}

impl SubmitMessage {
    pub fn new(
        command_id: CommandId,
        conversation_id: ConversationId,
        message_id: MessageId,
        text: &str,
    ) -> Result<Self, TextLength> {
        if !(1..=MAX_USER_BYTES).contains(&text.len()) {
            return Err(TextLength);
        }
        Ok(Self { command_id, conversation_id, message_id, text: text.to_owned() })
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// V1 is two fixed-width IDs, a big-endian byte length and exact UTF-8 bytes.
    /// The outer command digest binds the operation kind and version.
    pub fn canonical_payload(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(36 + self.text.len());
        bytes.extend_from_slice(self.conversation_id.as_bytes());
        bytes.extend_from_slice(self.message_id.as_bytes());
        bytes.extend_from_slice(&(self.text.len() as u32).to_be_bytes());
        bytes.extend_from_slice(self.text.as_bytes());
        bytes
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunTicket {
    pub instance_id: InstanceId,
    pub conversation_id: ConversationId,
    pub run_id: i64,
    pub epoch: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunState {
    Open,
    Held,
    Completed,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FakeFailure {
    Adapter,
    ProgressFull,
    ProgressDisconnected,
    InvalidResponse,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssistantResponse {
    pub message_id: MessageId,
    text: String,
}

impl AssistantResponse {
    pub fn new(message_id: MessageId, text: &str) -> Result<Self, TextLength> {
        if !(1..=MAX_RESPONSE_BYTES).contains(&text.len()) {
            return Err(TextLength);
        }
        Ok(Self { message_id, text: text.to_owned() })
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FakeOutcome {
    Response(AssistantResponse),
    Failed(FakeFailure),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompleteFakeRun {
    pub ticket: RunTicket,
    pub outcome: FakeOutcome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConversationDecision {
    Created { run_id: i64 },
    Accepted(RunTicket),
    Duplicate { run_id: i64, state: RunState },
    Completed { run_id: i64 },
    Failed { run_id: i64, reason: FakeFailure },
    Conflict,
    Busy,
    NotFound,
    StaleRun,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplayCursor {
    pub instance_id: InstanceId,
    pub conversation_id: ConversationId,
    pub after: i64,
}

impl ReplayCursor {
    pub fn start(instance_id: InstanceId, conversation_id: ConversationId) -> Self {
        Self { instance_id, conversation_id, after: 0 }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationMessage {
    pub id: MessageId,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConversationEventKind {
    Created,
    Submitted(ConversationMessage),
    Completed(ConversationMessage),
    Failed(FakeFailure),
    Held,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationEvent {
    pub sequence: i64,
    pub run_id: i64,
    pub kind: ConversationEventKind,
}

impl ConversationEvent {
    /// Conservative byte accounting for IDs, sequence, kind and UTF-8 body.
    pub fn replay_bytes(&self) -> usize {
        64 + match &self.kind {
            ConversationEventKind::Submitted(message) | ConversationEventKind::Completed(message) => message.text.len(),
            _ => 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayPage {
    pub events: Vec<ConversationEvent>,
    pub cursor: ReplayCursor,
    pub has_more: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConversationState {
    pub id: ConversationId,
    pub latest_run: Option<(i64, RunState)>,
}
