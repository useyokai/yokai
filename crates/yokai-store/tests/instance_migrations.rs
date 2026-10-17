//! Versioned Instance storage checks against the production lifecycle API.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{ExitCode,Command,Stdio};
use std::time::{Duration,Instant};
use rusqlite::Connection;
use rusqlite::config::DbConfig;
use yokai_store as store;

fn main() -> ExitCode {
    let dumpable = yokai_testsupport::make_non_dumpable();
    println!("{}", dumpable.line());
    if !dumpable.permits_checks() { return ExitCode::from(2); }
    let args:Vec<String>=std::env::args().collect();
    if args.get(1).map(String::as_str)==Some("__child") {return child(&args);}
    let checks: &[(&str,fn()->Result<String,String>)]=&[
        ("MG-1",check_new_instance),("MG-2",check_reopen),("MG-3",check_future),
        ("MG-4",check_divergence),("MG-11",check_disagreement),("SV-3",check_identity),
        ("UP-1",check_upgrade),("UP-4",check_snapshot_failure),("SV-5",check_hardened),
        ("MG-5",check_failed_migration),("MG-6",check_interruption_v2),("MG-7",check_forward),
        ("MG-8",check_headroom),("MG-9",check_lint),("MG-12",check_retention),
        ("UP-2",check_old_refusal),("UP-3",check_interruption_v1),
        ("TX-FK",check_transaction_fk),
    ];
    let mut failed=0;
    println!("INFO MG-10 not_applicable: owned runner; no refinery upgrade is adopted or tested");
    for (id,run) in checks { match run() {
        Ok(detail)=>println!("CHECK {id} PASS {detail}"),
        Err(error)=>{println!("CHECK {id} FAIL {error}");failed+=1;}
    }}
    if failed==0 { println!("RESULT PASS {}/{}",checks.len(),checks.len());ExitCode::SUCCESS }
    else {println!("RESULT FAIL {failed}/{}",checks.len());ExitCode::FAILURE}
}

