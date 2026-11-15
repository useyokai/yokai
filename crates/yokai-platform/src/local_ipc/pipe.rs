use super::peer::{ImagePin, Peer, ProcessIdentity, free_local, principal, sid_string};
use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use windows_sys::Win32::Foundation::{
    ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SE_KERNEL_OBJECT,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, DACL_SECURITY_INFORMATION, GetAce, GetSecurityDescriptorControl,
    SECURITY_ATTRIBUTES, SE_DACL_PROTECTED,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ,
    FILE_GENERIC_WRITE, FILE_READ_ATTRIBUTES,
    FILE_READ_DATA, FILE_WRITE_DATA, MOVEFILE_REPLACE_EXISTING, MoveFileExW, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX, ReadFile, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, SYNCHRONIZE,
    WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
    GetNamedPipeServerProcessId, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT, PeekNamedPipe,
};
use windows_sys::Win32::System::Threading::{CreateEventW, GetCurrentProcess, WaitForSingleObject};

const PREFIX: &str = r"\\.\pipe\yokai-m1-";
const CLIENT_OPEN: u32 = FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION;
const SERVER_MODE: u32 = PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
const PIPE_BYTES: u32 = 4096;
const LOGON_ACCESS: u32 = FILE_GENERIC_READ | FILE_GENERIC_WRITE;

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

fn handle(raw: HANDLE) -> io::Result<OwnedHandle> {
    if raw.is_null() || raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // The caller supplies a newly acquired handle with ownership.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

#[derive(Clone, Debug)]
pub struct PipeName(String);

impl PipeName {
    pub fn from_nonce(nonce: &str) -> io::Result<Self> {
        if nonce.len() != 32 || !nonce.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid local pipe nonce"));
        }
        Ok(Self(format!("{PREFIX}{nonce}")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug)]
pub enum IoError {
    System(io::Error),
    Deadline,
    PendingRetained,
}

impl From<io::Error> for IoError {
    fn from(error: io::Error) -> Self {
        Self::System(error)
    }
}

impl fmt::Display for IoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::System(e) => e.fmt(f),
            Self::Deadline => f.write_str("pipe I/O deadline"),
            Self::PendingRetained => f.write_str("unconfirmed pipe I/O; owner retained"),
        }
    }
}

impl std::error::Error for IoError {}

#[derive(Clone, Copy)]
enum Operation {
    Connect,
    Read,
    Write,
}

struct Pending {
    _event: OwnedHandle,
    overlapped: Box<OVERLAPPED>,
    bytes: Vec<u8>,
}

pub struct Pipe {
    handle: Arc<OwnedHandle>,
    retained: AtomicBool,
}

impl Pipe {
    fn new(handle: OwnedHandle) -> Self {
        Self { handle: Arc::new(handle), retained: AtomicBool::new(false) }
    }

    fn raw(&self) -> HANDLE {
        self.handle.as_raw_handle()
    }

    pub fn retained(&self) -> bool {
        self.retained.load(Ordering::Acquire)
    }

    pub fn available(&self) -> Result<u32, IoError> {
        if self.retained() {
            return Err(IoError::PendingRetained);
        }
        let mut count = 0;
        if unsafe { PeekNamedPipe(self.raw(), null_mut(), 0, null_mut(), &mut count, null_mut()) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(count)
    }

    fn perform(&self, op: Operation, bytes: Vec<u8>, deadline: Instant) -> Result<(Vec<u8>, usize), IoError> {
        if self.retained() {
            return Err(IoError::PendingRetained);
        }
        if Instant::now() >= deadline {
            return Err(IoError::Deadline);
        }
        let event = handle(unsafe { CreateEventW(null(), 1, 0, null()) })?;
        let mut pending = Pending {
            overlapped: Box::new(OVERLAPPED { hEvent: event.as_raw_handle(), ..Default::default() }),
            _event: event,
            bytes,
        };
        let raw = self.raw();
        let overlapped = &raw mut *pending.overlapped;
        let length = u32::try_from(pending.bytes.len()).map_err(io::Error::other)?;
        let mut count = 0;
        // Both heap allocations and the event stay live until the kernel completes this operation.
        let result = unsafe {
            match op {
                Operation::Connect => ConnectNamedPipe(raw, overlapped),
                Operation::Read => ReadFile(raw, pending.bytes.as_mut_ptr(), length, null_mut(), overlapped),
                Operation::Write => WriteFile(raw, pending.bytes.as_ptr(), length, null_mut(), overlapped),
            }
        };
        if result != 0 {
            if !matches!(op, Operation::Connect)
                && unsafe { GetOverlappedResult(raw, overlapped, &mut count, 0) } == 0
            {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(ERROR_IO_INCOMPLETE as i32) {
                    return Err(self.retain(pending));
                }
                return Err(error.into());
            }
            return Ok((pending.bytes, count as usize));
        }
        let error = io::Error::last_os_error();
        if matches!(op, Operation::Connect) && error.raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32) {
            return Ok((pending.bytes, 0));
        }
        if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
            return Err(error.into());
        }

