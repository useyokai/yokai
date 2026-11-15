//! Boundary between Yokai's durable execution records and the engine that runs agent steps.
//!
//! This crate must not depend on a specific engine; execution records stay owned by
//! `yokai-store`.
#![forbid(unsafe_code)]
mod conversation;
mod fake;
pub use conversation::{
    ClientError, ConversationClient, ConversationRuntime, DispatchStop, PendingCompletion, Stream,
    StreamError, Submission,
};
pub use fake::Chunk;
#[cfg(windows)]
pub mod local_runtime;

#[cfg(feature = "test-api")]
pub mod test_api {
    pub use crate::fake::{FakePause, TestFake};
}
