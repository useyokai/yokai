//! Test-only support code. This crate may appear only as a dev-dependency.
//!
//! Every test binary in the workspace must use a custom harness, call [`make_non_dumpable`] as its
//! first action, print [`Dumpable::line`], and refuse to run any check unless [`Dumpable::ok`]
//! holds, so that a crashing test does not produce a core dump (unless the host sets
//! `fs.suid_dumpable=2`). Each process clears the flag for itself: `execve` resets it, so nothing
//! set by a parent is relied on.
#![deny(unsafe_code)]

/// Values observed while clearing the dumpable flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dumpable {
    /// The flag before this process changed anything (1 is expected after `execve`).
    pub before: i32,
    /// Return code of the request to clear the flag (0 on success).
    pub set_rc: i32,
    /// The flag afterwards (must be 0).
    pub after: i32,
}

impl Dumpable {
    /// True if the flag was cleared and reads back as 0.
    pub fn ok(&self) -> bool {
        self.set_rc == 0 && self.after == 0
    }

    /// The first line every test binary prints: `DUMPABLE before=<n> set_rc=<n> after=<n>`.
    pub fn line(&self) -> String {
        format!("DUMPABLE before={} set_rc={} after={}", self.before, self.set_rc, self.after)
    }
}

/// Reads the dumpable flag, clears it, and reads it back.
///
/// Implemented for Linux only. Elsewhere nothing is changed and every field is -1, so
/// [`Dumpable::ok`] is false and a gated test refuses to run.
pub fn make_non_dumpable() -> Dumpable {
    sys::make_non_dumpable()
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod sys {
    use std::ffi::{c_int, c_ulong};

    use super::Dumpable;

    const PR_GET_DUMPABLE: c_int = 3;
    const PR_SET_DUMPABLE: c_int = 4;

    unsafe extern "C" {
        // glibc: int prctl(int option, ...);
        fn prctl(option: c_int, ...) -> c_int;
    }

    pub(crate) fn make_non_dumpable() -> Dumpable {
        let z: c_ulong = 0;
        // SAFETY: PR_GET_DUMPABLE and PR_SET_DUMPABLE take only integer arguments and touch no
        // memory.
        unsafe {
            let before = prctl(PR_GET_DUMPABLE, z, z, z, z);
            let set_rc = prctl(PR_SET_DUMPABLE, z, z, z, z);
            let after = prctl(PR_GET_DUMPABLE, z, z, z, z);
            Dumpable { before, set_rc, after }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod sys {
    use super::Dumpable;

    pub(crate) fn make_non_dumpable() -> Dumpable {
        Dumpable { before: -1, set_rc: -1, after: -1 }
    }
}