        let timeout = deadline.saturating_duration_since(Instant::now()).as_millis().min(u32::MAX as u128 - 1) as u32;
        let wait = unsafe { WaitForSingleObject(pending._event.as_raw_handle(), timeout) };
        if wait == WAIT_OBJECT_0 {
            if unsafe { GetOverlappedResult(raw, overlapped, &mut count, 0) } != 0 {
                return Ok((pending.bytes, count as usize));
            }
            let completion_error = io::Error::last_os_error();
            if completion_error.raw_os_error() != Some(ERROR_IO_INCOMPLETE as i32) {
                return Err(completion_error.into());
            }
        }

        let failure = if wait == WAIT_TIMEOUT { IoError::Deadline } else { io::Error::last_os_error().into() };
        unsafe { CancelIoEx(raw, overlapped) };
        // No deadline extension authorizes application work. This is a short completion drain only.
        if unsafe { WaitForSingleObject(pending._event.as_raw_handle(), 100) } == WAIT_OBJECT_0 {
            if unsafe { GetOverlappedResult(raw, overlapped, &mut count, 0) } != 0
                || io::Error::last_os_error().raw_os_error() != Some(ERROR_IO_INCOMPLETE as i32)
            {
                return Err(failure);
            }
        }
        Err(self.retain(pending))
    }

    fn retain(&self, pending: Pending) -> IoError {
        self.retained.store(true, Ordering::Release);
        // There is no safe recovery in this process. Keep the allocations and pipe alive.
        std::mem::forget(pending);
        std::mem::forget(self.handle.clone());
        IoError::PendingRetained
    }

    pub fn read_exact(&self, output: &mut [u8], deadline: Instant) -> Result<(), IoError> {
        for piece in output.chunks_mut(PIPE_BYTES as usize) {
            let mut offset = 0;
            while offset < piece.len() {
                let (bytes, n) = self.perform(Operation::Read, vec![0; piece.len() - offset], deadline)?;
                if n == 0 {
                    return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into());
                }
                piece[offset..offset + n].copy_from_slice(&bytes[..n]);
                offset += n;
            }
        }
        Ok(())
    }

    pub fn write_all(&self, input: &[u8], deadline: Instant) -> Result<(), IoError> {
        for piece in input.chunks(PIPE_BYTES as usize) {
            let mut offset = 0;
            while offset < piece.len() {
                let (_, n) = self.perform(Operation::Write, piece[offset..].to_vec(), deadline)?;
                if n == 0 {
                    return Err(io::Error::from(io::ErrorKind::WriteZero).into());
                }
                offset += n;
            }
        }
        Ok(())
    }

    fn peer(&self, server: bool, image: &ImagePin, expected: Option<ProcessIdentity>) -> io::Result<Peer> {
        #[cfg(feature = "test-api")]
        match PEER_FAULT.with(|f| f.replace(0)) {
            1 => return Err(io::Error::new(io::ErrorKind::PermissionDenied, "synthetic missing peer identity")),
            2 => return Err(io::Error::from_raw_os_error(5)),
            _ => {}
        }
        let query = if server { GetNamedPipeServerProcessId } else { GetNamedPipeClientProcessId };
        let mut pid = 0;
        if unsafe { query(self.raw(), &mut pid) } == 0 || pid == 0 {
            return Err(io::Error::last_os_error());
        }
        let peer = Peer::check(pid, expected, image)?;
        if unsafe { query(self.raw(), &mut pid) } == 0 || pid != peer.identity.pid {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "pipe peer changed during authentication"));
        }
        peer.check_alive()?;
        Ok(peer)
    }
}

pub struct Listener {
    pipe: Pipe,
}

