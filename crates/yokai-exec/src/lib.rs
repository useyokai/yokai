//! Boundary between Yokai's durable execution records and the engine that runs agent steps.
//!
//! This crate must not depend on a specific engine; execution records stay owned by
//! `yokai-store`.
#![forbid(unsafe_code)]
