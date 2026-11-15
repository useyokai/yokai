//! Local inter-process protocol: framing, handshake and role gates.
#![forbid(unsafe_code)]

mod protocol;
pub use protocol::*;
#[cfg(windows)]
mod local;
#[cfg(windows)]
pub use local::*;