fn parent(label:&str)->Result<PathBuf,String> {
    let base=PathBuf::from(std::env::var_os("YOKAI_TEST_DATA_DIR").ok_or("test data directory missing")?);
    let parent=base.join(format!("instance_migrations-{}-{label}",std::process::id()));
    fs::create_dir(&parent).map_err(|e|e.to_string())?;Ok(parent)
}
fn root(label:&str)->Result<PathBuf,String> {
    let instance=store::create_instance(&parent(label)?).map_err(|e|e.to_string())?;
    let root=instance.root().to_owned();instance.close().map_err(|e|e.to_string())?;Ok(root)
}
fn edit(root:&Path,sql:&str,wal:bool)->Result<(),String> {
    let conn=Connection::open(root.join("instance.db")).map_err(|e|e.to_string())?;
    if wal {conn.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,true).map_err(|e|e.to_string())?;}
    conn.execute_batch(sql).map_err(|e|e.to_string())
}
fn legacy(label:&str)->Result<PathBuf,String> {
    let root=root(label)?;
    edit(&root,"DROP TABLE events; DROP TABLE jobs; DROP TABLE commands; DROP TABLE runs; DROP TABLE runtime_epoch; DROP TABLE instance_meta; DROP TABLE yokai_schema_history; PRAGMA user_version=0;",false)?;
    Ok(root)
}
fn v1_root(label:&str)->Result<PathBuf,String> {
    let root=root(label)?;
    edit(&root,"DROP TABLE events; DROP TABLE jobs; DROP TABLE commands; DROP TABLE runs; DROP TABLE runtime_epoch; DELETE FROM yokai_schema_history WHERE version=2; PRAGMA user_version=1;",false)?;
    Ok(root)
}
fn protected(root:&Path)->Result<Vec<Option<Vec<u8>>>,String> {
    ["instance.db","instance.db-wal"].iter().map(|p|match fs::read(root.join(p)) {
        Ok(v)=>Ok(Some(v)),Err(e) if e.kind()==std::io::ErrorKind::NotFound=>Ok(None),Err(e)=>Err(e.to_string())
    }).collect()
}
fn refused(root:&Path,expected:&str)->Result<(),String> {
    let before=protected(root)?;
    let error=match store::open_instance(root){Ok(i)=>{i.close().map_err(|e|e.to_string())?;return Err("incompatible database accepted".into());},Err(e)=>e.to_string()};
    if !error.contains(expected) || before!=protected(root)? {return Err(format!("refusal={error}; expected={expected}; protected bytes changed={}",before!=protected(root)?));}
    Ok(())
}
fn check_reopen()->Result<String,String> {
    for wal in [false,true] {
        let root=root(if wal{"reopen-wal"}else{"reopen-main"})?;
        if wal {edit(&root,"UPDATE instance_meta SET last_health_check=NULL;",true)?;}
        let read=||->Result<(i64,i64,String),String>{let c=Connection::open_with_flags(root.join("instance.db"),rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e|e.to_string())?;
            c.query_row("SELECT (SELECT epoch FROM runtime_epoch),(SELECT count(*) FROM yokai_schema_history),(SELECT group_concat(checksum,'|') FROM (SELECT checksum FROM yokai_schema_history ORDER BY version))",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).map_err(|e|e.to_string())};
        let before=read()?;
        let instance=store::open_instance(&root).map_err(|e|e.to_string())?;
        if wal { store::test_api::no_checkpoint_on_close(&instance).map_err(|e|e.to_string())?; }
        instance.close().map_err(|e|e.to_string())?;
        let after=read()?;
        if after.0!=before.0+1 || after.1!=before.1 || after.2!=before.2 || root.join("backups").exists(){return Err(format!("current open changed migration history or did not advance epoch exactly once; wal_fixture={wal}; before={before:?}; after={after:?}"));}
    }
    Ok("current main/WAL migration history unchanged; epoch advanced exactly once; no migration/snapshot repeated".into())
}
fn check_future()->Result<String,String> {
    for wal in [false,true] {let root=root(if wal{"future-wal"}else{"future-main"})?;edit(&root,"PRAGMA user_version=3;",wal)?;refused(&root,"schema 3 is newer than supported maximum 2")?;}
    Ok("newer main and WAL states refused; protected bytes unchanged".into())
}
fn check_divergence()->Result<String,String> {
    for wal in [false,true] {let root=root(if wal{"drift-wal"}else{"drift-main"})?;edit(&root,"UPDATE yokai_schema_history SET checksum=printf('%064d',0);",wal)?;refused(&root,"migration history diverged")?;}
    Ok("changed checksum refused in both compatibility stages".into())
}
fn check_disagreement()->Result<String,String> {
    for wal in [false,true] {
        for (label,sql) in [("missing-history","DELETE FROM yokai_schema_history;"),("history-gap","UPDATE yokai_schema_history SET version=version+10;"),("version-disagrees","PRAGMA user_version=0;")] {
            let root=root(&format!("{label}-{wal}"))?;edit(&root,sql,wal)?;let before=protected(&root)?;
            let error=match store::open_instance(&root){Ok(i)=>{i.close().map_err(|e|e.to_string())?;return Err("inconsistent history accepted".into());},Err(e)=>e};
            let classified=if wal{matches!(&error,store::InstanceError::SchemaInconsistent(_))}
                else{matches!(&error,store::InstanceError::StageB{cause,..} if cause.contains("inconsistent schema"))};
            if !classified || before!=protected(&root)? {return Err(format!("{label}, wal={wal}: expected SchemaInconsistent, got {error}; protected data changed={}",before!=protected(&root)?));}
        }
    }
    Ok("missing/noncontiguous history and version disagreement classified SchemaInconsistent in both stages; DB/WAL unchanged".into())
}
fn check_identity()->Result<String,String> {
    for wal in [false,true] {let root=root(if wal{"identity-wal"}else{"identity-main"})?;edit(&root,"UPDATE instance_meta SET instance_id=zeroblob(16);",wal)?;refused(&root,"Instance identity")?;}
    Ok("database identity mismatch refused in both stages".into())
}
fn check_upgrade()->Result<String,String> {
    let root=legacy("legacy")?;
    let instance=store::open_instance(&root).map_err(|e|e.to_string())?;
    if store::test_api::user_version(&instance).map_err(|e|e.to_string())?!=2{return Err("legacy not upgraded".into());}
    instance.close().map_err(|e|e.to_string())?;
    let backups=fs::read_dir(root.join("backups")).map_err(|e|e.to_string())?.collect::<Result<Vec<_>,_>>().map_err(|e|e.to_string())?;
    if backups.len()!=2{return Err(format!("expected two snapshots, got {}",backups.len()));}
    let c=Connection::open_with_flags(backups.iter().find(|e| e.file_name().to_string_lossy().starts_with("pre-v0-")).ok_or("version0 snapshot missing")?.path(),rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e|e.to_string())?;
    let v:i64=c.query_row("PRAGMA user_version",[],|r|r.get(0)).map_err(|e|e.to_string())?;
    let integrity:String=c.query_row("PRAGMA integrity_check",[],|r|r.get(0)).map_err(|e|e.to_string())?;
    if v!=0 || integrity!="ok"{return Err("invalid pre-migration snapshot".into());}
    store::open_instance(&root).map_err(|e|e.to_string())?.close().map_err(|e|e.to_string())?;
    if fs::read_dir(root.join("backups")).map_err(|e|e.to_string())?.count()!=2{return Err("reopen repeated migration".into());}
    let old=PathBuf::from(std::env::var_os("YOKAI_TEST_V0_FIXTURE").ok_or("missing pinned old fixture input")?);
    let before=fs::read(old.join("instance.db")).map_err(|e|e.to_string())?;
    if store::test_api::sha256_chunks_for_test(&[&before])!="da63a52bdf62b3fc863995d79f497584be3ba97c1de4c55d1d301afbafab6cfc"{return Err("old fixture hash differs".into());}
    let copy=parent("accepted-fixture")?.join(old.file_name().ok_or("old fixture lacks name")?);
    fs::create_dir(&copy).map_err(|e|e.to_string())?;
    for name in ["instance.db","instance.id","instance.lock"] {fs::copy(old.join(name),copy.join(name)).map_err(|e|e.to_string())?;}
    let instance=store::open_instance(&copy).map_err(|e|e.to_string())?;
    if store::test_api::user_version(&instance).map_err(|e|e.to_string())?!=2{return Err("accepted fixture not upgraded".into());}
    instance.close().map_err(|e|e.to_string())?;
    if before!=fs::read(old.join("instance.db")).map_err(|e|e.to_string())?{return Err("preserved original fixture changed".into());}
    Ok("synthetic and preserved accepted version0 fixtures upgraded once; retained snapshots passed integrity".into())
}
fn check_snapshot_failure()->Result<String,String> {
    let root=legacy("backup-denied")?;
    fs::write(root.join("backups"),b"blocked").map_err(|e|e.to_string())?;
    refused(&root,"snapshot")?;
    Ok("unwritable snapshot destination refused; database unchanged; no physical ENOSPC claim".into())
}
fn check_hardened()->Result<String,String> {
    let instance=store::create_instance(&parent("settings")?).map_err(|e|e.to_string())?;
    let settings=store::test_api::hardened_settings(&instance).map_err(|e|e.to_string())?;
    instance.close().map_err(|e|e.to_string())?;
    if settings!=(0,true,0,1000){return Err(format!("actual writable settings={settings:?}; expected=(0,true,0,1000)"));}
    Ok(format!("trusted_schema/DEFENSIVE/auto_vacuum/wal_autocheckpoint={settings:?}"))
}

