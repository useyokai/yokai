//! Operating-system integration: Instance locking, path canonicalization, process identity and
//! pipe security.
//!
//! This is the only crate in which platform `unsafe` code may be written; each use must carry a
//! `SAFETY:` justification and an explicit `#[allow(unsafe_code)]` at the narrowest scope.
//! Nothing is implemented yet. In particular, nothing here may assume where the Supervisor runs.
#![deny(unsafe_code)]
