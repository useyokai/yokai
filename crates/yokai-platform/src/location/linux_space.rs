//! Linux x86_64 glibc statvfs ABI; other targets fail closed in the caller.
use std::{ffi::CString,io,path::Path};
use std::os::unix::ffi::OsStrExt;
#[repr(C)]
#[derive(Default)]
struct Statvfs {
    bsize:u64,frsize:u64,blocks:u64,bfree:u64,bavail:u64,files:u64,ffree:u64,favail:u64,
    fsid:u64,flag:u64,namemax:u64,fs_type:u32,spare:[i32;5],
}
const _:()=assert!(std::mem::size_of::<Statvfs>()==112);
unsafe extern "C"{fn statvfs(path:*const std::ffi::c_char,buffer:*mut Statvfs)->std::ffi::c_int;}
pub(super) fn available_bytes(path:&Path)->io::Result<u64>{
    let path=CString::new(path.as_os_str().as_bytes()).map_err(|_|io::Error::new(io::ErrorKind::InvalidInput,"NUL in path"))?;
    let mut value=Statvfs::default();
    // SAFETY: x86_64 glibc layout matches bits/statvfs.h; live C path and writable aligned buffer.
    if unsafe{statvfs(path.as_ptr(),&mut value)}!=0{return Err(io::Error::last_os_error());}
    value.bavail.checked_mul(value.frsize).ok_or_else(||io::Error::other("free-space byte count overflow"))
}