fn child(args:&[String])->ExitCode{
    if args.len()!=5{return ExitCode::from(2);}
    let count=match args[3].parse::<u32>(){Ok(n)=>n,Err(_)=>return ExitCode::from(2)};
    match store::test_api::open_test_versions(Path::new(&args[2]),count,args[4]=="fail") {
        Ok(i)=>{
            if std::env::var("YOKAI_TEST_BACKUP_WARNING_BYTES").ok().as_deref()==Some("1") {
                match i.backup_usage(){Ok(u) if u.warning && u.warning_threshold_bytes==1 && u.total_bytes>0=>{},other=>{eprintln!("low-threshold warning assertion failed: {other:?}");return ExitCode::from(4);}}
            }
            match i.close(){Ok(())=>ExitCode::SUCCESS,Err(e)=>{eprintln!("{e}");ExitCode::from(4)}}
        },
        Err(e)=>{eprintln!("{e}");ExitCode::from(4)}
    }
}
fn command(root:&Path,latest:u32,fail:bool)->Result<Command,String>{
    let mut c=Command::new(std::env::current_exe().map_err(|e|e.to_string())?);
    c.arg("__child").arg(root).arg(latest.to_string()).arg(if fail{"fail"}else{"ok"});
    c.stdin(Stdio::null());Ok(c)
}
fn state(root:&Path)->Result<(i64,i64),String>{
    let c=Connection::open_with_flags(root.join("instance.db"),rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e|e.to_string())?;
    c.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,true).map_err(|e|e.to_string())?;
    let version=c.query_row("PRAGMA user_version",[],|r|r.get(0)).map_err(|e|e.to_string())?;
    let history=if version==0{0}else{c.query_row("SELECT count(*) FROM yokai_schema_history",[],|r|r.get(0)).map_err(|e|e.to_string())?};
    Ok((version,history))
}
fn snapshots(root:&Path)->Result<Vec<(PathBuf,Vec<u8>)>,String>{
    let mut result=Vec::new();
    for e in fs::read_dir(root.join("backups")).map_err(|e|e.to_string())?{let p=e.map_err(|e|e.to_string())?.path();result.push((p.clone(),fs::read(p).map_err(|e|e.to_string())?));}
    result.sort_by(|a,b|a.0.cmp(&b.0));Ok(result)
}
fn check_failed_migration()->Result<String,String>{
    let root=root("failed-v2")?;let before=state(&root)?;
    let result=store::test_api::open_test_versions(&root,3,true);
    let e=match result{Ok(i)=>{i.close().map_err(|e|e.to_string())?;return Err("failing test migration unexpectedly succeeded".into());},Err(e)=>e.to_string()};
    if !e.contains("migration failed") || state(&root)?!=before || snapshots(&root)?.len()!=1{return Err(format!("failure={e}; version/history or snapshots incorrect"));}
    Ok("SQL failure rolled back test-only V3 schema/history/version; verified pre-V3 snapshot retained".into())
}
fn check_forward()->Result<String,String>{
    let root=root("forward-v2")?;
    let i=store::test_api::open_test_versions(&root,3,false).map_err(|e|e.to_string())?;
    i.close().map_err(|e|e.to_string())?;
    if state(&root)?!=(3,3) || snapshots(&root)?.len()!=1{return Err("forward migration/history/snapshot missing".into());}
    Ok("test-only V3 migrated prior version with retained snapshot".into())
}
fn check_headroom()->Result<String,String>{
    let root=legacy("low-headroom")?;let before=protected(&root)?;
    let out=command(&root,1,false)?.env("YOKAI_TEST_HEADROOM_BYTES","0").output().map_err(|e|e.to_string())?;
    if out.status.code()!=Some(4) || !String::from_utf8_lossy(&out.stderr).contains("insufficient measured headroom") || before!=protected(&root)? || root.join("backups").exists(){return Err(format!("headroom refusal missing: {}",String::from_utf8_lossy(&out.stderr)));}
    let root=legacy("snapshot-sync-failure")?;let before=protected(&root)?;
    let out=command(&root,1,false)?.env("YOKAI_TEST_FAILPOINT","snapshot-before-sync:error").output().map_err(|e|e.to_string())?;
    if out.status.code()!=Some(4) || !String::from_utf8_lossy(&out.stderr).contains("snapshot-before-sync") || before!=protected(&root)? || snapshots(&root)?.len()!=1{return Err("snapshot pre-sync failure did not abort migration and retain evidence".into());}
    Ok("low measured headroom and pre-sync snapshot failure refused before migration; protected files unchanged; partial snapshot retained".into())
}
fn check_lint()->Result<String,String>{
    for sql in ["VACUUM; PRAGMA user_version = 1;","ATTACH 'x' AS other; PRAGMA user_version = 1;","PRAGMA journal_mode=WAL; PRAGMA user_version = 1;","PRAGMA foreign_keys=OFF; PRAGMA user_version = 1;","PRAGMA writable_schema=ON; PRAGMA user_version = 1;","BEGIN; PRAGMA user_version = 1; COMMIT;","SAVEPOINT x; PRAGMA user_version = 1;","PRAGMA user_version = 2;","PRAGMA user_version = 1; SELECT 1;","PRAGMA user_version = 1; /*"] {
        if store::test_api::lint_migration(sql,1).is_ok(){return Err(format!("lint accepted {sql}"));}
    }
    store::test_api::lint_migration("-- VACUUM;\nCREATE TABLE x(v TEXT DEFAULT 'COMMIT;'); /* ATTACH */ PRAGMA user_version = 1;",1)?;
    Ok("forbidden statements/transaction escape/wrong terminal version rejected; strings/comments parsed".into())
}
fn check_retention()->Result<String,String>{
    let root=legacy("retention")?;let mut previous=Vec::new();let mut gaps=Vec::new();
    for version in 1..=4 {
        let out=command(&root,version,false)?.env("YOKAI_TEST_BACKUP_WARNING_BYTES","1").output().map_err(|e|e.to_string())?;
        if !out.status.success(){return Err(format!("upgrade {version}: {}",String::from_utf8_lossy(&out.stderr)));}
        let text=String::from_utf8_lossy(&out.stderr);
        if text.lines().filter(|l|l.starts_with("INFO retained_backups total_bytes=")).count()!=1
            || text.lines().filter(|l|l.starts_with("WARN retained_backups total_bytes=")).count()!=1 {
            gaps.push(format!("upgrade {version} did not emit the size report and low-threshold warning"));
        }
        let next=snapshots(&root)?;
        if next.len()!=version as usize || previous.iter().any(|p|!next.contains(p)){return Err("snapshot removed/changed or count incorrect".into());}
        previous=next;
    }
    let i=store::test_api::open_test_versions(&root,4,false).map_err(|e|e.to_string())?;
    let usage=i.backup_usage().map_err(|e|e.to_string())?;i.close().map_err(|e|e.to_string())?;
    let total=previous.iter().map(|(_,b)|b.len() as u64).sum::<u64>();
    if usage.total_bytes!=total || usage.warning{return Err("normal threshold byte report incorrect".into());}
    let c=Connection::open(root.join("instance.db")).map_err(|e|e.to_string())?;
    let value:String=c.query_row("SELECT value FROM migration_probe WHERE id=1",[],|r|r.get(0)).map_err(|e|e.to_string())?;
    if value!="retained" || state(&root)?!=(4,4){return Err("successive logical data/history mismatch".into());}
    let failed=root_for_notification_failure()?;
    let out=command(&failed,3,true)?.env("YOKAI_TEST_BACKUP_WARNING_BYTES","1").output().map_err(|e|e.to_string())?;
    let text=String::from_utf8_lossy(&out.stderr);
    if out.status.code()!=Some(4) || !text.contains("INFO retained_backups total_bytes=") || !text.contains("WARN retained_backups total_bytes=") || !text.contains("migration failed") || state(&failed)?!=(2,2){gaps.push("snapshot notification missing before later SQL migration failure".into());}
    let threshold=1024*1024*1024;
    if store::test_api::backup_warning_for_test(threshold,threshold) || !store::test_api::backup_warning_for_test(threshold+1,threshold){gaps.push("warning must be above the threshold, with no warning at equality".into());}
    if !gaps.is_empty(){return Err(gaps.join("; "));}
    Ok(format!("four snapshots retained byte-identically; total_bytes={total}; actual size report/warning emitted after each snapshot and before later migration failure; warning only above threshold"))
}
fn root_for_notification_failure()->Result<PathBuf,String>{root("notification-before-failure")}
fn check_old_refusal()->Result<String,String>{
    let old=PathBuf::from(std::env::var_os("YOKAI_TEST_V0_BINARY").ok_or("missing pinned old executable")?);
    let bytes=fs::read(&old).map_err(|e|e.to_string())?;
    if store::test_api::sha256_chunks_for_test(&[&bytes])!="d9c546acba6046e384b1e0a3ff30a9fc195131244722b7d3f23559fe67292e08"{return Err("old executable hash differs".into());}
    let root=root("old-refusal")?;let before=protected(&root)?;
    let out=Command::new(old).arg("__child").arg("probe").arg(&root).stdin(Stdio::null()).output().map_err(|e|e.to_string())?;
    if out.status.code()!=Some(4) || !String::from_utf8_lossy(&out.stderr).contains("schema 2 is newer than supported maximum 0") || before!=protected(&root)?{return Err(format!("old probe={:?}: {}",out.status,String::from_utf8_lossy(&out.stderr)));}
    Ok("preserved accepted V0 PE refused V2; DB/WAL bytes unchanged; old executable not rebuilt".into())
}
fn interruptions(version:u32)->Result<String,String>{
    for mode in ["exit","wait"] {for repeat in 0..3 {
        let label=format!("interrupt-v{version}-{mode}-{repeat}");
        let root=if version==1{legacy(&label)?}else{v1_root(&label)?};
        let marker=root.parent().ok_or("root parent missing")?.join("ready");
        let mut c=command(&root,version,false)?;
        c.env("YOKAI_TEST_FAILPOINT",format!("migration-{version}-before-commit:{mode}")).env("YOKAI_TEST_READY_FILE",&marker);
        let mut child=c.stdout(Stdio::null()).stderr(Stdio::piped()).spawn().map_err(|e|e.to_string())?;
        if mode=="wait" {
            let until=Instant::now()+Duration::from_secs(10);
            while !marker.exists() && Instant::now()<until {
                if child.try_wait().map_err(|e|e.to_string())?.is_some(){return Err("child exited before injection boundary".into());}
                std::thread::sleep(Duration::from_millis(10));
            }
            if !marker.exists(){let _=child.kill();let _=child.wait();return Err("injection readiness timed out".into());}
            child.kill().map_err(|e|e.to_string())?;
        }
        let status=child.wait().map_err(|e|e.to_string())?;
        if (mode=="exit" && status.code()!=Some(86)) || (mode=="wait" && status.success()){return Err(format!("injection not observed: {status}"));}
        if state(&root)?!=(i64::from(version-1),i64::from(version-1)){return Err("partial migration committed after interruption".into());}
        let saved=snapshots(&root)?;
        if saved.len()!=1{return Err("interrupted migration snapshot absent".into());}
        let i=store::test_api::open_test_versions(&root,version,false).map_err(|e|e.to_string())?;i.close().map_err(|e|e.to_string())?;
        if state(&root)?!=(i64::from(version),i64::from(version)) || saved.iter().any(|p|!snapshots(&root).unwrap_or_default().contains(p)){return Err("recovery repeated history or removed prior snapshot".into());}
    }}
    Ok(format!("V{}->V{version}: exit and parent kill each3 times; previous committed state retained; reopen completed once; earlier snapshot retained",version-1))
}
fn check_interruption_v1()->Result<String,String>{interruptions(1)}
fn check_interruption_v2()->Result<String,String>{interruptions(2)}