impl Listener {
    pub fn new(name: &PipeName) -> io::Result<Self> {
        let logon = principal(unsafe { GetCurrentProcess() })?.logon;
        let sddl = wide(OsStr::new(&format!("D:P(A;;0x{LOGON_ACCESS:x};;;{logon})")));
        let mut descriptor = null_mut();
        if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), 1, &mut descriptor, null_mut()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let attrs = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let name = wide(OsStr::new(name.as_str()));
        let result = handle(unsafe {
            CreateNamedPipeW(
                name.as_ptr(), PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
                SERVER_MODE, 1, PIPE_BYTES, PIPE_BYTES, 50, &attrs,
            )
        });
        free_local(descriptor);
        let pipe = Pipe::new(result?);
        verify_dacl(pipe.raw(), &logon)?;
        Ok(Self { pipe })
    }

    pub fn accept(&self, deadline: Instant) -> Result<(), IoError> {
        self.pipe.perform(Operation::Connect, Vec::new(), deadline)?;
        Ok(())
    }

    pub fn authenticate(&self, image: &ImagePin) -> io::Result<Peer> {
        self.pipe.peer(false, image, None)
    }

    pub fn pipe(&self) -> &Pipe {
        &self.pipe
    }

    pub fn disconnect(&self) -> Result<(), IoError> {
        if self.pipe.retained() {
            return Err(IoError::PendingRetained);
        }
        if unsafe { DisconnectNamedPipe(self.pipe.raw()) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }
}

fn verify_dacl(pipe: HANDLE, logon: &str) -> io::Result<()> {
    let mut descriptor = null_mut();
    let mut dacl = null_mut();
    let result = unsafe {
        GetSecurityInfo(pipe, SE_KERNEL_OBJECT, DACL_SECURITY_INFORMATION,
            null_mut(), null_mut(), &mut dacl, null_mut(), &mut descriptor)
    };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    let valid = (|| {
        let (mut control, mut revision) = (0, 0);
        if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0
            || control & SE_DACL_PROTECTED == 0 || dacl.is_null() || unsafe { (*dacl).AceCount } != 1
        {
            return Ok(false);
        }
        let mut ace = null_mut();
        if unsafe { GetAce(dacl, 0, &mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let ace = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        // ACCESS_ALLOWED_ACE_TYPE is zero. SidStart is the beginning of the embedded SID.
        Ok(ace.Header.AceType == 0 && ace.Header.AceFlags == 0 && ace.Mask == LOGON_ACCESS
            && sid_string((&raw const ace.SidStart).cast_mut().cast())? == logon)
    })();
    free_local(descriptor);
    if valid? {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "pipe DACL readback mismatch"))
    }
}

pub struct Client {
    pub pipe: Pipe,
    pub peer: Peer,
}

impl Client {
    pub fn connect(name: &PipeName, image: &ImagePin, expected: ProcessIdentity) -> io::Result<Self> {
        let name = wide(OsStr::new(name.as_str()));
        let flags = CLIENT_OPEN;
        #[cfg(feature = "test-api")]
        OPEN_CALLS.with(|v| { let mut v=v.borrow_mut(); assert!(v.len()<32); v.push(flags); });
        let handle = handle(unsafe {
            CreateFileW(
                name.as_ptr(), FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                0, null(), OPEN_EXISTING, flags, null_mut(),
            )
        })?;
        let pipe = Pipe::new(handle);
        let peer = pipe.peer(true, image, Some(expected))?;
        Ok(Self { pipe, peer })
    }
}

pub fn replace_file(temporary: &Path, target: &Path) -> io::Result<()> {
    if temporary.parent() != target.parent() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "expected same-directory publication"));
    }
    let temporary = wide(temporary.as_os_str());
    let target = wide(target.as_os_str());
    if unsafe { MoveFileExW(temporary.as_ptr(), target.as_ptr(), MOVEFILE_REPLACE_EXISTING) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(feature = "test-api")]
pub fn adapter_flags() -> (u32, u32) {
    (CLIENT_OPEN, SERVER_MODE)
}


#[cfg(feature = "test-api")]
thread_local! {
    static OPEN_CALLS: std::cell::RefCell<Vec<u32>> = const { std::cell::RefCell::new(Vec::new()) };
    static PEER_FAULT: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}
#[cfg(feature = "test-api")]
pub fn take_open_calls() -> Vec<u32> { OPEN_CALLS.with(|v| std::mem::take(&mut *v.borrow_mut())) }
#[cfg(feature = "test-api")]
pub fn fail_next_peer_query(kind: u8) { assert!(matches!(kind, 1 | 2)); PEER_FAULT.with(|v| v.set(kind)); }

