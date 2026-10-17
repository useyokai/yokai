//! Local Instance storage. Application connections are owned by the Instance.
#![forbid(unsafe_code)]
mod connection;
pub use connection::{StoreError, ConnectionSettings, SqliteRuntime, MIN_SQLITE_VERSION, BUSY_TIMEOUT_MS,
    ext_code, parse_version, require_min_version};
pub(crate) use connection::{create_file, init_connection, foreign_key_violations};
mod runtime;
mod durable;
pub use durable::{CommandRequest,InvalidCommand,AcceptOutcome,PendingCommand};
mod startup;
pub use startup::StartupReport;
pub use runtime::{StoreHandle, StoreOptions, StoreState, Phase, Certainty, WriteReason,
    WriteEvidence, WriteFailure, WriteSuccess, WriteResult, Completion, PendingWrite,
    InstanceMetadata, ReadError, PendingRead, StoreMetrics};
mod instance;
mod preflight;
mod schema;
mod snapshot;
pub use snapshot::BackupUsage;
#[cfg(feature = "test-api")]
mod failpoint;
pub use instance::{create_instance, open_instance, create_instance_with_options, open_instance_with_options, IncompleteReason, Instance, InstanceError};
pub use preflight::{FileInventory, InventoryEntry};
#[cfg(feature = "test-api")]
pub mod test_api;
