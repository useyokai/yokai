//! Recovery snapshots are retained, including failed/partial attempts. Never remove automatically.
use std::fs::{self,OpenOptions};
use std::path::{Path,PathBuf};
use std::io::Write;
use rusqlite::{Connection,OpenFlags,config::DbConfig};
use crate::InstanceError;

const WARNING_BYTES:u64=1024*1024*1024;
const RESERVE_BYTES:u64=16*1024*1024;
#[derive(Clone,Debug,PartialEq,Eq)]
pub struct BackupUsage {
    pub total_bytes:u64,
    pub warning_threshold_bytes:u64,
    pub warning:bool,
}
fn error(e:impl ToString)->InstanceError{InstanceError::Snapshot(e.to_string())}
fn directory(root:&Path,create:bool)->Result<Option<PathBuf>,InstanceError>{
    let path=root.join("backups");
    if create {match fs::create_dir(&path){Ok(())=>{yokai_platform::location::flush_directory(root).map_err(error)?;},Err(e) if e.kind()==std::io::ErrorKind::AlreadyExists=>{},Err(e)=>return Err(error(e))}}
    let meta=match fs::symlink_metadata(&path){Ok(m)=>m,Err(e) if !create && e.kind()==std::io::ErrorKind::NotFound=>return Ok(None),Err(e)=>return Err(error(e))};
    if !meta.is_dir() || meta.file_type().is_symlink(){return Err(error("backups must be a real directory"));}
    let real=yokai_platform::location::validate_root(&path).map_err(error)?;
    if real.parent()!=Some(root){return Err(error("backups path escaped the Instance root"));}
    Ok(Some(real))
}
pub(crate) fn usage(root:&Path)->Result<BackupUsage,InstanceError>{
    let mut total=0u64;
    if let Some(dir)=directory(root,false)? {
        for entry in fs::read_dir(dir).map_err(error)? {
            let path=entry.map_err(error)?.path();let meta=fs::symlink_metadata(&path).map_err(error)?;
            yokai_platform::location::validate_regular_single_link(&path,&meta).map_err(error)?;
            total=total.checked_add(meta.len()).ok_or_else(||error("backup byte count overflow"))?;
        }
    }
    let threshold=WARNING_BYTES;
    #[cfg(feature="test-api")]
    let threshold=std::env::var("YOKAI_TEST_BACKUP_WARNING_BYTES").ok().and_then(|s|s.parse::<u64>().ok()).unwrap_or(threshold);
    Ok(policy(total,threshold))
}
pub(crate) fn policy(total:u64,threshold:u64)->BackupUsage {
    BackupUsage{total_bytes:total,warning_threshold_bytes:threshold,warning:total>threshold}
}
fn report(root:&Path)->Result<(),InstanceError>{
    let usage=usage(root)?;
    let id=root.file_name().and_then(|s|s.to_str()).ok_or_else(||error("snapshot report Instance ID unavailable"))?;
    let mut message=format!("INFO retained_backups total_bytes={} warning_threshold_bytes={} instance={id}\n",usage.total_bytes,usage.warning_threshold_bytes);
    if usage.warning {
        message.push_str(&format!("WARN retained_backups total_bytes={} warning_threshold_bytes={} instance={id} automatic_deletion=false\n",usage.total_bytes,usage.warning_threshold_bytes));
    }
    // Reporting is required before migration. A closed/failed sink aborts the upgrade,
    // leaving the verified snapshot available; avoid the panic-on-write eprintln macro.
    let mut sink=std::io::stderr().lock();
    sink.write_all(message.as_bytes()).and_then(|_|sink.flush()).map_err(error)
}
pub(crate) fn create(conn:&Connection,root:&Path,from:u32,to:u32)->Result<PathBuf,InstanceError>{
    if !conn.is_autocommit(){return Err(error("snapshot requested inside transaction"));}
    let page_size:i64=conn.query_row("PRAGMA page_size",[],|r|r.get(0)).map_err(error)?;
    let pages:i64=conn.query_row("PRAGMA page_count",[],|r|r.get(0)).map_err(error)?;
    let page_size=u64::try_from(page_size).map_err(error)?;
    let pages=u64::try_from(pages).map_err(error)?;
    let needed=pages.checked_mul(page_size).and_then(|n|n.checked_mul(2)).and_then(|n|n.checked_add(RESERVE_BYTES)).ok_or_else(||error("headroom size overflow"))?;
    let available=yokai_platform::location::available_bytes(root).map_err(error)?;
    #[cfg(feature="test-api")]
    let available=match std::env::var("YOKAI_TEST_HEADROOM_BYTES") {Ok(s)=>s.parse::<u64>().map_err(error)?.min(available),Err(_)=>available};
    if available<needed {return Err(error(format!("insufficient measured headroom: available={available}, required={needed}")));}
    // Sampled headroom is a refusal guard, not a reservation or an ENOSPC guarantee.
    let dir=directory(root,true)?.ok_or_else(||error("snapshot directory missing"))?;
    let stamp=crate::instance::utc_timestamp().map_err(error)?.replace(':',"");
    let nonce=yokai_platform::random_instance_id().map_err(error)?.to_hex();
    let path=dir.join(format!("pre-v{from}-to-v{to}-{stamp}-{nonce}.db"));
    let mut options=OpenOptions::new();options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options,0o600);
    #[cfg(windows)]
    std::os::windows::fs::OpenOptionsExt::share_mode(&mut options,3); // read/write, deny replacement while claimed
    let file=options.open(&path).map_err(error)?;
    crate::instance::hit_failpoint("snapshot-before-copy")?;
    let name=path.to_str().ok_or_else(||error("snapshot path is not UTF-8"))?;
    conn.execute("VACUUM INTO ?1",[name]).map_err(error)?;
    crate::instance::hit_failpoint("snapshot-before-sync")?;
    file.sync_all().map_err(error)?;
    yokai_platform::location::validate_regular_single_link(&path,&fs::symlink_metadata(&path).map_err(error)?).map_err(error)?;
    let check=Connection::open_with_flags(&path,OpenFlags::SQLITE_OPEN_READ_ONLY|OpenFlags::SQLITE_OPEN_NOFOLLOW|OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(error)?;
    check.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,true).map_err(error)?;
    check.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE,true).map_err(error)?;
    check.execute_batch("PRAGMA query_only=ON; PRAGMA trusted_schema=OFF;").map_err(error)?;
    let results:Vec<String>=check.prepare("PRAGMA integrity_check").map_err(error)?.query_map([],|r|r.get(0)).map_err(error)?.collect::<Result<_,_>>().map_err(error)?;
    let version:u32=check.query_row("PRAGMA user_version",[],|r|r.get(0)).map_err(error)?;
    if results!=["ok"] || version!=from || !crate::foreign_key_violations(&check).map_err(error)?.is_empty(){return Err(error("snapshot integrity/version/FK verification failed"));}
    drop(check);drop(file);
    yokai_platform::location::flush_directory(&dir).map_err(error)?;
    report(root)?;
    crate::instance::hit_failpoint("snapshot-verified")?;
    Ok(path)
}