fn check_transaction_fk()->Result<String,String>{
    let dir=parent("transaction-fk")?;
    let mut c=store::test_api::create_file(&dir.join("fk.db")).map_err(|e|e.to_string())?;
    c.execute_batch("CREATE TABLE p(id INTEGER PRIMARY KEY) STRICT; CREATE TABLE immediate_child(id INTEGER PRIMARY KEY, p INTEGER REFERENCES p(id)) STRICT; CREATE TABLE deferred_child(id INTEGER PRIMARY KEY, p INTEGER REFERENCES p(id) DEFERRABLE INITIALLY DEFERRED) STRICT;").map_err(|e|e.to_string())?;
    let immediate=c.execute("INSERT INTO immediate_child VALUES(1,99)",[]).err().ok_or("immediate FK insert accepted")?;
    if store::ext_code(&immediate)!=Some(787){return Err(format!("wrong immediate FK error: {immediate}"));}
    let tx=c.transaction().map_err(|e|e.to_string())?;
    tx.execute("INSERT INTO deferred_child VALUES(1,99)",[]).map_err(|e|e.to_string())?;
    let deferred=tx.commit().err().ok_or("deferred FK commit accepted")?;
    let rows:i64=c.query_row("SELECT count(*) FROM deferred_child",[],|r|r.get(0)).map_err(|e|e.to_string())?;
    if store::ext_code(&deferred)!=Some(787) || rows!=0 || !c.is_autocommit(){return Err("failed deferred commit retained data or transaction".into());}
    Ok("actual immediate insert and deferred commit failed with extended code787; failed deferred transaction left no rows".into())
}

