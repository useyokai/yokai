//! Test-only native process resources; snapshots are owned and never cached.
use std::ffi::c_void;
use std::io;

type Handle = *mut c_void;
#[repr(C)]
struct ThreadEntry {
    size: u32,
    usage: u32,
    thread_id: u32,
    owner_pid: u32,
    base_priority: i32,
    delta_priority: i32,
    flags: u32,
}
const _: () = assert!(std::mem::size_of::<ThreadEntry>() == 28);
impl ThreadEntry {
    fn new() -> Self {
        Self { size: 28, usage: 0, thread_id: 0, owner_pid: 0,
            base_priority: 0, delta_priority: 0, flags: 0 }
    }
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> Handle;
    fn GetProcessHandleCount(process: Handle, count: *mut u32) -> i32;
    fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> Handle;
    fn Thread32First(snapshot: Handle, entry: *mut ThreadEntry) -> i32;
    fn Thread32Next(snapshot: Handle, entry: *mut ThreadEntry) -> i32;
    fn CloseHandle(handle: Handle) -> i32;
}

struct Snapshot(Handle);
impl Snapshot {
    fn close(mut self) -> io::Result<()> {
        // Take ownership before closing; a failed close is an error, never a count.
        let handle = std::mem::replace(&mut self.0, std::ptr::null_mut());
        if unsafe { CloseHandle(handle) } == 0 { Err(io::Error::last_os_error()) }
        else { Ok(()) }
    }
}
impl Drop for Snapshot {
    fn drop(&mut self) {
        if !self.0.is_null() { unsafe { CloseHandle(self.0); } }
    }
}
fn handles(process: Handle) -> io::Result<usize> {
    let mut count = 0;
    // The pseudo handle is borrowed and must never be closed.
    if unsafe { GetProcessHandleCount(process, &mut count) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(count as usize)
}
fn threads(snapshot: Handle) -> io::Result<usize> {
    let mut entry = ThreadEntry::new();
    if unsafe { Thread32First(snapshot, &mut entry) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut own = 0;
    for _ in 0..1_000_000 {
        // The API may return a shorter structure; owner_pid ends at byte16.
        if entry.size < 16 || entry.size > 28 {
            return Err(io::Error::other("short or invalid THREADENTRY32"));
        }
        if entry.owner_pid == std::process::id() { own += 1; }
        entry.size = 28;
        if unsafe { Thread32Next(snapshot, &mut entry) } == 0 {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(18) { Ok(own) } else { Err(error) };
        }
    }
    Err(io::Error::other("thread snapshot enumeration limit"))
}

pub fn sample() -> io::Result<(usize, usize)> {
    // Count before creating the snapshot so it cannot inflate its own sample.
    let handles = handles(unsafe { GetCurrentProcess() })?;
    let raw = unsafe { CreateToolhelp32Snapshot(4, 0) }; // TH32CS_SNAPTHREAD
    if raw == -1isize as Handle { return Err(io::Error::last_os_error()); }
    if raw.is_null() { return Err(io::Error::other("null thread snapshot")); }
    let snapshot = Snapshot(raw);
    let measured = threads(snapshot.0);
    let closed = snapshot.close();
    let threads = match (measured, closed) {
        (Ok(count), Ok(())) => count,
        (Err(error), Ok(())) | (Ok(_), Err(error)) => return Err(error),
        (Err(error), Err(close)) => return Err(io::Error::other(format!("{error}; snapshot close: {close}"))),
    };
    if handles == 0 || threads == 0 { return Err(io::Error::other("resource counter returned zero")); }
    Ok((handles, threads))
}

#[allow(dead_code)] // The lifecycle binary only consumes sample().
pub fn invalid_handle_errors() -> io::Result<(i32, i32)> {
    let process = match handles(std::ptr::null_mut()) {
        Err(error) => error, Ok(_) => return Err(io::Error::other("null process unexpectedly accepted")),
    };
    let snapshot = match threads(std::ptr::null_mut()) {
        Err(error) => error, Ok(_) => return Err(io::Error::other("null snapshot unexpectedly accepted")),
    };
    Ok((process.raw_os_error().ok_or_else(|| io::Error::other("missing process native error"))?,
        snapshot.raw_os_error().ok_or_else(|| io::Error::other("missing snapshot native error"))?))
}
