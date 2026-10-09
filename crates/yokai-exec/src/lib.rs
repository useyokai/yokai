//! Boundary between Yokai's durable execution records and the engine that runs agent steps.
//! Not implemented yet.
//!
//! The execution engine is not selected. This crate must not depend on any engine; execution
//! records stay owned by `yokai-store` whichever engine is chosen.
#![forbid(unsafe_code)]
