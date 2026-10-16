//! Canonical path and local-filesystem policy for Instance storage.
use std::fs::{self, Metadata};
#[cfg(unix)]
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
#[cfg(feature = "test-api")]
use std::sync::{Mutex, OnceLock};

#[cfg(feature = "test-api")]
static TEST_TMPFS_ROOT: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
#[cfg(feature = "test-api")]
static DIRECTORY_FLUSH_EVENTS: OnceLock<Mutex<Vec<(PathBuf, bool)>>> = OnceLock::new();

#[cfg(any(windows, feature = "test-api"))]
fn windows_path_is_network(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.starts_with("\\\\?\\unc\\")
        || (lower.starts_with("\\\\") && !lower.starts_with("\\\\?\\"))
}

#[cfg(feature = "test-api")]
pub fn windows_path_is_network_for_test(text: &str) -> bool {
    windows_path_is_network(text)
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}
fn unsupported(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, message.into())
}

pub fn validate_parent(path: &Path) -> io::Result<PathBuf> {
    validate_existing_directory(path, "parent")
}
pub fn validate_root(path: &Path) -> io::Result<PathBuf> {
    validate_existing_directory(path, "root")
}
fn validate_existing_directory(path: &Path, label: &str) -> io::Result<PathBuf> {
    if !path.is_absolute() { return Err(invalid(format!("{label} path must be absolute"))); }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(invalid(format!("{label} must be a real directory")));
    }
    let canonical = fs::canonicalize(path)?;
    check_local_filesystem(&canonical)?;
    Ok(canonical)
}

pub fn validate_entries(root: &Path) -> io::Result<()> {
    for name in ["instance.lock", "instance.id", "instance.db", "instance.db-wal", "instance.db-shm", "instance.db-journal"] {
        let path = root.join(name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => validate_regular_single_link(&path, &metadata)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub fn validate_regular_single_link(path: &Path, metadata: &Metadata) -> io::Result<()> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(invalid(format!("{} is not a regular file", path.display())));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(invalid(format!("{} has {} hard links", path.display(), metadata.nlink())));
        }
    }
    #[cfg(windows)]
    windows_api::validate_regular_single_link(path)?;
    Ok(())
}

/// Registers one test directory whose tmpfs descendants may bypass the production allowlist.
#[cfg(feature = "test-api")]
pub fn allow_tmpfs_for_test(path: &Path) -> io::Result<()> {
    let canonical = fs::canonicalize(path)?;
    if !canonical.is_dir() { return Err(invalid("test tmpfs registration must be a directory")); }
    let registry = TEST_TMPFS_ROOT.get_or_init(|| Mutex::new(None));
    let mut guard = registry.lock().map_err(|_| io::Error::other("test path registry poisoned"))?;
    match guard.as_ref() {
        Some(existing) if existing == &canonical => Ok(()),
        Some(_) => Err(invalid("a different test tmpfs directory is already registered")),
        None => { *guard = Some(canonical); Ok(()) }
    }
}
#[cfg(all(target_os = "linux", feature = "test-api"))]
fn test_tmpfs_contains(path: &Path) -> bool {
    TEST_TMPFS_ROOT.get()
        .and_then(|registry| registry.lock().ok().and_then(|guard| guard.clone()))
        .is_some_and(|root| path.starts_with(root))
}