#[cfg(feature = "test-api")]
impl Listener {
    /// Evaluate the actual descriptor using a synthetic SID-only Authz context.
    /// No logon, impersonation, account provisioning, or descriptor mutation.
    pub fn foreign_sid_denied(&self) -> io::Result<bool> {
        use windows_sys::Win32::Security::Authorization::*;
        use windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION;
        use windows_sys::Win32::Foundation::LUID;
        fn report(stage: &str, error: io::Error) -> io::Error {
            eprintln!("AUTHZ_CHECK_ERROR stage={stage} winerror={:?}", error.raw_os_error());
            error
        }
        fn release<T>(pointer: *mut std::ffi::c_void, stage: &str, result: io::Result<T>) -> io::Result<T> {
            if !unsafe { windows_sys::Win32::Foundation::LocalFree(pointer) }.is_null() {
                let cleanup = report(stage, io::Error::last_os_error());
                return match result { Err(first) => Err(first), Ok(_) => Err(cleanup) };
            }
            result
        }
        let logon = principal(unsafe { GetCurrentProcess() }).map_err(|e| report("principal", e))?.logon;
        verify_dacl(self.pipe.raw(), &logon).map_err(|e| report("verify_dacl", e))?;
        let (mut sd, mut owner, mut dacl) = (null_mut(), null_mut(), null_mut());
        let status = unsafe { GetSecurityInfo(self.pipe.raw(), SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner, null_mut(), &mut dacl, null_mut(), &mut sd) };
        if status != 0 { return Err(report("GetSecurityInfo", io::Error::from_raw_os_error(status as i32))); }
        let result = (|| {
            if sd.is_null() || owner.is_null() || dacl.is_null() {
                return Err(report("descriptor_components", io::Error::other("owner and DACL required")));
            }
            let mut manager = null_mut();
            if unsafe { AuthzInitializeResourceManager(AUTHZ_RM_FLAG_NO_AUDIT, None, None, None, null(), &mut manager) } == 0 {
                return Err(report("AuthzInitializeResourceManager", io::Error::last_os_error()));
            }
            let evaluate = |role: &str, sid_text: &str| -> io::Result<(u32, u32)> {
                let mut sid = null_mut();
                let text = wide(OsStr::new(sid_text));
                if unsafe { ConvertStringSidToSidW(text.as_ptr(), &mut sid) } == 0 {
                    return Err(report("ConvertStringSidToSidW", io::Error::last_os_error()));
                }
                let result = (|| {
                    let mut context = null_mut();
                    if unsafe { AuthzInitializeContextFromSid(AUTHZ_SKIP_TOKEN_GROUPS, sid, manager, null(), LUID::default(), null(), &mut context) } == 0 {
                        return Err(report("AuthzInitializeContextFromSid", io::Error::last_os_error()));
                    }
                    // All three one-element outputs outlive the synchronous access check.
                    let (mut granted, mut sacl, mut error) = (0, 0, 0);
                    let request = AUTHZ_ACCESS_REQUEST { DesiredAccess: FILE_READ_DATA | FILE_WRITE_DATA, ..Default::default() };
                    let mut reply = AUTHZ_ACCESS_REPLY { ResultListLength: 1, GrantedAccessMask: &mut granted,
                        SaclEvaluationResults: &mut sacl, Error: &mut error };
                    let ok = unsafe { AuthzAccessCheck(0, context, &request, null_mut(), sd, null(), 0, &mut reply, null_mut()) };
                    let result = if ok == 0 { Err(report("AuthzAccessCheck", io::Error::last_os_error())) }
                        else { eprintln!("AUTHZ_CHECK role={role} api_ok=1 granted={granted} error={error}"); Ok((granted, error)) };
                    if unsafe { AuthzFreeContext(context) } == 0 {
                        let cleanup = report("AuthzFreeContext", io::Error::last_os_error());
                        return match result { Err(first) => Err(first), Ok(_) => Err(cleanup) };
                    }
                    result
                })();
                release(sid, "LocalFree_sid", result)
            };
            let result = (|| {
                let foreign = evaluate("foreign", "S-1-5-21-10001-10002-10003-10004")?;
                let allowed = evaluate("logon", &logon)?;
                Ok(foreign == (0, 5) && allowed == (FILE_READ_DATA | FILE_WRITE_DATA, 0))
            })();
            if unsafe { AuthzFreeResourceManager(manager) } == 0 {
                let cleanup = report("AuthzFreeResourceManager", io::Error::last_os_error());
                return match result { Err(first) => Err(first), Ok(_) => Err(cleanup) };
            }
            result
        })();
        release(sd, "LocalFree_descriptor", result)
    }
}
