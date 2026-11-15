use sha2::{Digest, Sha256};
use std::ffi::{OsString, c_void};
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, FILETIME, HANDLE, LocalFree, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, PSID, TOKEN_ELEVATION,
    TOKEN_GROUPS, TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TOKEN_USER,
    TokenElevation, TokenIntegrityLevel, TokenLogonSid, TokenSessionId, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_SHARE_READ, GetFileInformationByHandle, SYNCHRONIZE,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetProcessTimes, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW, WaitForSingleObject,
};

fn denied(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, reason)
}

fn token_data(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> io::Result<Vec<usize>> {
    let mut size = 0;
    // Use pointer-aligned storage for the token structs and their internal SID pointers.
    let result = unsafe { GetTokenInformation(token, class, null_mut(), 0, &mut size) };
    let error = io::Error::last_os_error();
    if result != 0 || error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32)
        || size == 0 || size > 64 * 1024
    {
        return Err(error);
    }
    let mut words = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
    if unsafe { GetTokenInformation(token, class, words.as_mut_ptr().cast(), size, &mut size) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(words)
}

pub(crate) fn sid_string(sid: PSID) -> io::Result<String> {
    let mut text = null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // Windows returns an allocated, terminated string.
    let mut len = 0;
    while unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    let value = String::from_utf16(unsafe { std::slice::from_raw_parts(text, len) });
    unsafe { LocalFree(text.cast()) };
    value.map_err(io::Error::other)
}

#[derive(Eq, PartialEq)]
pub(crate) struct Principal {
    user: String,
    pub(crate) logon: String,
    session: u32,
    integrity: u32,
}

pub(crate) fn principal(process: HANDLE) -> io::Result<Principal> {
    let mut raw = null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut raw) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = unsafe { OwnedHandle::from_raw_handle(raw) };
    let token = token.as_raw_handle();
    let user = token_data(token, TokenUser)?;
    let logon = token_data(token, TokenLogonSid)?;
    let session = token_data(token, TokenSessionId)?;
    // TokenElevation requires the exact buffer size and may reject a sizing probe.
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let elevation_size = size_of::<TOKEN_ELEVATION>() as u32;
    let mut returned = 0;
    if unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&raw mut elevation).cast(),
            elevation_size,
            &mut returned,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if returned != elevation_size {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid token elevation size"));
    }
    let integrity = token_data(token, TokenIntegrityLevel)?;
    // Each successful query returns the corresponding token information structure.
    let user = unsafe { sid_string((*user.as_ptr().cast::<TOKEN_USER>()).User.Sid)? };
    let groups = unsafe { &*logon.as_ptr().cast::<TOKEN_GROUPS>() };
    if groups.GroupCount != 1 {
        return Err(denied("expected one logon SID"));
    }
    let logon = sid_string(groups.Groups[0].Sid)?;
    let session = unsafe { *session.as_ptr().cast::<u32>() };
    let label = unsafe { (*integrity.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()).Label.Sid };
    let count = unsafe { *GetSidSubAuthorityCount(label) };
    if count == 0 {
        return Err(denied("missing integrity RID"));
    }
    let integrity = unsafe { *GetSidSubAuthority(label, u32::from(count - 1)) };
    if elevation.TokenIsElevated != 0 || !(0x2000..0x3000).contains(&integrity) || session == 0 {
        return Err(denied("ordinary interactive Windows process required"));
    }
    Ok(Principal { user, logon, session, integrity })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub creation_time: u64,
}

fn creation_time(process: HANDLE) -> io::Result<u64> {
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default(),
    );
    if unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

pub fn current_identity() -> io::Result<ProcessIdentity> {
    let handle = unsafe { GetCurrentProcess() };
    Ok(ProcessIdentity { pid: unsafe { GetCurrentProcessId() }, creation_time: creation_time(handle)? })
}

fn file_key(file: &File) -> io::Result<(u32, u32, u32)> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((info.dwVolumeSerialNumber, info.nFileIndexHigh, info.nFileIndexLow))
}

/// Enrollment holds an image open without write/delete sharing.
pub struct ImagePin {
    path: PathBuf,
    file: File,
}

impl ImagePin {
    pub fn open(path: &Path, expected_sha256: &str) -> io::Result<Self> {
        if !path.is_absolute() || expected_sha256.len() != 64 {
            return Err(denied("invalid enrolled image"));
        }
        let path = path.canonicalize()?;
        let mut file = OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(&path)?;
        if !file.metadata()?.is_file() || file.metadata()?.len() > 64 * 1024 * 1024 {
            return Err(denied("invalid enrolled image size"));
        }
        let mut digest = Sha256::new();
        let mut block = [0; 8192];
        loop {
            let n = file.read(&mut block)?;
            if n == 0 {
                break;
            }
            digest.update(&block[..n]);
        }
        let actual: String = digest.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
        if actual != expected_sha256 {
            return Err(denied("enrolled image digest mismatch"));
        }
        Ok(Self { path, file })
    }
}

pub struct Peer {
    handle: OwnedHandle,
    pub identity: ProcessIdentity,
}

impl Peer {
    pub(crate) fn check(pid: u32, expected: Option<ProcessIdentity>, image: &ImagePin) -> io::Result<Self> {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        let raw = handle.as_raw_handle();
        let identity = ProcessIdentity { pid, creation_time: creation_time(raw)? };
        if expected.is_some_and(|expected| expected != identity)
            || principal(raw)? != principal(unsafe { GetCurrentProcess() })?
        {
            return Err(denied("pipe peer identity mismatch"));
        }
        let mut name = vec![0u16; 32768];
        let mut len = name.len() as u32;
        if unsafe { QueryFullProcessImageNameW(raw, 0, name.as_mut_ptr(), &mut len) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let path = PathBuf::from(OsString::from_wide(&name[..len as usize])).canonicalize()?;
        let actual = OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(&path)?;
        if path != image.path || file_key(&actual)? != file_key(&image.file)? {
            return Err(denied("pipe peer image mismatch"));
        }
        let peer = Self { handle, identity };
        peer.check_alive()?;
        Ok(peer)
    }

    pub fn check_alive(&self) -> io::Result<()> {
        if unsafe { WaitForSingleObject(self.handle.as_raw_handle(), 0) } != WAIT_TIMEOUT {
            return Err(denied("pipe peer liveness unconfirmed"));
        }
        Ok(())
    }
}

pub(crate) fn free_local(pointer: *mut c_void) {
    unsafe { LocalFree(pointer) };
}
