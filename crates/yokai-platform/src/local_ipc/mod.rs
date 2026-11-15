//! Windows local pipe and peer identity operations.
mod peer;
mod pipe;

pub use peer::{ImagePin, Peer, ProcessIdentity, current_identity};
pub use pipe::{Client, IoError, Listener, Pipe, PipeName, replace_file};
#[cfg(feature = "test-api")]
pub use pipe::{adapter_flags, take_open_calls, fail_next_peer_query};
