//! Operating-system integration for Instance paths, ownership locks, and randomness.
#![deny(unsafe_code)]

pub mod lock;
pub mod location;
pub mod random;
#[cfg(windows)]
#[allow(unsafe_code)]
pub mod local_ipc;

pub use lock::InstanceLock;
pub use random::random_instance_id;
