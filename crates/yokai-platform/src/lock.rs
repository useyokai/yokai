//! Persistent lock-file ownership. The lock file is never removed or truncated.
use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

#[cfg(windows)]
const FILE_SHARE_READ: u32 = 0x0000_0001;
#[cfg(windows)]
const FILE_SHARE_WRITE: u32 = 0x0000_0002;

#[derive(Debug)]
pub struct InstanceLock { lock_handle: File }

impl InstanceLock {
    /// Exclusively creates and locks the persistent lock file.
    pub fn create_and_lock(path: &Path) -> io::Result<Self> {
        let mut options = base_options();
        options.create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let file = options.open(path)?;
        file.try_lock()?;
        Ok(Self { lock_handle: file })
    }

    /// Opens and locks an existing file without creating, replacing, or truncating it.
    pub fn open_and_lock(path: &Path) -> io::Result<Self> {
        let file = base_options().open(path)?;
        file.try_lock()?;
        Ok(Self { lock_handle: file })
    }

    pub fn release(self) { drop(self.lock_handle); }
    pub fn is_locked(&self) -> bool { let _ = &self.lock_handle; true }
}

fn base_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(windows)]
    {
        // Permit contenders to open the persistent lock before try_lock reports Busy.
        // Omitting FILE_SHARE_DELETE also keeps the lock path present while this handle lives.
        std::os::windows::fs::OpenOptionsExt::share_mode(&mut options, FILE_SHARE_READ | FILE_SHARE_WRITE);
    }
    options
}