#[cfg(target_os = "linux")]
fn check_local_filesystem(path: &Path) -> io::Result<()> {
    let fs_type = linux_mount_type(path)?;
    match fs_type.as_str() {
        "ext4" | "xfs" | "btrfs" => Ok(()),
        "tmpfs" => {
            #[cfg(feature = "test-api")]
            if test_tmpfs_contains(path) { return Ok(()); }
            Err(unsupported("filesystem tmpfs is not approved"))
        }
        other => Err(unsupported(format!("filesystem {other} is not approved"))),
    }
}
#[cfg(target_os = "linux")]
fn linux_mount_type(path: &Path) -> io::Result<String> {
    let contents = fs::read_to_string("/proc/self/mountinfo")?;
    linux_mount_type_from(path, &contents)
}
#[cfg(target_os = "linux")]
fn linux_mount_type_from(path: &Path, contents: &str) -> io::Result<String> {
    let mut best: Option<(usize, String)> = None;
    for line in contents.lines() {
        let Some((left, right)) = line.split_once(" - ") else { continue };
        let left_fields: Vec<&str> = left.split_whitespace().collect();
        let right_fields: Vec<&str> = right.split_whitespace().collect();
        if left_fields.len() < 5 || right_fields.is_empty() { continue; }
        let mount = decode_mount_field(left_fields[4]);
        let mount_path = Path::new(&mount);
        if path.starts_with(mount_path) {
            let len = mount_path.as_os_str().len();
            if best.as_ref().is_none_or(|(old, _)| len > *old) {
                best = Some((len, right_fields[0].to_owned()));
            }
        }
    }
    best.map(|(_, fs_type)| fs_type).ok_or_else(|| unsupported("no mount matches canonical path"))
}
#[cfg(all(target_os = "linux", feature = "test-api"))]
pub fn mount_type_for_test(path: &Path, mountinfo: &str) -> io::Result<String> {
    linux_mount_type_from(path, mountinfo)
}
#[cfg(target_os = "linux")]
fn decode_mount_field(value: &str) -> String {
    value.replace("\\040", " ").replace("\\011", "\t").replace("\\012", "\n").replace("\\134", "\\")
}
#[cfg(windows)]
fn check_local_filesystem(path: &Path) -> io::Result<()> {
    if windows_path_is_network(&path.to_string_lossy()) {
        return Err(unsupported("UNC paths are not supported"));
    }
    windows_api::require_fixed_local_volume(path)
}
#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_api {
    use std::ffi::c_void;
    use std::fs::OpenOptions;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;

    const DRIVE_FIXED: u32 = 3;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_READ_WRITE: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE;

    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        _low: u32,
        _high: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct ByHandleFileInformation {
        file_attributes: u32,
        _creation_time: FileTime,
        _last_access_time: FileTime,
        _last_write_time: FileTime,
        _volume_serial_number: u32,
        _file_size_high: u32,
        _file_size_low: u32,
        number_of_links: u32,
        _file_index_high: u32,
        _file_index_low: u32,
    }

    // SAFETY: The declarations match the documented kernel32 ABI. The wrappers below pass
    // nul-terminated strings, live handles, and initialized writable output buffers.
    #[link(name = "kernel32")]
    unsafe extern "system" {
        #[link_name = "GetVolumePathNameW"]
        fn get_volume_path_name_w(file_name: *const u16, volume_path_name: *mut u16, length: u32) -> i32;
        #[link_name = "GetDriveTypeW"]
        fn get_drive_type_w(root_path_name: *const u16) -> u32;
        #[link_name = "GetFileInformationByHandle"]
        fn get_file_information_by_handle(handle: *mut c_void, information: *mut ByHandleFileInformation) -> i32;
    }

    pub fn require_fixed_local_volume(path: &Path) -> io::Result<()> {
        let mut path_wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        path_wide.push(0);
        let mut volume_path = vec![0u16; 32_768];
        // SAFETY: Both buffers are valid for the duration of the call; input is nul-terminated
        // and output has 32,768 writable UTF-16 code units as required by the API contract.
        if unsafe {
            get_volume_path_name_w(path_wide.as_ptr(), volume_path.as_mut_ptr(), volume_path.len() as u32)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: GetVolumePathNameW succeeded and wrote a nul-terminated root in volume_path.
        let drive_type = unsafe { get_drive_type_w(volume_path.as_ptr()) };
        if drive_type == DRIVE_FIXED {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Instance paths require a fixed local drive",
            ))
        }
    }

    pub fn validate_regular_single_link(path: &Path) -> io::Result<()> {
        let mut options = OpenOptions::new();
        options.read(true);
        options.share_mode(FILE_SHARE_READ_WRITE);
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        let file = options.open(path)?;
        let mut information = ByHandleFileInformation::default();
        // SAFETY: The handle remains open and information points to a writable initialized
        // structure matching BY_HANDLE_FILE_INFORMATION.
        if unsafe { get_file_information_by_handle(file.as_raw_handle(), &mut information) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if information.file_attributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a regular file", path.display()),
            ));
        }
        if information.number_of_links != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} has {} hard links", path.display(), information.number_of_links),
            ));
        }
        Ok(())
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn check_local_filesystem(_path: &Path) -> io::Result<()> {
    Err(unsupported("no filesystem policy exists for this operating system"))
}

#[cfg(feature = "test-api")]
fn record_directory_flush(path: &Path, performed: bool) {
    if let Ok(mut events) = DIRECTORY_FLUSH_EVENTS.get_or_init(|| Mutex::new(Vec::new())).lock() {
        events.push((path.to_path_buf(), performed));
    }
}
#[cfg(feature = "test-api")]
pub fn take_directory_flush_observations() -> Vec<(PathBuf, bool)> {
    DIRECTORY_FLUSH_EVENTS.get_or_init(|| Mutex::new(Vec::new())).lock()
        .map(|mut events| std::mem::take(&mut *events)).unwrap_or_default()
}
#[cfg(feature = "test-api")]
pub fn clear_directory_flush_observations() {
    let _ = take_directory_flush_observations();
}
pub fn flush_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let result = File::open(path)?.sync_all();
        if result.is_ok() {
            #[cfg(feature = "test-api")]
            record_directory_flush(path, true);
        }
        result
    }
    #[cfg(windows)]
    {
        let _ = path;
        #[cfg(feature = "test-api")]
        record_directory_flush(path, false);
        Ok(())
    }
}
