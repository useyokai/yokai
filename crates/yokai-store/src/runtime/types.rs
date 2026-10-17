use std::fmt;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use yokai_types::InstanceId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreState { Running, Degraded, Failed, ShuttingDown, Closed }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase { Queued, Begun, CommitIssued, Done }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Certainty { NotApplied, OutcomeUnknown }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteReason { Overloaded, Expired, Degraded, Failed, ShuttingDown, Invalid,
    BeginFailed, Busy, Constraint, StorageFull, StorageError, Corrupt, WriterGone, WaitExpired, Refused }
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteEvidence {
    pub phase: Phase,
    pub sqlite_code: Option<i32>,
    pub autocommit_after_error: Option<bool>,
    pub rollback_succeeded: Option<bool>,
    pub autocommit_after_rollback: Option<bool>,
}
impl WriteEvidence {
    pub(crate) fn queued() -> Self { Self { phase: Phase::Queued, sqlite_code: None,
        autocommit_after_error: None, rollback_succeeded: None, autocommit_after_rollback: None } }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteFailure {
    pub operation_id: Option<u64>,
    pub certainty: Certainty,
    pub reason: WriteReason,
    pub evidence: WriteEvidence,
}
impl WriteFailure {
    pub(crate) fn queued(id: Option<u64>, reason: WriteReason) -> Self {
        Self { operation_id: id, certainty: Certainty::NotApplied, reason, evidence: WriteEvidence::queued() }
    }
}
impl fmt::Display for WriteFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "write {:?}: {:?}/{:?} at {:?}", self.operation_id, self.certainty, self.reason, self.evidence.phase)
    }
}
impl std::error::Error for WriteFailure {}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteSuccess { pub operation_id: u64 }
pub type WriteResult = Result<WriteSuccess, WriteFailure>;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion { pub operation_id: u64, pub phase: Phase, pub result: WriteResult }
pub struct PendingWrite {
    pub(crate) id: u64,
    pub(crate) reply: mpsc::Receiver<WriteResult>,
    pub(crate) progress: Arc<Mutex<WriteEvidence>>,
}
impl PendingWrite {
    pub fn operation_id(&self) -> u64 { self.id }
    /// A caller wait timeout cannot cancel a handed-off write or infer NotApplied.
    pub fn wait(self, deadline: Instant) -> WriteResult {
        match self.reply.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(result) => result,
            Err(error) => {
                let evidence = super::lock(&self.progress).clone();
                let timeout = matches!(error, mpsc::RecvTimeoutError::Timeout);
                let certainty = if !timeout && evidence.phase == Phase::Queued { Certainty::NotApplied }
                    else { Certainty::OutcomeUnknown };
                Err(WriteFailure { operation_id: Some(self.id), certainty,
                    reason: if timeout { WriteReason::WaitExpired } else { WriteReason::WriterGone }, evidence })
            }
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstanceMetadata {
    pub instance_id: InstanceId,
    pub format: u32,
    pub created_at: String,
    pub last_health_check: Option<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadError { Overloaded, Expired, WaitExpired, Interrupted, Failed, ShuttingDown, StorageError, Corrupt, Invalid }
impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "read: {self:?}") }
}
impl std::error::Error for ReadError {}
pub struct PendingRead {
    pub(crate) id: u64,
    pub(crate) reply: mpsc::Receiver<Result<InstanceMetadata, ReadError>>,
}
impl PendingRead {
    pub fn query_id(&self) -> u64 { self.id }
    pub fn wait(self, deadline: Instant) -> Result<InstanceMetadata, ReadError> {
        self.reply.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap_or_else(|error| {
            Err(if matches!(error, mpsc::RecvTimeoutError::Timeout) { ReadError::WaitExpired } else { ReadError::Failed })
        })
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoreMetrics {
    pub wal_bytes: u64,
    pub wal_size_verified: bool,
    pub wal_sampled_at: Option<Instant>,
    pub checkpoint_pending: bool,
    pub last_checkpoint_error: Option<i32>,
    pub completed_writes: u64,
}

/// Resource bounds, validated before any Instance file is created or opened.
#[derive(Clone, Debug)]
pub struct StoreOptions {
    pub queue_capacity: usize,
    pub readers: usize,
    pub close_timeout: Duration,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self { queue_capacity: 64, readers: 1, close_timeout: Duration::from_secs(10) }
    }
}

impl StoreOptions {
    pub(crate) fn validate(&self) -> Result<(), crate::InstanceError> {
        if !(1..=256).contains(&self.queue_capacity) || !(1..=2).contains(&self.readers)
            || !(Duration::from_millis(1)..=Duration::from_secs(30)).contains(&self.close_timeout) {
            return Err(crate::InstanceError::Store("invalid storage worker bounds".into()));
        }
        Ok(())
    }
}