fn check_new_instance() -> Result<String, String> {
    let base = PathBuf::from(std::env::var_os("YOKAI_TEST_DATA_DIR").ok_or("test data directory missing")?);
    if !base.is_absolute() || !base.is_dir() { return Err("test directory must be absolute and exist".into()); }
    let parent = base.join(format!("instance_migrations-{}", std::process::id()));
    fs::create_dir(&parent).map_err(|e| e.to_string())?;
    println!("INFO test_data={}", parent.display());
    let instance = store::create_instance(&parent).map_err(|e| e.to_string())?;
    let root = instance.root().to_path_buf();
    let id = instance.id();
    instance.close().map_err(|e| e.to_string())?;
    let conn = Connection::open(root.join("instance.db")).map_err(|e| e.to_string())?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(|e| e.to_string())?;
    if version != 2 { return Err(format!("new Instance schema version={version}; expected 2")); }
    let stored_id: Vec<u8> = conn.query_row("SELECT instance_id FROM instance_meta WHERE id=1", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    let history: i64 = conn.query_row("SELECT count(*) FROM yokai_schema_history", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if stored_id.as_slice() != id.as_bytes() || history != 2 { return Err("identity/history binding failed".into()); }
    if root.join("backups").exists() { return Err("new Instance unexpectedly created a backup".into()); }
    Ok("new V1 identity/history followed by V2 committed; no pre-migration snapshot needed".into())
}
