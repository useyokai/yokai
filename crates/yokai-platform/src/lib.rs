//! Operating-system integration: Instance locking, path canonicalization, process identity and
//! pipe security.
//!
//! This is the only crate in which `unsafe` code for production OS bindings may be written; each
//! use must carry a `SAFETY:` justification and an explicit `#[allow(unsafe_code)]` at the
//! narrowest scope. Nothing here may assume where the Supervisor runs.
#![deny(unsafe_code)]
