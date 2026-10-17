//! Test-only support code. This crate may appear only as a dev-dependency.
//!
//! Every test binary in the workspace must use a custom harness and report its dumpability
//! status before running checks. Linux must clear the process dumpable flag. Windows has no
//! equivalent implemented here, so checks remain disabled unless an isolated test runner sets
//! YOKAI_TEST_ALLOW_UNPROTECTED_WINDOWS=1; this opt-in does not disable Windows Error Reporting
//! or establish that Windows crash dumps are impossible. Other unsupported targets remain blocked.
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
    /// Allows checks only after Linux protection succeeds or with the explicit Windows opt-in.
    pub fn permits_checks(&self) -> bool {
        if self.ok() {
            return true;
        }
        #[cfg(windows)]
        {
            self.before == -1 && self.set_rc == -1 && self.after == -1 && windows_test_opt_in()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    pub fn line(&self) -> String {
        #[cfg(windows)]
        if self.before == -1 && self.set_rc == -1 && self.after == -1 {
            return format!(
                "DUMPABLE status=unsupported platform=windows opt_in={}",
                u8::from(windows_test_opt_in())
            );
        }
        format!("DUMPABLE before={} set_rc={} after={}", self.before, self.set_rc, self.after)
    }
}

/// Reads and clears the Linux dumpable flag. Windows reports the unsupported sentinel; its
/// gated test can proceed only with the explicit isolated-test opt-in documented above.

pub fn make_non_dumpable() -> Dumpable {
    sys::make_non_dumpable()
}

#[cfg(windows)]
const WINDOWS_OPT_IN_ENV: &str = "YOKAI_TEST_ALLOW_UNPROTECTED_WINDOWS";

#[cfg(windows)]
fn windows_test_opt_in() -> bool {
    matches!(std::env::var(WINDOWS_OPT_IN_ENV), Ok(value) if value == "1")
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
