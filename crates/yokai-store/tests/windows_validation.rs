//! Native Windows qualification observations. Every filesystem mutation is a test fixture.
#[cfg(windows)]
fn main() -> std::process::ExitCode { native::run() }
#[cfg(not(windows))]
fn main() -> std::process::ExitCode {
    println!("RESULT NOT_CHECKED windows_validation requires native Windows");
    std::process::ExitCode::SUCCESS
}

#[cfg(windows)]
#[path = "support/mod.rs"]
mod support;

#[cfg(windows)]
#[path = "support/windows_process.rs"]
mod windows_process;

#[cfg(windows)]
mod native {
    use super::{support,windows_process};
    use std::collections::BTreeMap;
    use std::fs::{self,File,OpenOptions};
    use std::io::{Read,Seek,SeekFrom,Write};
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::{Path,PathBuf};
    use std::process::{Child,Command,ExitCode,ExitStatus,Stdio};
    use std::os::windows::process::CommandExt;
    use std::sync::atomic::{AtomicUsize,Ordering};
    use std::time::{Duration,Instant};
    use rusqlite::{Connection,OpenFlags,config::DbConfig};
    use yokai_store::{self as store,AcceptOutcome,CommandRequest,InstanceError,Certainty,StoreState,Phase};
    use yokai_types::CommandId;
    type Check=fn()->Result<String,String>;
    static NEXT:AtomicUsize=AtomicUsize::new(0);

    pub fn run()->ExitCode {
        let dump=yokai_testsupport::make_non_dumpable();println!("{}",dump.line());
        if !dump.permits_checks(){return ExitCode::from(2);}
        let args:Vec<String>=std::env::args().collect();
        if args.get(1).is_some_and(|a|a.starts_with("__")){return child_mode(&args);}
        let checks:&[(&str,Check)]=&[
            ("WV-REPLAY-ROLLBACK",replay_rollback),
            ("WV-STARTUP-CLOSE",startup_close),
            ("WV-V2-DRIFT",v2_drift),
            ("WV-V2-COMPAT",v2_compat),
            ("WV-SQLITE-OPTIONS",sqlite_options),
            ("WV-ALIAS",aliases),
            ("WV-OPEN-FILES",open_files),
            ("WV-SQLITE-FILES",sqlite_files),
            ("WV-PREFLIGHT-ACCESS",preflight_access),
            ("WV-PREFLIGHT-DAMAGE",preflight_damage),
            ("WV-PREFLIGHT-CONSISTENCY",preflight_consistency),
            ("WV-PREFLIGHT-ACL",preflight_acl),
            ("WV-STAGEB-REFUSALS",stageb_refusals),
            ("WV-STAGEB-LEGACY",stageb_legacy),
            ("WV-PREFLIGHT-SEMANTIC",preflight_semantic),
            ("WV-PREFLIGHT-INTEGRITY",preflight_integrity),
            ("WV-LOCK-SOAK",lock_soak),
            ("WV-RESOURCE-COUNTS",resource_counts),
            ("WV-PREFLIGHT-CORRUPTION",preflight_corruption),
            ("WV-RAW-HEADER",raw_headers),
        ];
        let selected:Vec<_>=match args.as_slice(){
            [_]=>checks.iter().collect(),
            [_,flag,id] if flag=="--only" && checks.iter().any(|(known,_)|known==id)=>checks.iter().filter(|(known,_)|known==id).collect(),
            _=>{eprintln!("expected no arguments or --only <known check ID>");return ExitCode::from(2);}
        };
        let mut failed=0;
        for (id,check) in &selected {match check(){Ok(detail)=>println!("CHECK {id} PASS {detail}"),Err(error)=>{println!("CHECK {id} FAIL {error}");failed+=1;}}}
        if failed==0 {println!("RESULT PASS {}/{}",selected.len(),selected.len());ExitCode::SUCCESS}
        else {println!("RESULT FAIL {failed}/{}",selected.len());ExitCode::FAILURE}
    }
    fn resource_counts()->Result<String,String>{
        use std::sync::mpsc;
        struct Workers { releases:Vec<mpsc::Sender<()>>, joins:Vec<std::thread::JoinHandle<()>> }
        impl Workers {
            fn join(&mut self)->Result<(),String>{
                self.releases.clear();
                let failed=self.joins.drain(..).map(|j|j.join().is_err()).fold(false,|a,b|a|b);
                if failed {Err("resource control worker panicked".into())} else {Ok(())}
            }
        }
        impl Drop for Workers {fn drop(&mut self){let _=self.join();}}
        let fixture=parent("resource-counts")?;
        let sample=||windows_process::sample().map_err(|e|e.to_string());
        let baseline=sample()?;
        for _ in 0..32 {if sample()?!=baseline{return Err("repeated snapshots changed resource counts".into());}}
        let errors=windows_process::invalid_handle_errors().map_err(|e|e.to_string())?;
        if errors!=(6,6) || sample()?!=baseline {return Err(format!("invalid native query controls={errors:?}"));}
        let mut files=Vec::new();
        for n in 0..8 {files.push(File::create_new(fixture.join(format!("held-{n}.txt"))).map_err(|e|e.to_string())?);}
        let held_files=sample()?;
        if held_files!=(baseline.0+8,baseline.1){return Err(format!("file handle delta {baseline:?}->{held_files:?}"));}
        let (ready_tx,ready_rx)=mpsc::channel();
        let mut workers=Workers{releases:Vec::new(),joins:Vec::new()};
        for n in 0..4 {
            let (release_tx,release_rx)=mpsc::channel();let ready=ready_tx.clone();
            let join=std::thread::Builder::new().name(format!("resource-control-{n}")).spawn(move||{
                let _=ready.send(());let _=release_rx.recv();
            }).map_err(|e|e.to_string())?;
            workers.releases.push(release_tx);workers.joins.push(join);
        }
        drop(ready_tx);
        for _ in 0..4 {ready_rx.recv_timeout(Duration::from_secs(10)).map_err(|e|e.to_string())?;}
        let held_threads=sample()?;
        if held_threads.1!=baseline.1+4 {return Err(format!("thread delta {baseline:?}->{held_threads:?}"));}
        workers.join()?;
        let joined=sample()?;
        if joined!=held_files{return Err(format!("joined resources {held_files:?}->{joined:?}"));}
        drop(files);
        let final_counts=sample()?;
        if final_counts!=baseline{return Err(format!("released resources {baseline:?}->{final_counts:?}"));}
        println!("INFO WV resources pid={} baseline={baseline:?} held_files={held_files:?} held_threads={held_threads:?} joined={joined:?} final={final_counts:?} repeated_samples=32 invalid_errors={errors:?} fixture={}",std::process::id(),fixture.display());
        Ok("positive native counters;32 closed snapshots; null queries error6;8 held files and4 ready threads detected; owned handles/threads return to baseline".into())
    }

    fn deadline()->Instant{Instant::now()+Duration::from_secs(10)}
    fn parent(label:&str)->Result<PathBuf,String>{
        let base=PathBuf::from(std::env::var_os("YOKAI_TEST_DATA_DIR").ok_or("missing test data root")?);
        let path=base.join(format!("windows_validation-{}-{label}-{}",std::process::id(),NEXT.fetch_add(1,Ordering::Relaxed)));
        fs::create_dir(&path).map_err(|e|e.to_string())?;Ok(path)
    }
    fn instance(label:&str)->Result<store::Instance,String>{store::create_instance(&parent(label)?).map_err(|e|e.to_string())}
    fn request(id:&str,payload:&[u8])->Result<CommandRequest,String>{
        CommandRequest::new(CommandId::new(id).map_err(|e|e.to_string())?,"test",1,payload).map_err(|e|e.to_string())
    }
    fn accept(h:&store::StoreHandle,id:&str,payload:&[u8])->Result<AcceptOutcome,String>{
        h.accept_command(request(id,payload)?,deadline()).map_err(|e|e.to_string())?.wait(deadline()).map_err(|e|e.to_string())
    }
    fn ro(root:&Path)->Result<Connection,String>{
        let c=Connection::open_with_flags(root.join("instance.db"),OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e|e.to_string())?;
        c.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,true).map_err(|e|e.to_string())?;Ok(c)
    }
    fn protected(root:&Path)->Result<Vec<Option<Vec<u8>>>,String>{
        ["instance.db","instance.db-wal","instance.id"].iter().map(|name|match fs::read(root.join(name)){
            Ok(bytes)=>Ok(Some(bytes)),Err(e) if e.kind()==std::io::ErrorKind::NotFound=>Ok(None),Err(e)=>Err(e.to_string())}).collect()
    }
    struct OwnedChild{child:Child}
    impl Drop for OwnedChild {fn drop(&mut self){if !matches!(self.child.try_wait(),Ok(Some(_))){let _=self.child.kill();let _=self.child.wait();}}}
    fn launch(mut command:Command,label:&str)->Result<(OwnedChild,PathBuf),String>{
        let logs=parent(label)?;
        command.stdin(Stdio::null()).stdout(Stdio::from(File::create_new(logs.join("child.out")).map_err(|e|e.to_string())?))
            .stderr(Stdio::from(File::create_new(logs.join("child.err")).map_err(|e|e.to_string())?));
        let child=command.spawn().map_err(|e|e.to_string())?;
        println!("INFO WV child_pid={} logs={}",child.id(),logs.display());Ok((OwnedChild{child},logs))
    }
    fn wait(child:&mut OwnedChild,timeout:Duration)->Result<ExitStatus,String>{
        let until=Instant::now()+timeout;
        loop {if let Some(status)=child.child.try_wait().map_err(|e|e.to_string())?{return Ok(status);}
            if Instant::now()>=until{return Err(format!("owned child {} timed out",child.child.id()));}std::thread::sleep(Duration::from_millis(10));}
    }
    fn command(mode:&str,root:&Path)->Result<Command,String>{
        let mut c=Command::new(std::env::current_exe().map_err(|e|e.to_string())?);c.arg(mode).arg(root);Ok(c)
    }
    fn child_mode(args:&[String])->ExitCode {
        let result=(||->Result<(),String>{
            let mode=args.get(1).ok_or("child mode missing")?;let root=Path::new(args.get(2).ok_or("child root missing")?);
            match mode.as_str(){
                "__busy"=>{if !matches!(store::open_instance(root),Err(InstanceError::Busy(_))){return Err("competing opener was not Busy".into());}},
                "__truth"=>{
                    let c=ro(root)?;
                    for table in ["runs","commands","jobs","events WHERE kind='run.accepted'"]{
                        let count:i64=c.query_row(&format!("SELECT count(*) FROM {table}"),[],|r|r.get(0)).map_err(|e|e.to_string())?;
                        if count!=1{return Err(format!("truth {table}={count}"));}
                    }
                },
                "__stageb_refusal"=>{
                    let expected=args.get(3).ok_or("missing Stage-B expected cause")?;
                    let before=access_snapshot(root)?;let (cause,effects)=stageb_refusal(root)?;
                    if &cause!=expected || access_snapshot(root)?!=before || effects!="no inventory changes"{return Err(format!("child Stage-B cause={cause:?} effects={effects:?}"));}
                    println!("INFO WV stageb_child cause={cause:?} effects={effects:?} exact_refusal=true lock_reacquired=true complete_bytes_unchanged=true");
                },
                "__semantic_refusal"=>{
                    let case=args.get(3).ok_or("missing semantic case")?;let expected=args.get(4).ok_or("missing semantic detail")?;
                    let before=access_snapshot(root)?;let outcome=semantic_refusal(root,case,expected)?;let after=access_snapshot(root)?;
                    semantic_preserved(&before,&after,true)?;
                    println!("INFO WV semantic_child case={case} outcome={outcome} exact_refusal=true lock_reacquired=true protected_bytes_unchanged=true inventory_before={:?} inventory_after={:?}",stageb_inventory(&before)?,stageb_inventory(&after)?);
                },
                "__corruption_refusal"=>{
                    let case=args.get(3).ok_or("missing corruption case")?;
                    let page=args.get(4).ok_or("missing corruption page")?.parse::<u32>().map_err(|e|e.to_string())?;
                    let before=access_snapshot(root)?;let outcome=corruption_refusal(root,case,page)?;let after=access_snapshot(root)?;
                    semantic_preserved(&before,&after,case=="leaf")?;
                    println!("INFO WV corruption_child case={case} outcome={outcome} exact_refusal=true protected_bytes_unchanged=true inventory_before={:?} inventory_after={:?}",stageb_inventory(&before)?,stageb_inventory(&after)?);
                },
                "__raw_header_refusal"=>{
                    let case=args.get(3).ok_or("missing raw-header case")?;
                    let before=access_snapshot(root)?;let outcome=raw_header_refusal(root,case)?;let after=access_snapshot(root)?;
                    if before!=after{return Err("raw-header child changed complete closed bytes/names/types".into());}
                    println!("INFO WV raw_header_child case={case} outcome={outcome} sqlite_events=[] exact_refusal=true complete_closed_bytes_unchanged=true inventory_before={:?} inventory_after={:?}",stageb_inventory(&before)?,stageb_inventory(&after)?);
                },
                "__close_error"=>{
                    store::test_api::fail_startup_close_once();
                    let error=match store::open_instance(root){Ok(i)=>{i.close().map_err(|e|e.to_string())?;return Err("startup unexpectedly succeeded".into());},Err(e)=>e.to_string()};
                    if !error.contains("SQLite close unconfirmed") || !error.contains("ownership retained"){return Err(format!("wrong startup failure: {error}"));}
                    if store::test_api::connection_events().iter().any(|s|*s=="lock-released"){return Err("ownership released after close failure".into());}
                    fs::write(args.get(3).ok_or("missing close marker")?,&error).map_err(|e|e.to_string())?;
                    println!("INFO WV real SQLite close failure: {error}");
                    loop {std::thread::park_timeout(Duration::from_secs(1));}
                },
                _=>return Err("unknown child mode".into()),
            }
            Ok(())
        })();
        match result{Ok(())=>ExitCode::SUCCESS,Err(e)=>{eprintln!("WV_CHILD_FAIL {e}");ExitCode::from(3)}}
    }
    fn replay_rollback()->Result<String,String>{
        for payload in [b"original".as_slice(),b"conflict".as_slice()]{
            let i=instance("replay-rollback")?;let root=i.root().to_owned();let original=accept(&i.store(),"stable",b"original")?;
            let run_id=match original{AcceptOutcome::Accepted{run_id}=>run_id,_=>return Err("initial acceptance failed".into())};
            let result=store::test_api::accept_command_fault(&i,request("stable",payload)?,deadline(),store::test_api::WriteFault::RollbackFailure(10))
                .map_err(|e|e.to_string())?.wait(deadline());
            let failure=result.err().ok_or("typed replay success escaped injected failed rollback")?;
            if failure.certainty!=Certainty::OutcomeUnknown || failure.evidence.phase!=Phase::Begun
                || failure.evidence.rollback_succeeded!=Some(false) || failure.evidence.autocommit_after_rollback!=Some(false)
                || i.store().state()!=StoreState::Failed {return Err(format!("replay failure not conservative: {failure:?}"));}
            i.close().map_err(|e|e.to_string())?;
            let (mut truth,_)=launch(command("__truth",&root)?,"replay-truth")?;
            if !wait(&mut truth,Duration::from_secs(10))?.success(){return Err("fresh-process durable truth failed".into());}
            let reopened=store::open_instance(&root).map_err(|e|e.to_string())?;
            let expected=if payload==b"original"{AcceptOutcome::Duplicate{run_id}}else{AcceptOutcome::Conflict{run_id}};
            if accept(&reopened.store(),"stable",payload)?!=expected{return Err("replay identity changed after reopen".into());}
            reopened.close().map_err(|e|e.to_string())?;
        }
        Ok("Duplicate and Conflict failed replay rollback/cleanup never reply success; Unknown/Failed; fresh-process truth and retry retain the original run (synthetic rollback errors)".into())
    }
    fn startup_close()->Result<String,String>{
        let i=instance("startup-close")?;let root=i.root().to_owned();i.close().map_err(|e|e.to_string())?;
        let marker=parent("close-marker")?.join("ready");let mut c=command("__close_error",&root)?;
        c.arg(&marker).env("YOKAI_TEST_FAILPOINT","startup-after-epoch-commit:error");
        let (mut child,logs)=launch(c,"close-child")?;let until=deadline();
        while !marker.exists(){
            if let Some(status)=child.child.try_wait().map_err(|e|e.to_string())?{return Err(format!("close child exited {status}; {}",fs::read_to_string(logs.join("child.err")).unwrap_or_default()));}
            if Instant::now()>=until{return Err("close child did not reach retained-ownership marker".into());}std::thread::sleep(Duration::from_millis(10));
        }
        let before=protected(&root)?;
        if !matches!(store::open_instance(&root),Err(InstanceError::Busy(_))) || protected(&root)?!=before{return Err("unconfirmed close failed to retain exclusive ownership/bytes".into());}
        child.child.kill().map_err(|e|e.to_string())?;let status=wait(&mut child,Duration::from_secs(10))?;
        println!("INFO WV killed_retained_owner={} status={status}",child.child.id());
        let reopened=store::open_instance(&root).map_err(|e|e.to_string())?;
        if !reopened.startup_report().is_some_and(|r|r.epoch==3){return Err("reopen did not advance the committed partial-startup epoch".into());}
        reopened.close().map_err(|e|e.to_string())?;
        Ok("real unfinalized-statement SQLite close error retains connection+lock; competing open Busy/unchanged; owned TerminateProcess/reap releases ownership and epoch3 reopen succeeds".into())
    }
    fn v2_drift()->Result<String,String>{
        let objects=[("trigger","commands_immutable"),("trigger","commands_append_only"),("trigger","commands_no_replace"),("index","events_one_accept_per_run")];
        for (kind,name) in objects {for wal in [false,true] {for alter in [false,true] {
            let i=instance("object-drift")?;let root=i.root().to_owned();i.close().map_err(|e|e.to_string())?;
            let c=Connection::open(root.join("instance.db")).map_err(|e|e.to_string())?;
            if wal {c.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,true).map_err(|e|e.to_string())?;}
            let ddl:String=c.query_row("SELECT sql FROM sqlite_schema WHERE name=?1",[name],|r|r.get(0)).map_err(|e|e.to_string())?;
            c.execute_batch(&format!("DROP {kind} {name};")).map_err(|e|e.to_string())?;
            if alter {
                let changed=if kind=="trigger"{ddl.replace("commands are immutable","changed").replace("commands are append-only","changed")}
                    else{ddl.replace("'run.accepted'","'run.held'")};
                if changed==ddl{return Err(format!("drift fixture did not alter {name}"));}c.execute_batch(&changed).map_err(|e|e.to_string())?;
            }
            c.close().map_err(|(_,e)|e.to_string())?;
            if root.join("instance.db-wal").exists()!=wal{return Err("wrong WAL fixture state".into());}
            let before=protected(&root)?;store::test_api::clear();let result=store::open_instance(&root);
            let refused=if wal {matches!(&result,Err(InstanceError::SchemaInconsistent(_)))}
                else{matches!(&result,Err(InstanceError::StageB{cause,..}) if cause.contains("schema objects"))};
            if !refused{return Err(format!("{name} wal={wal} altered={alter}: {:?}",result.as_ref().err()));}
            if wal && protected(&root)?!=before{return Err("WAL preflight drift refusal changed protected bytes".into());}
            let effects=match &result{Err(InstanceError::StageB{observed_effects,..})=>observed_effects.as_str(),_=>"Stage A protected bytes unchanged"};
            let c=ro(&root)?;let epoch:i64=c.query_row("SELECT epoch FROM runtime_epoch",[],|r|r.get(0)).map_err(|e|e.to_string())?;
            let present:i64=c.query_row("SELECT count(*) FROM sqlite_schema WHERE name=?1",[name],|r|r.get(0)).map_err(|e|e.to_string())?;
            if epoch!=1 || present!=(if alter{1}else{0}) || root.join("backups").exists(){return Err("drift refusal repaired/started/migrated the fixture".into());}
            if alter {let after:String=c.query_row("SELECT sql FROM sqlite_schema WHERE name=?1",[name],|r|r.get(0)).map_err(|e|e.to_string())?;if after==ddl{return Err("altered object was repaired".into());}}
            println!("INFO WV drift={name} wal={wal} altered={alter} effects={effects}");
        }}}
        Ok("16 removed/altered V2 trigger/index fixtures refused across WAL/no-WAL paths; no epoch/repair/snapshot; Stage A bytes preserved and Stage B effects recorded".into())
    }
    fn v2_compat()->Result<String,String>{
        let original=PathBuf::from(std::env::var_os("YOKAI_TEST_V2_FIXTURE").ok_or("missing pinned V2 fixture")?);
        let old=PathBuf::from(std::env::var_os("YOKAI_TEST_V2_BINARY").ok_or("missing pinned V2 binary")?);
        let before=protected(&original)?;let root=parent("v2-copy")?.join(original.file_name().ok_or("fixture ID missing")?);fs::create_dir(&root).map_err(|e|e.to_string())?;
        for name in ["instance.db","instance.id","instance.lock"]{fs::copy(original.join(name),root.join(name)).map_err(|e|e.to_string())?;}
        let i=store::open_instance(&root).map_err(|e|e.to_string())?;
        if !i.startup_report().is_some_and(|r|r.epoch==2){return Err("preserved V2 fixture did not reopen without migration".into());}i.close().map_err(|e|e.to_string())?;
        let mut old_probe=Command::new(old);old_probe.arg("__child").arg("probe").arg(&root);
        let (mut child,_)=launch(old_probe,"v2-old-probe")?;
        if !wait(&mut child,Duration::from_secs(10))?.success(){return Err("pinned previous V2 PE refused compatible copied data".into());}
        let c=ro(&root)?;let (version,history,epoch):(i64,i64,i64)=c.query_row("SELECT (SELECT user_version FROM pragma_user_version),(SELECT count(*) FROM yokai_schema_history),(SELECT epoch FROM runtime_epoch)",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).map_err(|e|e.to_string())?;
        if (version,history,epoch)!=(2,2,3) || root.join("backups").exists() || protected(&original)?!=before{return Err("V2 compatibility changed schema/history/original or repeated migration".into());}
        Ok("copied preserved V2 reopens under candidate and prior pinned V2 PE; schema/history2, epochs2/3, no snapshot; original fixture untouched".into())
    }
    fn sqlite_options()->Result<String,String>{
        let i=instance("sqlite-options")?;let root=i.root().to_owned();i.close().map_err(|e|e.to_string())?;let c=ro(&root)?;
        let runtime=store::test_api::sqlite_runtime(&c).map_err(|e|e.to_string())?;
        let mut options=c.prepare("PRAGMA compile_options").map_err(|e|e.to_string())?.query_map([],|r|r.get::<_,String>(0)).map_err(|e|e.to_string())?.collect::<Result<Vec<_>,_>>().map_err(|e|e.to_string())?;
        options.sort();if options.is_empty() || options.windows(2).any(|p|p[0]==p[1]) || !options.iter().any(|o|o=="THREADSAFE=1"){return Err("missing/ambiguous linked SQLite compile options".into());}
        println!("INFO WV SQLite version={} source_id={} compile_options={options:?}",runtime.version,runtime.source_id);
        Ok(format!("actual linked engine and{} sorted compile options recorded; no Linux runtime or option-parity inference",options.len()))
    }
    fn conventional(path:&Path)->String{let text=path.to_string_lossy();text.strip_prefix("\\\\?\\").unwrap_or(&text).to_owned()}
    fn cmd()->Result<Command,String>{
        let mut c=Command::new(PathBuf::from(std::env::var_os("SystemRoot").ok_or("SystemRoot missing")?).join("System32").join("cmd.exe"));
        c.arg("/u").creation_flags(0x0800_0000);Ok(c)
    }
    fn run_cmd(command:Command,label:&str)->Result<(ExitStatus,String),String>{
        let paths:Vec<_>=command.get_envs().filter(|(name,_)|name.to_string_lossy().starts_with("WV_")).collect();
        println!("INFO WV cmd_program={:?} args={:?} fixture_paths={paths:?}",command.get_program(),command.get_args().collect::<Vec<_>>());
        let (mut child,logs)=launch(command,label)?;let status=wait(&mut child,Duration::from_secs(10))?;
        let bytes=fs::read(logs.join("child.out")).map_err(|e|e.to_string())?;
        if bytes.len()%2!=0{return Err("cmd /u did not produce complete UTF16 output".into());}
        let units:Vec<_>=bytes.chunks_exact(2).map(|b|u16::from_le_bytes([b[0],b[1]])).collect();
        let text=String::from_utf16(&units).map_err(|e|e.to_string())?;Ok((status,text.trim_start_matches('\u{feff}').to_owned()))
    }
    fn alias_refusal(path:&Path,root:&Path)->Result<(),String>{
        let before=protected(root)?;
        match store::open_instance(path){Err(InstanceError::Busy(_)|InstanceError::PathInvalid(_)|InstanceError::UnsupportedLocation(_))=>{},
            Ok(i)=>{i.close().map_err(|e|e.to_string())?;return Err("alias acquired a second owner".into());},Err(e)=>return Err(format!("unexpected alias refusal: {e}"))}
        if protected(root)?!=before{return Err("alias refusal changed protected bytes".into());}Ok(())
    }
    fn remove_owned_link(alias:&Path,target:&Path,parent:&Path)->Result<(),String>{
        use std::os::windows::fs::MetadataExt;
        if alias.parent()!=Some(parent) || !alias.is_absolute() || fs::symlink_metadata(alias).map_err(|e|e.to_string())?.file_attributes()&0x400==0
            || fs::canonicalize(alias).map_err(|e|e.to_string())?!=fs::canonicalize(target).map_err(|e|e.to_string())? {return Err("owned link identity not verified for unlink".into());}
        println!("INFO WV unlink_owned_reparse={} target={}",alias.display(),target.display());
        fs::remove_dir(alias).map_err(|e|e.to_string())
    }
    fn aliases()->Result<String,String>{
        let i=instance("aliases")?;let root=i.root().to_owned();let pause=store::test_api::block_writer(&i).map_err(|e|e.to_string())?;
        let normal=conventional(&root);let mut exercised=vec!["normal","case","extended"];
        for alias in [PathBuf::from(&normal),PathBuf::from(normal.to_ascii_uppercase()),PathBuf::from(format!("\\\\?\\{normal}"))]{alias_refusal(&alias,&root)?;}
        let link_parent=parent("owned-aliases")?;let junction=link_parent.join("junction");
        // cmd.exe parses its command tail rather than the C argv escaping rules.
        // Only this fixed text is raw; the two paths are separate environment values.
        let mut make=cmd()?;make.args(["/d","/q","/c"]).raw_arg("mklink /J \"%WV_ALIAS%\" \"%WV_TARGET%\"").env("WV_ALIAS",&junction).env("WV_TARGET",&normal);
        let (status,text)=run_cmd(make,"junction-create")?;if !status.success(){return Err(format!("junction fixture failed {status}: {text}"));}
        let result=alias_refusal(&junction,&root);remove_owned_link(&junction,&root,&link_parent)?;result?;exercised.push("junction");
        let symbolic=link_parent.join("symbolic");
        match std::os::windows::fs::symlink_dir(&root,&symbolic){
            Ok(())=>{let result=alias_refusal(&symbolic,&root);remove_owned_link(&symbolic,&root,&link_parent)?;result?;exercised.push("symlink");},
            Err(e) if e.raw_os_error()==Some(1314) || e.kind()==std::io::ErrorKind::PermissionDenied=>println!("INFO WV symlink=not_checked privilege unavailable: {e}"),
            Err(e)=>return Err(format!("symlink fixture error: {e}")),
        }
        let mut short=cmd()?;short.args(["/d","/q","/c"]).raw_arg("for %I in (\"%WV_TARGET%\") do @echo %~sI").env("WV_TARGET",&normal);
        let (status,text)=run_cmd(short,"short-name")?;if !status.success(){return Err(format!("short-name query failed {status}"));}
        let short=text.trim();if !short.eq_ignore_ascii_case(&normal) && short.contains('~'){
            if fs::canonicalize(short).map_err(|e|e.to_string())?!=root{return Err("short alias did not bind the same root".into());}alias_refusal(Path::new(short),&root)?;exercised.push("8.3");
        }else{println!("INFO WV 8.3=not_checked no distinct short alias returned");}
        for path in ["\\\\localhost\\not-a-share\\x","\\\\?\\UNC\\localhost\\not-a-share\\x"]{
            if !yokai_platform::location::windows_path_is_network_for_test(path){return Err("UNC syntax not classified as network".into());}
        }
        println!("INFO WV UNC=syntax_only actual_loopback_share=not_checked subst=not_checked no host mappings/shares/privileges changed");
        drop(pause);i.close().map_err(|e|e.to_string())?;
        Ok(format!("no second owner or protected-byte change for {exercised:?}; capability gaps explicit; only verified owned links unlinked, all targets/logs retained"))
    }
    fn sharing_violation(result:std::io::Result<()>,label:&str)->Result<(),String>{
        match result {
            Err(e) if e.raw_os_error()==Some(32)=>{println!("INFO WV open_file_operation={label} refused_os=32");Ok(())},
            other=>Err(format!("{label}: expected sharing violation32, observed {other:?}")),
        }
    }
    fn owned_file_paths(parent:&Path,source:&Path,destination:&Path)->Result<(),String>{
        if !parent.is_absolute() || !source.is_absolute() || !destination.is_absolute()
            || source.parent()!=Some(parent) || destination.parent()!=Some(parent) || source==destination{return Err("file observation paths are not fresh owned siblings".into());}
        match fs::symlink_metadata(destination){
            Err(e) if e.kind()==std::io::ErrorKind::NotFound=>{},
            other=>return Err(format!("owned destination absence unconfirmed: {other:?}")),
        }
        Ok(())
    }
    fn data_opens_refused(lock:&Path)->Result<(),String>{
        for (label,read,write) in [("read",true,false),("write",false,true),("read-write",true,true)] {
            let opened=OpenOptions::new().read(read).write(write).share_mode(7).open(lock);
            sharing_violation(opened.map(drop),label)?;
        }
        Ok(())
    }
    fn open_files()->Result<String,String>{
        let created=instance("open-files")?;let root=created.root().to_owned();
        support::verify_absence_oracle(&root)?;
        data_opens_refused(&root.join("instance.lock"))?;
        created.close().map_err(|e|e.to_string())?;
        let lock=root.join("instance.lock");let destination=root.join("lock-rename-probe");owned_file_paths(&root,&lock,&destination)?;
        // Capture bytes while unowned: share-zero ownership refuses raw data handles.
        let lock_bytes=fs::read(&lock).map_err(|e|format!("unowned lock baseline: {e}"))?;
        let i=store::open_instance(&root).map_err(|e|e.to_string())?;let pause=store::test_api::block_writer(&i).map_err(|e|e.to_string())?;
        let before=protected(&root)?;
        data_opens_refused(&lock)?;
        let locked_read=fs::read(&lock);
        if !matches!(&locked_read,Err(e) if e.raw_os_error()==Some(32)){return Err(format!("raw lock read did not encounter sharing violation32: {locked_read:?}"));}
        match store::open_instance(&root) {
            Err(InstanceError::Busy(_))=>{},
            Ok(second)=>{second.close().map_err(|e|e.to_string())?;return Err("second owner opened while share0 held".into());},
            Err(e)=>return Err(format!("second opener not Busy: {e}")),
        }
        sharing_violation(fs::rename(&lock,&destination),"product-lock-rename")?;
        sharing_violation(fs::remove_file(&lock),"product-lock-delete")?;
        if protected(&root)?!=before
            || support::entry_exists(&destination)?{return Err("product lock refusal changed protected files or lock identity path".into());}
        println!("INFO WV product_lock create_and_reopen_data_handles=refused32 second_instance=Busy raw_read=sharing_violation32 rename=sharing_violation delete=sharing_violation source_requested_share_mask=0 metadata_validation=retained");
        drop(pause);i.close().map_err(|e|e.to_string())?;
        if fs::read(&lock).map_err(|e|format!("released lock comparison: {e}"))?!=lock_bytes{return Err("released persistent lock bytes differ from unowned baseline".into());}

        let bytes=b"owned Windows file probe";
        let zero=parent("share-zero")?;let source=zero.join("probe.bin");let destination=zero.join("renamed.bin");owned_file_paths(&zero,&source,&destination)?;
        let mut handle=OpenOptions::new().create_new(true).read(true).write(true).share_mode(0).open(&source).map_err(|e|e.to_string())?;
        handle.write_all(bytes).map_err(|e|e.to_string())?;handle.flush().map_err(|e|e.to_string())?;
        let second=OpenOptions::new().read(true).open(&source);
        if !matches!(&second,Err(e) if e.raw_os_error()==Some(32)){return Err(format!("share0 fixture admitted compatible handle or wrong refusal: {second:?}"));}
        sharing_violation(fs::rename(&source,&destination),"synthetic-share0-rename")?;
        sharing_violation(fs::remove_file(&source),"synthetic-share0-delete")?;drop(handle);
        if fs::read(&source).map_err(|e|e.to_string())?!=bytes || support::entry_exists(&destination)?{return Err("share0 fixture bytes/path changed".into());}
        println!("INFO WV synthetic_share0 second_open=sharing_violation rename=sharing_violation delete=sharing_violation original_retained={}",source.display());

        let shared=parent("share-delete")?;let source=shared.join("probe.bin");let destination=shared.join("renamed.bin");owned_file_paths(&shared,&source,&destination)?;
        let retained=shared.join("original-content.bin");let mut copy=File::create_new(&retained).map_err(|e|e.to_string())?;
        copy.write_all(bytes).map_err(|e|e.to_string())?;copy.sync_all().map_err(|e|e.to_string())?;drop(copy);
        let mut handle=OpenOptions::new().create_new(true).read(true).write(true).share_mode(7).open(&source).map_err(|e|e.to_string())?;
        handle.write_all(bytes).map_err(|e|e.to_string())?;handle.flush().map_err(|e|e.to_string())?;
        fs::rename(&source,&destination).map_err(|e|format!("shared-delete fixture rename: {e}"))?;
        if support::entry_exists(&source)? || fs::read(&destination).map_err(|e|e.to_string())?!=bytes{return Err("shared-delete rename lost bytes or left old name".into());}
        fs::remove_file(&destination).map_err(|e|format!("shared-delete fixture unlink: {e}"))?;
        let pending=match fs::symlink_metadata(&destination){Ok(m)=>format!("present len={}",m.len()),Err(e)=>format!("error_os={:?} kind={:?}",e.raw_os_error(),e.kind())};
        handle.seek(SeekFrom::Start(0)).map_err(|e|e.to_string())?;let mut observed=Vec::new();handle.read_to_end(&mut observed).map_err(|e|e.to_string())?;drop(handle);
        let closed=fs::symlink_metadata(&destination).err();
        if observed!=bytes || !closed.is_some_and(|e|e.kind()==std::io::ErrorKind::NotFound)
            || fs::read(&retained).map_err(|e|e.to_string())?!=bytes{return Err("shared-delete handle/content or final unlink state mismatch".into());}
        println!("INFO WV synthetic_share7 rename=allowed unlink=allowed metadata_while_open={pending} original_handle_bytes={observed:?} destination_absent_after_owned_close=true original_retained={}",retained.display());
        Ok("product share0 refuses raw data handles on create/reopen and second Instance is Busy; held lock rename/delete refuse32 unchanged; separate share0/share7 owned probes record rename/delete/handle semantics with original bytes retained; no database/root deletion or discard design".into())
    }
    #[derive(Debug,PartialEq,Eq)]
    struct SqliteEntry {
        length:u64,
        attributes:u32,
        created:u64,
        bytes:Option<Vec<u8>>,
        raw_read_error:Option<i32>,
    }
    fn sqlite_entry(path:&Path)->Result<SqliteEntry,String>{
        use std::os::windows::fs::MetadataExt;
        let metadata=fs::symlink_metadata(path).map_err(|e|format!("SQLite entry {}: {e}",path.display()))?;
        if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.file_attributes()&0x400!=0 || metadata.len()==0 {
            return Err(format!("SQLite entry is not a nonempty regular file: {}",path.display()));
        }
        yokai_platform::location::validate_regular_single_link(path,&metadata).map_err(|e|e.to_string())?;
        let (bytes,raw_read_error)=match fs::read(path){
            Ok(bytes)=>(Some(bytes),None),
            Err(e) if path.file_name()==Some(std::ffi::OsStr::new("probe.db-shm")) && e.raw_os_error()==Some(33)=>(None,Some(33)),
            Err(e)=>return Err(format!("SQLite entry read {}: {e}",path.display())),
        };
        Ok(SqliteEntry{length:metadata.len(),attributes:metadata.file_attributes(),created:metadata.creation_time(),bytes,raw_read_error})
    }
    fn sqlite_file_refusals(dir:&Path,mode:&str,phase:&str,names:&[&str])->Result<(),String>{
        let retained=dir.join(format!("retained-{phase}"));fs::create_dir(&retained).map_err(|e|e.to_string())?;
        let mut before=Vec::new();
        for name in names {
            let source=dir.join(name);let destination=dir.join(format!("{name}-{phase}-rename"));
            owned_file_paths(dir,&source,&destination)?;
            let snapshot=sqlite_entry(&source)?;
            if let Some(bytes)=&snapshot.bytes {
                let mut copy=File::create_new(retained.join(name)).map_err(|e|e.to_string())?;
                copy.write_all(bytes).map_err(|e|e.to_string())?;copy.sync_all().map_err(|e|e.to_string())?;
            }
            println!("INFO WV sqlite_file mode={mode} phase={phase} file={name} length={} attributes={} created={} bytes_verified={} raw_read_error={:?} baseline_directory={}",
                snapshot.length,snapshot.attributes,snapshot.created,snapshot.bytes.is_some(),snapshot.raw_read_error,retained.display());
            before.push((source,destination,snapshot));
        }
        // The fixture connections perform no SQL between these snapshots.
        for (source,destination,_) in &before {
            sharing_violation(fs::rename(source,destination),&format!("sqlite-{mode}-{phase}-{}-rename",source.file_name().unwrap().to_string_lossy()))?;
            sharing_violation(fs::remove_file(source),&format!("sqlite-{mode}-{phase}-{}-delete",source.file_name().unwrap().to_string_lossy()))?;
        }
        for (source,destination,snapshot) in before {
            if sqlite_entry(&source)?!=snapshot || support::entry_exists(&destination)? {
                return Err(format!("SQLite refusal changed observed source/destination: {}",source.display()));
            }
        }
        println!("INFO WV sqlite_phase mode={mode} phase={phase} files={} operations={} all_refused_os=32 readable_bytes_unchanged=true metadata_unchanged=true stable_object_id=not_checked",names.len(),names.len()*2);
        Ok(())
    }
    fn sqlite_rows(c:&Connection,pending:bool)->Result<(),String>{
        let mut query=c.prepare("SELECT id,payload FROM w5_probe ORDER BY id").map_err(|e|e.to_string())?;
        let rows=query.query_map([],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,Vec<u8>>(1)?))).map_err(|e|e.to_string())?
            .collect::<rusqlite::Result<Vec<_>>>().map_err(|e|e.to_string())?;
        let mut expected=vec![(1,b"committed-owned".to_vec())];
        if pending {expected.push((2,b"pending-owned".to_vec()));}
        if rows!=expected{return Err(format!("SQLite logical rows differ: {rows:?}"));}
        Ok(())
    }
    fn sqlite_files()->Result<String,String>{
        for mode in ["WAL","DELETE"] {
            let dir=parent(&format!("sqlite-files-{}",mode.to_ascii_lowercase()))?;let path=dir.join("probe.db");
            let writer=store::test_api::create_file(&path).map_err(|e|e.to_string())?;
            let actual:String=writer.query_row(&format!("PRAGMA journal_mode={mode}"),[],|r|r.get(0)).map_err(|e|e.to_string())?;
            if !actual.eq_ignore_ascii_case(mode){return Err(format!("SQLite fixture mode={actual}, expected {mode}"));}
            writer.execute_batch("CREATE TABLE w5_probe(id INTEGER PRIMARY KEY,payload BLOB NOT NULL)").map_err(|e|e.to_string())?;
            writer.execute("INSERT INTO w5_probe VALUES(1,?1)",[b"committed-owned".as_slice()]).map_err(|e|e.to_string())?;
            writer.execute_batch("BEGIN IMMEDIATE").map_err(|e|e.to_string())?;
            writer.execute("INSERT INTO w5_probe VALUES(2,?1)",[b"pending-owned".as_slice()]).map_err(|e|e.to_string())?;
            if writer.is_autocommit(){return Err("SQLite pending writer transaction not active".into());}
            sqlite_rows(&writer,true)?;
            let writer_files:&[&str]=if mode=="WAL" {&["probe.db","probe.db-wal","probe.db-shm"]} else {&["probe.db","probe.db-journal"]};
            sqlite_file_refusals(&dir,mode,"writer-transaction",writer_files)?;
            writer.execute_batch("ROLLBACK").map_err(|e|e.to_string())?;
            if !writer.is_autocommit(){return Err("SQLite rollback did not end writer transaction".into());}
            sqlite_rows(&writer,false)?;
            let reader=Connection::open_with_flags(&path,OpenFlags::SQLITE_OPEN_READ_ONLY|OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(|e|e.to_string())?;
            reader.execute_batch("BEGIN").map_err(|e|e.to_string())?;sqlite_rows(&reader,false)?;
            writer.close().map_err(|(_,e)|e.to_string())?;
            if reader.is_autocommit(){return Err("SQLite read snapshot not active after writer close".into());}
            let reader_files:&[&str]=if mode=="WAL" {&["probe.db","probe.db-wal","probe.db-shm"]} else {&["probe.db"]};
            sqlite_file_refusals(&dir,mode,"reader-after-writer-close",reader_files)?;
            sqlite_rows(&reader,false)?;
            reader.execute_batch("ROLLBACK").map_err(|e|e.to_string())?;
            reader.close().map_err(|(_,e)|e.to_string())?;
            for name in ["probe.db","probe.db-wal","probe.db-shm","probe.db-journal"] {
                let file=dir.join(name);let present=support::entry_exists(&file)?;
                if present {drop(OpenOptions::new().read(true).write(true).share_mode(0).open(&file).map_err(|e|format!("SQLite released handle {name}: {e}"))?);}
                let release=if present {"allowed"} else {"not_applicable_absent"};
                println!("INFO WV sqlite_after_close mode={mode} file={name} present={present} exclusive_data_open={release} cleanup=SQLite_intrinsic_not_test_delete");
            }
            let reopened=Connection::open_with_flags(&path,OpenFlags::SQLITE_OPEN_READ_ONLY|OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(|e|e.to_string())?;
            sqlite_rows(&reopened,false)?;
            let integrity:String=reopened.query_row("PRAGMA integrity_check",[],|r|r.get(0)).map_err(|e|e.to_string())?;
            if integrity!="ok"{return Err(format!("SQLite fixture integrity: {integrity}"));}
            reopened.close().map_err(|(_,e)|e.to_string())?;
            println!("INFO WV sqlite_logical mode={mode} pending_before_rollback=true pending_after_rollback=false reader_snapshot=committed_literal_row reopened=committed_literal_row integrity=ok synthetic_non_Instance=true");
        }
        Ok("WAL writer+reader main/WAL/SHM and DELETE writer main/journal+reader main:18 rename/delete refusals32; readable bytes and metadata preserved, SHM read-lock limits explicit; real rollback, reader snapshot, released handles and fresh logical reopen verified; owned synthetic files only, no Instance/root discard".into())
    }
    fn access_snapshot(root:&Path)->Result<BTreeMap<String,Option<Vec<u8>>>,String>{
        use std::os::windows::fs::MetadataExt;
        let mut snapshot=BTreeMap::new();
        for entry in fs::read_dir(root).map_err(|e|e.to_string())? {
            let entry=entry.map_err(|e|e.to_string())?;let path=entry.path();
            let name=entry.file_name().into_string().map_err(|_|"non-UTF8 owned fixture entry")?;
            let meta=fs::symlink_metadata(&path).map_err(|e|e.to_string())?;
            if meta.file_type().is_symlink() || meta.file_attributes()&0x400!=0{return Err("access fixture contains a link/reparse entry".into());}
            let bytes=if meta.is_file(){
                yokai_platform::location::validate_regular_single_link(&path,&meta).map_err(|e|e.to_string())?;
                Some(fs::read(&path).map_err(|e|e.to_string())?)
            }else if meta.is_dir(){
                if fs::read_dir(&path).map_err(|e|e.to_string())?.next().transpose().map_err(|e|e.to_string())?.is_some(){return Err("access fixture contains a nonempty subdirectory".into());}
                None
            }else{return Err("unexpected access fixture entry type".into());};
            snapshot.insert(name,bytes);
        }
        Ok(snapshot)
    }
    fn future_wal_control(root:&Path,expected:u32)->Result<(),String>{
        store::test_api::clear();
        let result=store::open_instance(root);let events=store::test_api::connection_events();
        match result {
            Err(InstanceError::SchemaTooNew{found,maximum:2}) if found==expected && events==["stage-a-read-only"]=>Ok(()),
            Ok(i)=>{i.close().map_err(|e|e.to_string())?;Err(format!("future WAL unexpectedly admitted an Instance; events={events:?}"))},
            Err(e)=>Err(format!("future WAL control expected={expected}: {e:?}; events={events:?}")),
        }
    }
    fn preflight_access()->Result<String,String>{
        for blocked in ["instance.db","instance.db-wal","instance.db-shm"] {
            let i=instance("preflight-access")?;let root=i.root().to_owned();i.close().map_err(|e|e.to_string())?;
            let c=Connection::open_with_flags(root.join("instance.db"),OpenFlags::SQLITE_OPEN_READ_WRITE|OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(|e|e.to_string())?;
            c.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,true).map_err(|e|e.to_string())?;
            c.execute_batch("PRAGMA wal_autocheckpoint=0; PRAGMA user_version=3;").map_err(|e|e.to_string())?;
            let wal_version:i64=c.query_row("PRAGMA user_version",[],|r|r.get(0)).map_err(|e|e.to_string())?;
            if wal_version!=3{return Err("future version did not commit to WAL".into());}
            c.close().map_err(|(_,e)|e.to_string())?;
            future_wal_control(&root,3)?;
            let before=access_snapshot(&root)?;
            let file=|name:&str|before.get(name).and_then(Option::as_ref).ok_or_else(||format!("missing access fixture file {name}"));
            if file("instance.db")?.get(60..64)!=Some(2u32.to_be_bytes().as_slice())
                || file("instance.db-wal")?.len()<=32 || file("instance.db-shm")?.len()<=128
                || before.contains_key("instance.db-journal"){return Err("future WAL fixture lacks main2/nonempty WAL+SHM or has journal".into());}
            let retained=root.parent().ok_or("access fixture parent missing")?.join("retained-before-denial");
            fs::create_dir(&retained).map_err(|e|e.to_string())?;
            for (name,bytes) in &before {
                if let Some(bytes)=bytes {
                    let mut copy=File::create_new(retained.join(name)).map_err(|e|e.to_string())?;
                    copy.write_all(bytes).map_err(|e|e.to_string())?;copy.sync_all().map_err(|e|e.to_string())?;
                }
            }
            println!("INFO WV preflight_access fixture={} blocked={blocked} main_header=2 committed_wal_version=3 fixture_connection=closed retained={} entries={:?}",root.display(),retained.display(),before.keys().collect::<Vec<_>>());
            let path=root.join(blocked);
            let guard=OpenOptions::new().read(true).write(true).share_mode(0).open(&path).map_err(|e|e.to_string())?;
            sharing_violation(fs::read(&path).map(|_|()),&format!("preflight-{blocked}-raw-read"))?;
            store::test_api::clear();
            let result=store::open_instance(&root);let events=store::test_api::connection_events();
            let error=match result {
                Err(InstanceError::PreflightUnavailable(reason))=>reason,
                Ok(i)=>{i.close().map_err(|e|e.to_string())?;return Err("share-denied preflight admitted an Instance".into());},
                Err(e)=>return Err(format!("share-denied preflight wrong refusal: {e:?}")),
            };
            if !events.is_empty(){return Err(format!("share-denied preflight reached SQLite: {events:?}"));}
            drop(guard);
            if access_snapshot(&root)?!=before{return Err(format!("{blocked} access refusal changed root entries/bytes"));}
            for (name,bytes) in &before {
                if let Some(bytes)=bytes {if fs::read(retained.join(name)).map_err(|e|e.to_string())?!=*bytes{return Err("retained access baseline differs".into());}}
            }
            drop(OpenOptions::new().read(true).write(true).share_mode(0).open(&path).map_err(|e|format!("released access guard: {e}"))?);
            future_wal_control(&root,3)?;
            let after=access_snapshot(&root)?;
            if before.keys().ne(after.keys()) || before.iter().any(|(name,bytes)|name!="instance.db-shm" && after.get(name)!=Some(bytes)) {
                return Err("post-release read-only control changed names or non-SHM bytes".into());
            }
            println!("INFO WV preflight_access blocked={blocked} raw_read_error=32 refusal=PreflightUnavailable sqlite_events=[] all_entries_bytes_unchanged_after_refusal=true retained_copies_verified=true released_exclusive_open=allowed control_before=SchemaTooNew3 control_after=SchemaTooNew3 control_sqlite_events=[stage-a-read-only] non_shm_bytes_unchanged=true shm_changed_by_readonly_control={} reason={error:?}",before.get("instance.db-shm")!=after.get("instance.db-shm"));
        }
        Ok("database/WAL/SHM independently share-denied32: PreflightUnavailable without SQLite fallback; complete root bytes/entries and retained copies unchanged after refusal; released handles restore Stage-A read-only future-WAL refusal; ACL/directory-writability and full PF-13/W2 remain untested".into())
    }
    fn retain_damage_snapshot(root:&Path,label:&str,snapshot:&BTreeMap<String,Option<Vec<u8>>>)->Result<PathBuf,String>{
        let retained=root.parent().ok_or("damage fixture parent missing")?.join(label);
        fs::create_dir(&retained).map_err(|e|e.to_string())?;
        for (name,bytes) in snapshot {
            if let Some(bytes)=bytes {
                let mut copy=File::create_new(retained.join(name)).map_err(|e|e.to_string())?;
                copy.write_all(bytes).map_err(|e|e.to_string())?;copy.sync_all().map_err(|e|e.to_string())?;
            }
        }
        Ok(retained)
    }
    fn preflight_damage()->Result<String,String>{
        for (case,expected) in [("truncate",3),("checksum",3),("shm",4)] {
            let i=instance("preflight-damage")?;let root=i.root().to_owned();i.close().map_err(|e|e.to_string())?;
            let db=root.join("instance.db");let wal_path=root.join("instance.db-wal");let shm_path=root.join("instance.db-shm");
            let c=Connection::open_with_flags(&db,OpenFlags::SQLITE_OPEN_READ_WRITE|OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(|e|e.to_string())?;
            c.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,true).map_err(|e|e.to_string())?;
            c.execute_batch("PRAGMA wal_autocheckpoint=0; PRAGMA user_version=4;").map_err(|e|e.to_string())?;
            c.close().map_err(|(_,e)|e.to_string())?;
            let mut main=fs::read(&db).map_err(|e|e.to_string())?;
            if main.get(60..64)!=Some(2u32.to_be_bytes().as_slice()){return Err("damage fixture did not start with main2".into());}
            main[60..64].copy_from_slice(&3u32.to_be_bytes());fs::write(&db,&main).map_err(|e|e.to_string())?;
            future_wal_control(&root,4)?;
            let pristine=access_snapshot(&root)?;
            let file=|name:&str|pristine.get(name).and_then(Option::as_ref).ok_or_else(||format!("missing pristine damage file {name}"));
            let wal=file("instance.db-wal")?;let shm=file("instance.db-shm")?;
            if wal.len()<120 || shm.len()<128 || shm.len()>1_048_576 || pristine.contains_key("instance.db-journal"){return Err("invalid damage fixture files".into());}
            let page=u32::from_be_bytes(wal[8..12].try_into().map_err(|_|"WAL page field")?) as usize;
            let db_size=u32::from_be_bytes(wal[36..40].try_into().map_err(|_|"WAL commit field")?);
            if !(512..=65_536).contains(&page) || !page.is_power_of_two() || wal.len()!=32+24+page
                || wal[32..36]!=1u32.to_be_bytes() || db_size==0 || wal[116..120]!=4u32.to_be_bytes()
                || file("instance.db")?.get(60..64)!=Some(3u32.to_be_bytes().as_slice()) {
                return Err("damage fixture is not one committed page1 WAL4 over main3".into());
            }
            let original=retain_damage_snapshot(&root,"retained-pristine",&pristine)?;
            let mut changed=wal.clone();
            if case=="shm" {
                fs::write(&shm_path,vec![0xa5;shm.len()]).map_err(|e|e.to_string())?;
            }else {
                // All SQLite connections are closed. Remove only owned SHM so the probe must recover the index.
                drop(OpenOptions::new().read(true).write(true).share_mode(0).open(&shm_path).map_err(|e|e.to_string())?);
                fs::remove_file(&shm_path).map_err(|e|e.to_string())?;
                if support::entry_exists(&shm_path)?{return Err("closed fixture SHM still exists".into());}
                if case=="truncate" {changed.truncate(32+24+page/2);}else{changed[48]^=1;}
                if changed[..32]!=wal[..32] || (case=="checksum" && changed.iter().zip(wal).filter(|(a,b)|a!=b).count()!=1){return Err("damage changed the WAL header or wrong checksum extent".into());}
                fs::write(&wal_path,&changed).map_err(|e|e.to_string())?;
            }
            let before=access_snapshot(&root)?;
            let damaged=retain_damage_snapshot(&root,"retained-damaged",&before)?;
            println!("INFO WV preflight_damage fixture={} case={case} main_header=3 pristine_wal_version=4 page_size={page} committed_page=1 commit_db_pages={db_size} pristine_wal_bytes={} damaged_wal_bytes={} shm_before={} pristine={} damaged={}",
                root.display(),wal.len(),changed.len(),before.contains_key("instance.db-shm"),original.display(),damaged.display());
            future_wal_control(&root,expected)?;
            let after=access_snapshot(&root)?;
            for name in before.keys().chain(after.keys()) {
                if name!="instance.db-shm" && before.get(name)!=after.get(name){return Err(format!("{case} read-only refusal changed non-SHM entry {name}"));}
            }
            for (snapshot,retained) in [(&pristine,&original),(&before,&damaged)] {
                for (name,bytes) in snapshot {
                    if let Some(bytes)=bytes {if fs::read(retained.join(name)).map_err(|e|e.to_string())?!=*bytes{return Err("damage baseline copy changed".into());}}
                }
            }
            for (name,bytes) in &after {
                if bytes.is_some(){drop(OpenOptions::new().read(true).write(true).share_mode(0).open(root.join(name)).map_err(|e|format!("damage probe left an open handle {name}: {e}"))?);}
            }
            println!("INFO WV preflight_damage case={case} refusal=SchemaTooNew{expected} maximum=2 sqlite_events=[stage-a-read-only] non_shm_entries_bytes_unchanged=true retained_copies_verified=true released_exclusive_opens=allowed shm_after={} shm_changed={} cleanup=SQLite_SHM_only_no_main_WAL_repair",
                after.contains_key("instance.db-shm"),before.get("instance.db-shm")!=after.get("instance.db-shm"));
        }
        Ok("incomplete frame and bad stored frame checksum recover main3; damaged SHM recovers valid WAL4; exact future-version Stage-A-only refusals, main/WAL/non-SHM bytes preserved, pristine+damaged copies retained and handles released; synthetic future versions only, no supported-version startup or full PF11/W2 acceptance".into())
    }
    fn closed_sidefile_fixture(label:&str,version:u32)->Result<PathBuf,String>{
        let i=instance(label)?;let root=i.root().to_owned();i.close().map_err(|e|e.to_string())?;
        let db=root.join("instance.db");
        let c=Connection::open_with_flags(&db,OpenFlags::SQLITE_OPEN_READ_WRITE|OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(|e|e.to_string())?;
        c.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,true).map_err(|e|e.to_string())?;
        c.execute_batch(&format!("PRAGMA wal_autocheckpoint=0; PRAGMA user_version={version};")).map_err(|e|e.to_string())?;
        c.close().map_err(|(_,e)|e.to_string())?;
        let mut main=fs::read(&db).map_err(|e|e.to_string())?;
        if main.get(60..64)!=Some(2u32.to_be_bytes().as_slice()){return Err("side-file fixture did not start with main2".into());}
        main[60..64].copy_from_slice(&3u32.to_be_bytes());fs::write(&db,&main).map_err(|e|e.to_string())?;
        future_wal_control(&root,version)?;
        let snapshot=access_snapshot(&root)?;
        let wal=snapshot.get("instance.db-wal").and_then(Option::as_ref).ok_or("missing side-file WAL")?;
        let shm=snapshot.get("instance.db-shm").and_then(Option::as_ref).ok_or("missing side-file SHM")?;
        if wal.len()<120 || shm.len()<128 || snapshot.contains_key("instance.db-journal"){return Err("incomplete side-file fixture".into());}
        let page=u32::from_be_bytes(wal[8..12].try_into().map_err(|_|"WAL page field")?) as usize;
        if !(512..=65_536).contains(&page) || !page.is_power_of_two() || wal.len()!=32+24+page
            || wal[32..36]!=1u32.to_be_bytes() || wal[36..40]==0u32.to_be_bytes() || wal[116..120]!=version.to_be_bytes() {
            return Err("side-file fixture is not one committed page1 frame".into());
        }
        for name in ["instance.db","instance.db-wal","instance.db-shm"] {
            drop(OpenOptions::new().read(true).write(true).share_mode(0).open(root.join(name)).map_err(|e|format!("side-file fixture handle not closed: {e}"))?);
        }
        Ok(root)
    }
    fn preflight_consistency()->Result<String,String>{
        for case in ["foreign-wal","foreign-shm","zero-main"] {
            let root=closed_sidefile_fixture("sidefile-target",4)?;
            let pristine=access_snapshot(&root)?;
            let original=retain_damage_snapshot(&root,"retained-target-pristine",&pristine)?;
            let donor=if case=="zero-main" {None}else{
                let path=closed_sidefile_fixture("sidefile-donor",5)?;let snapshot=access_snapshot(&path)?;
                let retained=retain_damage_snapshot(&path,"retained-donor-pristine",&snapshot)?;
                if path==root || pristine.get("instance.id")==snapshot.get("instance.id") {return Err("side-file donor identity is not independent".into());}
                let wal=|s:&BTreeMap<String,Option<Vec<u8>>>|s.get("instance.db-wal").and_then(Option::as_ref).cloned().ok_or("missing donor/target WAL");
                let own=wal(&pristine)?;let other=wal(&snapshot)?;
                if own[8..12]!=other[8..12] || own[16..24]==other[16..24] {return Err("side-file donor page size differs or salts are not distinct".into());}
                println!("INFO WV preflight_consistency donor={} case={case} donor_version=5 retained={}",path.display(),retained.display());
                Some((path,snapshot,retained))
            };
            if let Some((_,snapshot,_))=&donor {
                let name=if case=="foreign-wal" {"instance.db-wal"}else{"instance.db-shm"};
                let bytes=snapshot.get(name).and_then(Option::as_ref).ok_or("missing donor side file")?;
                if case=="foreign-wal" {
                    // Closed owned fixture only; retain original SHM before forcing WAL-index recovery.
                    fs::remove_file(root.join("instance.db-shm")).map_err(|e|e.to_string())?;
                    if support::entry_exists(&root.join("instance.db-shm"))?{return Err("target SHM still exists".into());}
                }
                fs::write(root.join(name),bytes).map_err(|e|e.to_string())?;
            }else{OpenOptions::new().write(true).open(root.join("instance.db")).and_then(|f|f.set_len(0)).map_err(|e|e.to_string())?;}
            let before=access_snapshot(&root)?;
            let altered=retain_damage_snapshot(&root,"retained-target-altered",&before)?;
            println!("INFO WV preflight_consistency fixture={} case={case} main_before=3 own_wal_version=4 original={} altered={} shm_before={}",root.display(),original.display(),altered.display(),before.contains_key("instance.db-shm"));
            store::test_api::clear();let result=store::open_instance(&root);let events=store::test_api::connection_events();
            let outcome=match result {
                Err(InstanceError::SchemaTooNew{found,maximum:2}) if case!="zero-main" && found==if case=="foreign-wal"{5}else{4} && events==["stage-a-read-only"]=>format!("SchemaTooNew{found}"),
                Err(InstanceError::SchemaInconsistent(reason)) if case=="zero-main" && reason=="zero-length database has a WAL side file" && events.is_empty()=>"SchemaInconsistentZeroMain".into(),
                Ok(i)=>{i.close().map_err(|e|e.to_string())?;return Err(format!("{case} unexpectedly admitted an Instance; events={events:?}"));},
                Err(e)=>return Err(format!("{case} wrong refusal: {e:?}; events={events:?}")),
            };
            let after=access_snapshot(&root)?;
            for name in before.keys().chain(after.keys()) {
                if (case=="zero-main" || name!="instance.db-shm") && before.get(name)!=after.get(name){return Err(format!("{case} refusal changed protected entry {name}"));}
            }
            let mut copies=vec![(&pristine,&original),(&before,&altered)];
            if let Some((path,snapshot,retained))=&donor {
                if access_snapshot(path)?!=*snapshot{return Err("target probe changed donor root".into());}
                copies.push((snapshot,retained));
            }
            for (snapshot,retained) in copies {
                if access_snapshot(retained)?!=*snapshot{return Err("side-file retained copy changed".into());}
            }
            for (name,bytes) in &after {
                if bytes.is_some(){drop(OpenOptions::new().read(true).write(true).share_mode(0).open(root.join(name)).map_err(|e|format!("side-file probe left open handle: {e}"))?);}
            }
            println!("INFO WV preflight_consistency case={case} refusal={outcome} sqlite_events={events:?} protected_entries_bytes_unchanged=true retained_copies_verified=true donor_unchanged={} released_exclusive_opens=allowed shm_after={} shm_changed={} no_repair=true",if donor.is_some(){"true"}else{"not_applicable"},after.contains_key("instance.db-shm"),before.get("instance.db-shm")!=after.get("instance.db-shm"));
        }
        Ok("independent genuine foreign WAL5/foreign SHM over own WAL4/main3 refuse exact future versions5/4 in Stage-A only; zero main with WAL refuses before SQLite; protected bytes, retained target/donor copies and donor roots unchanged, handles released; synthetic future versions only, no supported-version foreign-file safety or full PF12/W2 acceptance".into())
    }
    fn acl_command(root:&Path,journal:&Path,case:&str,operation:&str,label:&str)->Result<(),String>{
        let exe=PathBuf::from(std::env::var_os("YOKAI_TEST_POWERSHELL").ok_or("missing pinned Windows PowerShell")?);
        if !exe.is_absolute(){return Err("PowerShell path is not absolute".into());}
        let mut command=Command::new(exe);
        command.args(["-NoLogo","-NoProfile","-NonInteractive","-Command","-"]).creation_flags(0x08000000)
            .env("YOKAI_TEST_ACL_ROOT",root).env("YOKAI_TEST_ACL_JOURNAL",journal)
            .env("YOKAI_TEST_ACL_CASE",case).env("YOKAI_TEST_ACL_OPERATION",operation).env("YOKAI_TEST_ACL_LABEL",label)
            .stdin(Stdio::piped())
            .stdout(Stdio::from(File::create_new(journal.join(format!("{label}.out"))).map_err(|e|e.to_string())?))
            .stderr(Stdio::from(File::create_new(journal.join(format!("{label}.err"))).map_err(|e|e.to_string())?));
        let mut child=OwnedChild{child:command.spawn().map_err(|e|e.to_string())?};
        let mut input=child.child.stdin.take().ok_or("ACL helper stdin missing")?;
        input.write_all(include_str!("support/windows_acl.ps1").as_bytes()).map_err(|e|e.to_string())?;
        input.write_all(b"\n").map_err(|e|e.to_string())?;drop(input); // WinPS stdin needs a terminating blank line for a compound block.
        let status=wait(&mut child,Duration::from_secs(30))?;
        let out=fs::read_to_string(journal.join(format!("{label}.out"))).map_err(|e|e.to_string())?;
        let err=fs::read_to_string(journal.join(format!("{label}.err"))).map_err(|e|e.to_string())?;
        if !status.success() || out.lines().collect::<Vec<_>>()!=[format!("ACL_OPERATION {operation} PASS")] || !err.is_empty(){return Err(format!("ACL helper {label}: status={status} out={out:?} err={err:?}"));}
        Ok(())
    }
    struct AclGuard{root:PathBuf,journal:PathBuf,case:&'static str,restored:bool}
    impl AclGuard{
        fn deny(root:&Path,case:&'static str)->Result<Self,String>{
            let journal=root.parent().ok_or("ACL parent missing")?.join("acl-journal");
            fs::create_dir(&journal).map_err(|e|e.to_string())?;
            let guard=Self{root:root.to_owned(),journal,case,restored:false};
            acl_command(&guard.root,&guard.journal,case,"deny","deny")?;Ok(guard)
        }
        fn restore(mut self)->Result<(),String>{
            acl_command(&self.root,&self.journal,self.case,"restore","restore")?;self.restored=true;Ok(())
        }
    }
    impl Drop for AclGuard{fn drop(&mut self){
        if !self.restored && !matches!(support::entry_exists(&self.journal.join("original.json")),Ok(false)) {
            if let Err(e)=acl_command(&self.root,&self.journal,self.case,"restore","emergency-restore") {eprintln!("INFO WV ACL emergency restoration unconfirmed: {e}; wrapper must restore before sealing");}
        }
    }}
    fn access_denied(result:std::io::Result<()>,label:&str)->Result<(),String>{
        match result{Err(e) if e.raw_os_error()==Some(5)=>Ok(()),Err(e)=>Err(format!("{label}: expected access-denied5, got {e}")),Ok(())=>Err(format!("{label}: access unexpectedly permitted"))}
    }
    fn preflight_acl()->Result<String,String>{
        for case in ["db-read","shm-read","directory-create"] {
            let root=closed_sidefile_fixture("preflight-acl",4)?;
            let pristine=access_snapshot(&root)?;
            let original=retain_damage_snapshot(&root,"retained-acl-pristine",&pristine)?;
            if case=="directory-create" {
                drop(OpenOptions::new().read(true).write(true).share_mode(0).open(root.join("instance.db-shm")).map_err(|e|e.to_string())?);
                fs::remove_file(root.join("instance.db-shm")).map_err(|e|e.to_string())?;
                if support::entry_exists(&root.join("instance.db-shm"))?{return Err("ACL construction SHM still exists".into());}
            }
            let before=access_snapshot(&root)?;
            let retained=retain_damage_snapshot(&root,"retained-before-acl",&before)?;
            let guard=AclGuard::deny(&root,case)?;let journal=guard.journal.clone();
            if case=="directory-create" {
                let probe=root.join("acl-create-probe");
                if support::entry_exists(&probe)?{return Err("ACL create probe already exists".into());}
                access_denied(File::create_new(&probe).map(drop),case)?;
                if support::entry_exists(&probe)?{return Err("denied ACL create left an entry".into());}
            }else{access_denied(fs::read(root.join(if case=="db-read"{"instance.db"}else{"instance.db-shm"})).map(drop),case)?;}
            store::test_api::clear();let result=store::open_instance(&root);let events=store::test_api::connection_events();
            let reason=match result {
                Err(InstanceError::PreflightUnavailable(reason))=>reason,
                Ok(i)=>{i.close().map_err(|e|e.to_string())?;return Err(format!("{case} ACL-denied fixture admitted an Instance"));},
                Err(e)=>return Err(format!("{case} wrong ACL refusal: {e:?}; events={events:?}")),
            };
            let expected:Vec<&str>=if case=="directory-create"{vec!["stage-a-read-only"]}else{vec![]};
            if events!=expected{return Err(format!("{case} ACL refusal reached wrong SQLite path: {events:?}"));}
            if case=="directory-create" && access_snapshot(&root)?!=before{return Err("directory ACL refusal changed bytes while denied".into());}
            guard.restore()?;
            if access_snapshot(&root)?!=before{return Err("ACL refusal changed complete root entries/bytes".into());}
            for (snapshot,path) in [(&pristine,&original),(&before,&retained)] {
                if access_snapshot(path)?!=*snapshot{return Err("retained ACL baseline changed".into());}
            }
            for (name,bytes) in &before {
                if bytes.is_some(){drop(OpenOptions::new().read(true).write(true).share_mode(0).open(root.join(name)).map_err(|e|format!("ACL refusal left data handle: {e}"))?);}
            }
            future_wal_control(&root,4)?;let after=access_snapshot(&root)?;
            for name in before.keys().chain(after.keys()) {
                if name!="instance.db-shm" && before.get(name)!=after.get(name){return Err("post-ACL control changed non-SHM entry/byte".into());}
            }
            println!("INFO WV preflight_acl fixture={} case={case} pristine={} baseline={} journal={} raw_access_error=5 refusal=PreflightUnavailable sqlite_events={events:?} complete_refusal_entries_bytes_unchanged=true retained_copies_verified=true original_scoped_acl_rules_restored=true acl_control_effects_recorded=true released_exclusive_opens=allowed control_before=SchemaTooNew4 control_after=SchemaTooNew4 control_sqlite_events=[stage-a-read-only] control_shm_created={} reason={reason:?}",root.display(),original.display(),retained.display(),journal.display(),!before.contains_key("instance.db-shm") && after.contains_key("instance.db-shm"));
        }
        Ok("current-SID non-inheriting ReadData deny on main/SHM and directory CreateFiles/CreateDirectories deny with SHM absent: raw access-denied5 and exact PreflightUnavailable/no writable fallback; complete refusal bytes/copies and original permission ACEs/protection restored, auto-inheritance initialization recorded, released handles and read-only WAL4 controls verified; owned Windows fixtures only, not full descriptor restoration, Unix mode000 or full PF13/W2 acceptance".into())
    }
    fn stageb_inventory(snapshot:&BTreeMap<String,Option<Vec<u8>>>)->Result<store::FileInventory,String>{
        let mut entries=Vec::new();
        for name in ["instance.db","instance.db-wal","instance.db-shm","instance.db-journal"] {
            let bytes=match snapshot.get(name){Some(Some(bytes))=>Some(bytes),None=>None,_=>return Err("Stage-B inventory entry is not a regular file".into())};
            entries.push(store::InventoryEntry{name:name.into(),exists:bytes.is_some(),entry_type:bytes.map(|_|"regular".into()),
                size:bytes.map_or(0,|b|b.len() as u64),sha256:bytes.map(|b|store::test_api::sha256_chunks_for_test(&[b.as_slice()]))});
        }
        Ok(store::FileInventory{entries})
    }
    fn stageb_refusal(root:&Path)->Result<(String,String),String>{
        let before=access_snapshot(root)?;let inventory=stageb_inventory(&before)?;
        store::test_api::clear();let result=store::open_instance(root);let events=store::test_api::connection_events();
        let (cause,effects)=match result {
            Err(InstanceError::StageB{cause,observed_effects})=>(cause,observed_effects),
            Ok(i)=>{i.close().map_err(|e|e.to_string())?;return Err(format!("Stage-B fixture unexpectedly opened: {events:?}"));},
            Err(e)=>return Err(format!("wrong Stage-B envelope: {e:?}; events={events:?}")),
        };
        if events!=["stage-b-read-write"]{return Err(format!("Stage-B used wrong connection path: {events:?}"));}
        let after=access_snapshot(root)?;let measured=inventory.describe_difference(&stageb_inventory(&after)?);
        if effects!=measured{return Err(format!("Stage-B observed_effects={effects:?} measured={measured:?}"));}
        Ok((cause,effects))
    }
    fn stageb_released(root:&Path)->Result<(),String>{
        for (name,bytes) in access_snapshot(root)? {
            if bytes.is_some(){drop(OpenOptions::new().read(true).write(true).share_mode(0).open(root.join(name)).map_err(|e|format!("Stage-B retained data handle: {e}"))?);}
        }
        Ok(())
    }
    fn stageb_refusals()->Result<String,String>{
        for case in ["foreign-key","identity","objects-v0","corrupt-page"] {
            let i=instance("stageb-refusal")?;let root=i.root().to_owned();i.close().map_err(|e|e.to_string())?;
            let pristine=access_snapshot(&root)?;let original=retain_damage_snapshot(&root,"retained-stageb-pristine",&pristine)?;
            if pristine.keys().map(String::as_str).collect::<Vec<_>>()!=["instance.db","instance.id","instance.lock"]{return Err("Stage-B pristine fixture has side files or extras".into());}
            let c=Connection::open_with_flags(root.join("instance.db"),OpenFlags::SQLITE_OPEN_READ_WRITE|OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(|e|e.to_string())?;
            let mut corrupt_page=0i64;
            let expected=match case {
                "foreign-key"=>{
                    c.execute_batch("PRAGMA foreign_keys=OFF; INSERT INTO events(seq,run_id,kind,detail,epoch) VALUES(900001,987654321,'stageb.orphan','owned fixture',1);").map_err(|e|e.to_string())?;
                    let violations=store::test_api::foreign_key_violations(&c).map_err(|e|e.to_string())?;
                    let exact=vec!["events rowid=Some(900001) parent=runs fkid=0".to_owned()];
                    if violations!=exact{return Err(format!("Stage-B FK fixture differs: {violations:?}"));}
                    format!("foreign-key violations: {exact:?}")
                },
                "identity"=>{
                    c.execute_batch("UPDATE instance_meta SET instance_id=zeroblob(16) WHERE id=1;").map_err(|e|e.to_string())?;
                    "Instance identity does not match its directory".into()
                },
                "objects-v0"=>{
                    c.execute_batch("DROP TABLE events; DROP TABLE jobs; DROP TABLE commands; DROP TABLE runs; DROP TABLE runtime_epoch; DROP TABLE instance_meta; DROP TABLE yokai_schema_history; CREATE TABLE planted(id INTEGER); PRAGMA user_version=0;").map_err(|e|e.to_string())?;
                    "inconsistent schema: 1 schema objects at version 0".into()
                },
                "corrupt-page"=>{
                    corrupt_page=c.query_row("SELECT rootpage FROM sqlite_schema WHERE name='runs'",[],|r|r.get(0)).map_err(|e|e.to_string())?;
                    if corrupt_page<=1{return Err("corruption target is not a non-header runs rootpage".into());}
                    "inconsistent schema: database disk image is malformed".into()
                },_=>unreachable!(),
            };
            c.close().map_err(|(_,e)|e.to_string())?;
            if corrupt_page>0 {
                let path=root.join("instance.db");let mut bytes=fs::read(&path).map_err(|e|e.to_string())?;
                let page_size=match bytes.get(16..18){Some(b)=>u16::from_be_bytes([b[0],b[1]]) as usize,None=>return Err("short Stage-B database".into())};
                let page_size=if page_size==1{65536}else{page_size};let offset=(corrupt_page as usize-1).checked_mul(page_size).ok_or("corrupt offset overflow")?;
                if bytes.get(offset)!=Some(&13){return Err("Stage-B corruption target is not a table-leaf page".into());}
                bytes[offset]=0;fs::write(path,bytes).map_err(|e|e.to_string())?;
            }
            let before=access_snapshot(&root)?;
            if before.keys().map(String::as_str).collect::<Vec<_>>()!=["instance.db","instance.id","instance.lock"]{return Err("Stage-B baseline has side files or extras".into());}
            let retained=retain_damage_snapshot(&root,"retained-stageb-before",&before)?;
            let (cause,effects)=stageb_refusal(&root)?;let after=access_snapshot(&root)?;
            if cause!=expected{return Err(format!("{case} Stage-B cause={cause:?}; expected={expected:?}; effects={effects:?}"));}
            if before!=after{return Err(format!("{case} Stage-B refusal changed complete entries/bytes: {effects}"));}
            let final_copy=retain_damage_snapshot(&root,"retained-stageb-after",&after)?;
            for (snapshot,path) in [(&pristine,&original),(&before,&retained),(&after,&final_copy)] {
                if access_snapshot(path)?!=*snapshot{return Err("Stage-B retained copy differs".into());}
            }
            stageb_released(&root)?;
            let mut cmd=command("__stageb_refusal",&root)?;cmd.arg(&expected);let (mut child,logs)=launch(cmd,"stageb-probe")?;
            if !wait(&mut child,Duration::from_secs(15))?.success(){return Err(format!("Stage-B repeat opener failed; logs={}",logs.display()));}
            if access_snapshot(&root)?!=after{return Err("Stage-B child repeat changed root bytes".into());}
            println!("INFO WV stageb_refusal fixture={} case={case} pristine={} baseline={} after={} corrupt_page={corrupt_page} cause={cause:?} observed_effects={effects:?} sqlite_events=[stage-b-read-write] complete_entries_bytes_unchanged=true retained_copies_verified=true exclusive_data_opens=allowed child_lock_reacquired=true child_logs={} inventory_before={:?} inventory_after={:?}",root.display(),original.display(),retained.display(),final_copy.display(),logs.display(),stageb_inventory(&before)?,stageb_inventory(&after)?);
        }
        Ok("four owned Windows raw-header-accepted Stage-B refusals: exact FK/identity/version0-objects/corrupt-page causes and measured effects, all pristine/before/after bytes retained, data handles and fresh-child lock reacquisition checked; no full PF14/W2 acceptance".into())
    }
    fn stageb_active_snapshot(root:&Path)->Result<BTreeMap<String,Option<Vec<u8>>>,String>{
        let mut snapshot=BTreeMap::new();let mut lock_denied=false;
        for item in fs::read_dir(root).map_err(|e|e.to_string())? {
            let item=item.map_err(|e|e.to_string())?;let path=item.path();
            let name=item.file_name().into_string().map_err(|_|"non-UTF8 active legacy entry")?;
            let meta=fs::symlink_metadata(&path).map_err(|e|format!("active legacy {name} metadata: {e}"))?;
            yokai_platform::location::validate_regular_single_link(&path,&meta).map_err(|e|e.to_string())?;
            if name=="instance.lock" {
                match fs::read(&path){Err(e) if e.raw_os_error()==Some(32)=>lock_denied=true,Err(e)=>return Err(format!("active legacy lock wrong denial: {e}")),Ok(_)=>return Err("active legacy lock unexpectedly readable".into())}
            }else{snapshot.insert(name.clone(),Some(fs::read(path).map_err(|e|format!("active legacy {name} read: {e}"))?));}
        }
        if !lock_denied || snapshot.keys().map(String::as_str).collect::<Vec<_>>()!=["instance.db","instance.db-shm","instance.db-wal","instance.id"]{return Err("active legacy expected exact data files and sharing-denied32 lock".into());}
        Ok(snapshot)
    }
    fn stageb_legacy()->Result<String,String>{
        let source=PathBuf::from(std::env::var_os("YOKAI_TEST_V0_FIXTURE").ok_or("missing pinned V0 fixture")?);
        let original=access_snapshot(&source)?;
        if original.keys().map(String::as_str).collect::<Vec<_>>()!=["instance.db","instance.id","instance.lock"]{return Err("legacy source fixture has unexpected entries".into());}
        let name=source.file_name().and_then(|s|s.to_str()).ok_or("legacy fixture name invalid")?;
        if name.len()!=32 || !name.bytes().all(|b|b.is_ascii_digit() || (b'a'..=b'f').contains(&b)){return Err("legacy fixture identity directory invalid".into());}
        let root=parent("stageb-legacy")?.join(name);fs::create_dir(&root).map_err(|e|e.to_string())?;
        for (name,bytes) in &original {
            let mut copy=File::create_new(root.join(name)).map_err(|e|e.to_string())?;
            copy.write_all(bytes.as_ref().ok_or("legacy source contains a directory")?).map_err(|e|e.to_string())?;copy.sync_all().map_err(|e|e.to_string())?;
        }
        let before=access_snapshot(&root)?;if before!=original{return Err("legacy copied bytes differ".into());}
        let retained=retain_damage_snapshot(&root,"retained-legacy-before",&before)?;
        store::test_api::clear();let i=store::test_api::open_test_versions(&root,0,false).map_err(|e|e.to_string())?;
        if store::test_api::user_version(&i).map_err(|e|e.to_string())?!=0 || store::test_api::schema_objects(&i).map_err(|e|e.to_string())?!=0 || i.startup_report().is_some(){return Err("legacy control upgraded or has startup/schema state".into());}
        let open_events=store::test_api::connection_events();if open_events!=["stage-b-read-write"]{return Err(format!("legacy open events differ: {open_events:?}"));}
        let active=stageb_active_snapshot(&root)?;let active_copy=retain_damage_snapshot(&root,"retained-legacy-active",&active)?;
        for (name,bytes) in &before{if name!="instance.lock" && active.get(name)!=Some(bytes){return Err("legacy active control changed original bytes".into());}}
        i.close().map_err(|e|e.to_string())?;
        let close_events=store::test_api::connection_events();if close_events!=["connection-closed","lock-released"]{return Err(format!("legacy close events differ: {close_events:?}"));}
        let after=access_snapshot(&root)?;let after_copy=retain_damage_snapshot(&root,"retained-legacy-after",&after)?;
        if before!=after || access_snapshot(&source)?!=original{return Err("legacy close or original fixture bytes changed".into());}
        for (snapshot,path) in [(&before,&retained),(&active,&active_copy),(&after,&after_copy)]{if access_snapshot(path)?!=*snapshot{return Err("legacy retained snapshot differs".into());}}
        stageb_released(&root)?;
        println!("INFO WV stageb_legacy source={} fixture={} baseline={} active={} after={} version=0 objects=0 startup_report=none open_events={open_events:?} close_events={close_events:?} complete_closed_bytes_unchanged=true source_unchanged=true retained_copies_verified=true exclusive_data_opens=allowed active_lock_read=not_checked active_lock_raw_error=32 active_root_names_types=verified inventory_before={:?} inventory_active={:?} inventory_after={:?}",source.display(),root.display(),retained.display(),active_copy.display(),after_copy.display(),stageb_inventory(&before)?,stageb_inventory(&active)?,stageb_inventory(&after)?);
        Ok("copied accepted V0 fixture opens through current shared code with test-only empty migration plan; version0/zero objects/no startup report, explicit close and exact closed bytes/source preserved; active versus closed side files retained, not a historical binary rerun or non-mutating V2 startup claim".into())
    }
    fn semantic_preserved(before:&BTreeMap<String,Option<Vec<u8>>>,after:&BTreeMap<String,Option<Vec<u8>>>,allow_shm:bool)->Result<(),String>{
        for name in before.keys().chain(after.keys()) {
            if (!allow_shm || name!="instance.db-shm") && before.get(name)!=after.get(name){return Err(format!("semantic refusal changed protected entry {name}"));}
        }
        stageb_inventory(before)?;stageb_inventory(after)?;Ok(())
    }
    fn semantic_refusal(root:&Path,case:&str,expected:&str)->Result<String,String>{
        store::test_api::clear();let result=store::open_instance(root);let events=store::test_api::connection_events();
        let (kind,detail)=match (case,result) {
            ("foreign-key",Err(InstanceError::ForeignKeyViolations(rows)))=>("ForeignKeyViolations",format!("{rows:?}")),
            ("identity",Err(InstanceError::IdentityMismatch))=>("IdentityMismatch",String::new()),
            ("objects-v0",Err(InstanceError::SchemaInconsistent(detail)))=>("SchemaInconsistent",detail),
            ("integrity",Err(InstanceError::Integrity(detail)))=>("Integrity",detail),
            (_,Ok(i))=>{i.close().map_err(|e|e.to_string())?;return Err(format!("{case} semantic fixture unexpectedly opened; events={events:?}"));},
            (_,Err(e))=>return Err(format!("{case} wrong semantic refusal {e:?}; events={events:?}")),
        };
        if detail!=expected || events!=["stage-a-read-only"]{return Err(format!("{case} semantic detail={detail:?} expected={expected:?} events={events:?}"));}
        Ok(format!("{kind}({detail:?})"))
    }
    fn semantic_repeat(root:&Path,case:&str,expected:&str)->Result<PathBuf,String>{
        let mut cmd=command("__semantic_refusal",root)?;cmd.arg(case).arg(expected);let (mut child,logs)=launch(cmd,"semantic-probe")?;
        if !wait(&mut child,Duration::from_secs(15))?.success(){return Err(format!("semantic repeat failed; logs={}",logs.display()));}Ok(logs)
    }
    fn preflight_semantic()->Result<String,String>{
        for case in ["foreign-key","identity","objects-v0"] {
            let i=instance("semantic-wal")?;let root=i.root().to_owned();i.close().map_err(|e|e.to_string())?;
            let pristine=access_snapshot(&root)?;let original=retain_damage_snapshot(&root,"retained-semantic-pristine",&pristine)?;
            if pristine.keys().map(String::as_str).collect::<Vec<_>>()!=["instance.db","instance.id","instance.lock"]{return Err("semantic pristine fixture has side files/extras".into());}
            let c=Connection::open_with_flags(root.join("instance.db"),OpenFlags::SQLITE_OPEN_READ_WRITE|OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(|e|e.to_string())?;
            c.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,true).map_err(|e|e.to_string())?;
            c.execute_batch("PRAGMA wal_autocheckpoint=0;").map_err(|e|e.to_string())?;
            let version:i64=c.query_row("PRAGMA user_version",[],|r|r.get(0)).map_err(|e|e.to_string())?;
            let integrity:String=c.query_row("PRAGMA integrity_check",[],|r|r.get(0)).map_err(|e|e.to_string())?;
            if version!=2 || integrity!="ok" || !store::test_api::foreign_key_violations(&c).map_err(|e|e.to_string())?.is_empty(){return Err("semantic pristine control is not valid V2".into());}
            let expected=match case {
                "foreign-key"=>{
                    c.execute_batch("PRAGMA foreign_keys=OFF; BEGIN; INSERT INTO events(seq,run_id,kind,detail,epoch) VALUES(900002,987654322,'preflight.orphan','owned WAL fixture',1); COMMIT;").map_err(|e|e.to_string())?;
                    "[\"events rowid=Some(900002) parent=runs fkid=0\"]"
                },
                "identity"=>{c.execute_batch("BEGIN; UPDATE instance_meta SET instance_id=zeroblob(16) WHERE id=1; COMMIT;").map_err(|e|e.to_string())?;""},
                "objects-v0"=>{
                    c.execute_batch("BEGIN; DROP TABLE events; DROP TABLE jobs; DROP TABLE commands; DROP TABLE runs; DROP TABLE runtime_epoch; DROP TABLE instance_meta; DROP TABLE yokai_schema_history; CREATE TABLE planted(id INTEGER); PRAGMA user_version=0; COMMIT;").map_err(|e|e.to_string())?;
                    "1 schema objects at version 0"
                },_=>unreachable!(),
            };
            c.close().map_err(|(_,e)|e.to_string())?;let before=access_snapshot(&root)?;
            if before.keys().map(String::as_str).collect::<Vec<_>>()!=["instance.db","instance.db-shm","instance.db-wal","instance.id","instance.lock"]
                || before.get("instance.db")!=pristine.get("instance.db"){return Err("semantic WAL fixture changed main or has wrong side files".into());}
            let wal=before.get("instance.db-wal").and_then(Option::as_ref).ok_or("missing semantic WAL")?;
            if wal.len()<=32{return Err("semantic defect has no nonempty WAL frames".into());}
            let baseline=retain_damage_snapshot(&root,"retained-semantic-before",&before)?;
            let outcome=semantic_refusal(&root,case,expected)?;let after=access_snapshot(&root)?;
            semantic_preserved(&before,&after,true)?;let final_copy=retain_damage_snapshot(&root,"retained-semantic-after",&after)?;
            stageb_released(&root)?;let logs=semantic_repeat(&root,case,expected)?;let child_after=access_snapshot(&root)?;
            semantic_preserved(&after,&child_after,true)?;stageb_released(&root)?;
            let child_copy=retain_damage_snapshot(&root,"retained-semantic-child-after",&child_after)?;
            for (snapshot,path) in [(&pristine,&original),(&before,&baseline),(&after,&final_copy),(&child_after,&child_copy)] {
                if access_snapshot(path)?!=*snapshot{return Err("semantic retained copy differs".into());}
            }
            println!("INFO WV preflight_semantic fixture={} case={case} pristine={} baseline={} after={} child_after={} child_logs={} outcome={outcome} expected_detail={expected:?} main_pristine_unchanged=true wal_bytes={} sqlite_events=[stage-a-read-only] protected_entries_bytes_unchanged=true retained_copies_verified=true exclusive_data_opens=allowed child_lock_reacquired=true shm_changed={} child_shm_changed={} inventory_before={:?} inventory_after={:?} inventory_child_after={:?}",root.display(),original.display(),baseline.display(),final_copy.display(),child_copy.display(),logs.display(),wal.len(),before.get("instance.db-shm")!=after.get("instance.db-shm"),after.get("instance.db-shm")!=child_after.get("instance.db-shm"),stageb_inventory(&before)?,stageb_inventory(&after)?,stageb_inventory(&child_after)?);
        }
        Ok("three owned genuine committed-WAL semantic refusals: exact FK orphan/identity/version0-objects errors in Stage-A read-only only; pristine main, all protected entries, retained inventories/copies and handle/child lock release verified; SHM effects recorded, no full PF6/PF7/PF8/W2 acceptance".into())
    }
    fn preflight_integrity()->Result<String,String>{
        for wal_present in [false,true] {
            let i=instance("integrity-row")?;let root=i.root().to_owned();i.close().map_err(|e|e.to_string())?;
            let pristine=access_snapshot(&root)?;let original=retain_damage_snapshot(&root,"retained-integrity-pristine",&pristine)?;
            if pristine.keys().map(String::as_str).collect::<Vec<_>>()!=["instance.db","instance.id","instance.lock"]{return Err("integrity pristine has side files/extras".into());}
            let c=Connection::open_with_flags(root.join("instance.db"),OpenFlags::SQLITE_OPEN_READ_WRITE|OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(|e|e.to_string())?;
            let page:i64=c.query_row("SELECT rootpage FROM sqlite_schema WHERE name='runs'",[],|r|r.get(0)).map_err(|e|e.to_string())?;
            let rows:i64=c.query_row("SELECT count(*) FROM runs",[],|r|r.get(0)).map_err(|e|e.to_string())?;
            let integrity:String=c.query_row("PRAGMA integrity_check",[],|r|r.get(0)).map_err(|e|e.to_string())?;
            c.close().map_err(|(_,e)|e.to_string())?;
            if page<=1 || rows!=0 || integrity!="ok" || access_snapshot(&root)?!=pristine{return Err("integrity control is not unchanged empty runs table".into());}
            let path=root.join("instance.db");let mut bytes=fs::read(&path).map_err(|e|e.to_string())?;
            let raw=u16::from_be_bytes(bytes.get(16..18).ok_or("short integrity header")?.try_into().map_err(|_|"integrity page size")?);
            let size=if raw==1{65536}else{raw as usize};let offset=(page as usize-1).checked_mul(size).ok_or("integrity offset overflow")?;
            let empty=[13,0,0,0,0,(size>>8) as u8,size as u8,0];
            if bytes.get(offset..offset+8)!=Some(empty.as_slice()){return Err("integrity target is not an empty leaf with zero fragments".into());}
            bytes[offset+7]=1;fs::write(path,bytes).map_err(|e|e.to_string())?;
            if wal_present {File::create_new(root.join("instance.db-wal")).and_then(|f|f.sync_all()).map_err(|e|e.to_string())?;}
            let before=access_snapshot(&root)?;let baseline=retain_damage_snapshot(&root,"retained-integrity-before",&before)?;
            let detail=format!("{:?}",vec![format!("*** in database main ***\nTree {page} page {page}: free space corruption")]);
            let expected_cause=format!("integrity check failed: {detail}");
            let outcome=if wal_present {semantic_refusal(&root,"integrity",&detail)?}else{
                let (cause,effects)=stageb_refusal(&root)?;
                if cause!=expected_cause || effects!="no inventory changes"{return Err(format!("raw integrity cause={cause:?} expected={expected_cause:?} effects={effects:?}"));}
                format!("StageB({cause:?})")
            };
            let after=access_snapshot(&root)?;semantic_preserved(&before,&after,wal_present)?;
            let final_copy=retain_damage_snapshot(&root,"retained-integrity-after",&after)?;stageb_released(&root)?;
            let logs=if wal_present {semantic_repeat(&root,"integrity",&detail)?}else{
                let mut cmd=command("__stageb_refusal",&root)?;cmd.arg(&expected_cause);let (mut child,logs)=launch(cmd,"integrity-probe")?;
                if !wait(&mut child,Duration::from_secs(15))?.success(){return Err(format!("raw integrity repeat failed; logs={}",logs.display()));}logs
            };
            let child_after=access_snapshot(&root)?;semantic_preserved(&after,&child_after,wal_present)?;stageb_released(&root)?;
            let child_copy=retain_damage_snapshot(&root,"retained-integrity-child-after",&child_after)?;
            for (snapshot,path) in [(&pristine,&original),(&before,&baseline),(&after,&final_copy),(&child_after,&child_copy)] {
                if access_snapshot(path)?!=*snapshot{return Err("integrity retained copy differs".into());}
            }
            println!("INFO WV preflight_integrity fixture={} wal_present={wal_present} runs_page={page} changed_offset={} pristine={} baseline={} after={} child_after={} child_logs={} outcome={outcome} expected_detail={detail:?} fragment_before=0 fragment_after=1 protected_entries_bytes_unchanged=true retained_copies_verified=true exclusive_data_opens=allowed child_lock_reacquired=true shm_changed={} child_shm_changed={} inventory_before={:?} inventory_after={:?} inventory_child_after={:?}",root.display(),offset+7,original.display(),baseline.display(),final_copy.display(),child_copy.display(),logs.display(),before.get("instance.db-shm")!=after.get("instance.db-shm"),after.get("instance.db-shm")!=child_after.get("instance.db-shm"),stageb_inventory(&before)?,stageb_inventory(&after)?,stageb_inventory(&child_after)?);
        }
        Ok("one-byte empty runs-leaf fragmentation defect returns exact Integrity result row: StageB with no WAL and typed Stage-A read-only with empty WAL; complete retained inventories/protected bytes and handles/fresh-child ownership verified; not every corruption mode or a retained extended SQLite error code".into())
    }
    fn corruption_refusal(root:&Path,case:&str,page:u32)->Result<String,String>{
        store::test_api::clear();
        let result=store::open_instance(root);let events=store::test_api::connection_events();
        let (outcome,expected_events)=match (case,result) {
            ("magic",Err(InstanceError::HeaderInvalid(field))) if field=="magic"=>("HeaderInvalid(magic)".to_owned(),Vec::<&str>::new()),
            ("half",Err(InstanceError::HeaderInvalid(field))) if field=="database length"=>("HeaderInvalid(database length)".to_owned(),Vec::<&str>::new()),
            ("leaf",Err(InstanceError::Integrity(detail)))=>{
                let expected=format!("{:?}",vec![format!("*** in database main ***\nTree {page} page {page}: free space corruption")]);
                if detail!=expected{return Err(format!("corruption Integrity row={detail:?} expected={expected:?}"));}
                (format!("Integrity({detail:?})"),vec!["stage-a-read-only"])
            },
            (_,Ok(i))=>{i.close().map_err(|e|e.to_string())?;return Err(format!("{case} corrupt fixture unexpectedly opened; events={events:?}"));},
            (_,Err(error))=>return Err(format!("{case} wrong corruption refusal {error:?}; events={events:?}")),
        };
        if events!=expected_events{return Err(format!("{case} wrong corruption route {events:?}"));}
        Ok(outcome)
    }
    fn preflight_corruption()->Result<String,String>{
        const MARKER:&str="2000-01-01T00:00:01Z";
        for case in ["magic","leaf","half"] {
            let i=instance("main-corruption")?;let root=i.root().to_owned();i.close().map_err(|e|e.to_string())?;
            let pristine=access_snapshot(&root)?;let original=retain_damage_snapshot(&root,"retained-corruption-pristine",&pristine)?;
            if pristine.keys().map(String::as_str).collect::<Vec<_>>()!=["instance.db","instance.id","instance.lock"]{return Err("corruption pristine has side files/extras".into());}
            let c=Connection::open_with_flags(root.join("instance.db"),OpenFlags::SQLITE_OPEN_READ_WRITE|OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(|e|e.to_string())?;
            c.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,true).map_err(|e|e.to_string())?;
            c.execute_batch("PRAGMA wal_autocheckpoint=0;").map_err(|e|e.to_string())?;
            let page:u32=c.query_row("SELECT rootpage FROM sqlite_schema WHERE name='runs'",[],|r|r.get(0)).map_err(|e|e.to_string())?;
            let meta_page:u32=c.query_row("SELECT rootpage FROM sqlite_schema WHERE name='instance_meta'",[],|r|r.get(0)).map_err(|e|e.to_string())?;
            let rows:i64=c.query_row("SELECT count(*) FROM runs",[],|r|r.get(0)).map_err(|e|e.to_string())?;
            let version:i64=c.query_row("PRAGMA user_version",[],|r|r.get(0)).map_err(|e|e.to_string())?;
            let integrity:String=c.query_row("PRAGMA integrity_check",[],|r|r.get(0)).map_err(|e|e.to_string())?;
            if page<=1 || meta_page<=1 || page==meta_page || rows!=0 || version!=2 || integrity!="ok"
                || !store::test_api::foreign_key_violations(&c).map_err(|e|e.to_string())?.is_empty(){return Err("corruption pristine V2 control failed".into());}
            c.execute_batch("BEGIN IMMEDIATE;").map_err(|e|e.to_string())?;
            if c.execute("UPDATE instance_meta SET last_health_check=?1 WHERE id=1",[MARKER]).map_err(|e|e.to_string())?!=1{return Err("corruption marker update did not affect one row".into());}
            c.execute_batch("COMMIT;").map_err(|e|e.to_string())?;
            c.close().map_err(|(_,e)|e.to_string())?;
            let committed=access_snapshot(&root)?;
            if committed.get("instance.db")!=pristine.get("instance.db"){return Err("corruption marker checkpointed into main".into());}
            let wal=committed.get("instance.db-wal").and_then(Option::as_ref).ok_or("corruption WAL missing")?;
            let main=pristine.get("instance.db").and_then(Option::as_ref).ok_or("corruption main missing")?;
            let size=4096usize;
            if main.len()!=13*size || main.get(16..18)!=Some([0x10,0].as_slice()) || wal.len()<=32
                || (wal.len()-32)%(size+24)!=0 || wal.get(8..12)!=Some((size as u32).to_be_bytes().as_slice()) {
                return Err("corruption fixture geometry/WAL framing mismatch".into());
            }
            let mut frame_pages=Vec::new();let frames=(wal.len()-32)/(size+24);
            for (index,frame) in wal[32..].chunks_exact(size+24).enumerate(){
                let number=u32::from_be_bytes(frame[..4].try_into().map_err(|_|"corruption frame page")?);
                let commit=u32::from_be_bytes(frame[4..8].try_into().map_err(|_|"corruption frame commit")?);
                if number!=meta_page || number==1 || number==page || frame[8..16]!=wal[16..24]
                    || (index+1==frames && commit!=13) || (index+1<frames && commit!=0){return Err("corruption WAL masks main fault or lacks exact commit".into());}
                frame_pages.push(number);
            }
            let control=ro(&root)?;
            let marker:String=control.query_row("SELECT last_health_check FROM instance_meta WHERE id=1",[],|r|r.get(0)).map_err(|e|e.to_string())?;
            let control_version:i64=control.query_row("PRAGMA user_version",[],|r|r.get(0)).map_err(|e|e.to_string())?;
            let control_integrity:String=control.query_row("PRAGMA integrity_check",[],|r|r.get(0)).map_err(|e|e.to_string())?;
            if marker!=MARKER || control_version!=2 || control_integrity!="ok"
                || !store::test_api::foreign_key_violations(&control).map_err(|e|e.to_string())?.is_empty(){return Err("corruption read-only healthy WAL control failed".into());}
            control.close().map_err(|(_,e)|e.to_string())?;
            let healthy=access_snapshot(&root)?;semantic_preserved(&committed,&healthy,true)?;
            if healthy.keys().map(String::as_str).collect::<Vec<_>>()!=["instance.db","instance.db-shm","instance.db-wal","instance.id","instance.lock"]{return Err("corruption healthy fixture has wrong entries".into());}
            let healthy_copy=retain_damage_snapshot(&root,"retained-corruption-healthy",&healthy)?;
            let offset=(page as usize-1).checked_mul(size).ok_or("corruption page offset overflow")?;
            let empty=[13,0,0,0,0,0x10,0,0];
            if main.get(offset..offset+8)!=Some(empty.as_slice()){return Err("corruption target is not an empty runs leaf".into());}
            let mut damaged=main.clone();
            match case {"magic"=>damaged[..16].fill(0xa5),"leaf"=>damaged[offset+7]=1,"half"=>damaged.truncate(main.len()/2),_=>unreachable!()}
            fs::write(root.join("instance.db"),&damaged).map_err(|e|e.to_string())?;
            let before=access_snapshot(&root)?;
            if before.get("instance.db")!=Some(&Some(damaged)) || before.iter().any(|(name,bytes)|name!="instance.db" && healthy.get(name)!=Some(bytes)) {return Err("corruption setup changed another entry".into());}
            let baseline=retain_damage_snapshot(&root,"retained-corruption-before",&before)?;
            let outcome=corruption_refusal(&root,case,page)?;let after=access_snapshot(&root)?;
            let allow_shm=case=="leaf";semantic_preserved(&before,&after,allow_shm)?;
            let final_copy=retain_damage_snapshot(&root,"retained-corruption-after",&after)?;stageb_released(&root)?;
            let mut probe=command("__corruption_refusal",&root)?;probe.arg(case).arg(page.to_string());
            let (mut child,logs)=launch(probe,"corruption-probe")?;
            if !wait(&mut child,Duration::from_secs(15))?.success(){return Err(format!("corruption child repeat failed; logs={}",logs.display()));}
            let child_after=access_snapshot(&root)?;semantic_preserved(&after,&child_after,allow_shm)?;stageb_released(&root)?;
            let child_copy=retain_damage_snapshot(&root,"retained-corruption-child-after",&child_after)?;
            for (snapshot,path) in [(&pristine,&original),(&healthy,&healthy_copy),(&before,&baseline),(&after,&final_copy),(&child_after,&child_copy)]{
                if access_snapshot(path)?!=*snapshot{return Err("corruption retained copy differs".into());}
            }
            println!("INFO WV preflight_corruption fixture={} case={case} pristine={} healthy={} baseline={} after={} child_after={} child_logs={} runs_page={page} meta_page={meta_page} frame_pages={frame_pages:?} commit_pages=13 marker={MARKER} original_bytes={} damaged_bytes={} fault_offset={} outcome={outcome} allow_shm={allow_shm} healthy_control_version=2 healthy_control_integrity=ok main_WAL_protected=true all_copies_verified=true exclusive_data_opens=allowed child_lock_reacquired=true shm_changed={} child_shm_changed={} inventory_before={:?} inventory_after={:?} inventory_child_after={:?}",root.display(),original.display(),healthy_copy.display(),baseline.display(),final_copy.display(),child_copy.display(),logs.display(),main.len(),before.get("instance.db").and_then(Option::as_ref).ok_or("missing damaged main")?.len(),if case=="leaf"{offset+7}else{0},before.get("instance.db-shm")!=after.get("instance.db-shm"),after.get("instance.db-shm")!=child_after.get("instance.db-shm"),stageb_inventory(&before)?,stageb_inventory(&after)?,stageb_inventory(&child_after)?);
        }
        Ok("three genuine committed-WAL main faults: exact header-magic and half-length refusals before SQLite, one-byte non-header leaf Integrity via Stage-A read-only; healthy marker control, all retained/protected bytes, inventories and fresh-child ownership verified; no full corruption/PF6/W2 acceptance".into())
    }

    fn raw_header_case(case:&str)->Result<(Option<&'static str>,u32),String>{
        let field=match case {
            "magic"=>Some("magic"),"page-1000"|"page-65535"=>Some("page size"),
            "format"=>Some("read/write format versions"),"reserved"=>Some("reserved bytes"),
            "fractions"=>Some("payload fractions"),"schema"=>Some("schema format"),
            "encoding"=>Some("text encoding"),"length"=>Some("database length"),
            "version-3"|"version-255"|"version-256"|"version-65536"|"version-16777216"|"version-4294967295"=>None,
            _=>return Err(format!("unknown raw-header case {case}")),
        };
        let value=if field.is_some(){u32::MAX}else{case.strip_prefix("version-").ok_or("missing version label")?.parse::<u32>().map_err(|e|e.to_string())?};
        Ok((field,value))
    }
    fn raw_header_refusal(root:&Path,case:&str)->Result<String,String>{
        let (field,value)=raw_header_case(case)?;
        store::test_api::clear();let result=store::open_instance(root);let events=store::test_api::connection_events();
        let outcome=match (field,result) {
            (Some(expected),Err(InstanceError::HeaderInvalid(actual))) if actual==expected=>format!("HeaderInvalid({actual})"),
            (None,Err(InstanceError::SchemaTooNew{found,maximum:2})) if found==value=>format!("SchemaTooNew({found},2)"),
            (_,Ok(i))=>{i.close().map_err(|e|e.to_string())?;return Err(format!("raw-header {case} unexpectedly opened; events={events:?}"));},
            (_,Err(error))=>return Err(format!("raw-header {case} wrong refusal {error:?}; events={events:?}")),
        };
        if !events.is_empty(){return Err(format!("raw-header {case} opened a connection: {events:?}"));}
        let decoded=store::test_api::header_versions()?;
        let expected=if field.is_some(){Vec::new()}else{vec![value]};
        let second_take=store::test_api::header_versions()?;
        if decoded!=expected || !second_take.is_empty(){return Err(format!("raw-header {case} decoded={decoded:?} expected={expected:?} second_take={second_take:?}"));}
        println!("INFO WV header_decode case={case} decoded={decoded:?} second_take={second_take:?}");
        Ok(outcome)
    }
    fn raw_headers()->Result<String,String>{
        for case in ["magic","page-1000","page-65535","format","reserved","fractions","schema","encoding","length",
            "version-3","version-255","version-256","version-65536","version-16777216","version-4294967295"] {
            let (field,value)=raw_header_case(case)?;
            let i=instance("raw-header")?;let root=i.root().to_owned();
            if store::test_api::user_version(&i).map_err(|e|e.to_string())?!=2{return Err("raw-header pristine control is not V2".into());}
            i.close().map_err(|e|e.to_string())?;
            let pristine=access_snapshot(&root)?;
            if pristine.keys().map(String::as_str).collect::<Vec<_>>()!=["instance.db","instance.id","instance.lock"]
                || pristine.get("instance.lock")!=Some(&Some(Vec::new())){return Err("raw-header pristine has extras/side files/nonempty lock".into());}
            let main=pristine.get("instance.db").and_then(Option::as_ref).ok_or("raw-header pristine main missing")?;
            if main.len()<100 || main.len()%4096!=0 || &main[..16]!=b"SQLite format 3\0"
                || main[16..24]!=[0x10,0,2,2,0,64,32,32] || main[44..48]!=4u32.to_be_bytes()
                || main[56..60]!=1u32.to_be_bytes() || main[60..64]!=2u32.to_be_bytes(){return Err("raw-header pristine encoding/layout differs".into());}
            let original=retain_damage_snapshot(&root,"retained-header-pristine",&pristine)?;
            let mut edited=main.clone();edited[60..64].copy_from_slice(&value.to_be_bytes());
            match case {
                "magic"=>edited[0]^=1,
                "page-1000"=>edited[16..18].copy_from_slice(&1000u16.to_be_bytes()),
                "page-65535"=>edited[16..18].copy_from_slice(&u16::MAX.to_be_bytes()),
                "format"=>{edited[18]=3;edited[19]=3;},"reserved"=>edited[20]=8,
                "fractions"=>edited[21]=65,"schema"=>edited[44..48].copy_from_slice(&5u32.to_be_bytes()),
                "encoding"=>edited[56..60].copy_from_slice(&2u32.to_be_bytes()),
                "length"=>{edited.pop().ok_or("raw-header main empty")?;},
                _ if field.is_none()=>{},_=>return Err("raw-header mutation case differs".into()),
            }
            fs::write(root.join("instance.db"),&edited).map_err(|e|e.to_string())?;
            let before=access_snapshot(&root)?;let mut expected=pristine.clone();expected.insert("instance.db".into(),Some(edited));
            if before!=expected{return Err("raw-header setup changed another entry".into());}
            let baseline=retain_damage_snapshot(&root,"retained-header-before",&before)?;
            let outcome=raw_header_refusal(&root,case)?;let after=access_snapshot(&root)?;
            if before!=after{return Err("raw-header parent refusal changed closed bytes/names/types".into());}
            let final_copy=retain_damage_snapshot(&root,"retained-header-after",&after)?;stageb_released(&root)?;
            let mut probe=command("__raw_header_refusal",&root)?;probe.arg(case);
            let (mut child,logs)=launch(probe,"raw-header-probe")?;
            if !wait(&mut child,Duration::from_secs(15))?.success(){return Err(format!("raw-header child failed; logs={}",logs.display()));}
            let repeated=access_snapshot(&root)?;
            if after!=repeated{return Err("raw-header child refusal changed closed bytes/names/types".into());}
            stageb_released(&root)?;
            let child_copy=retain_damage_snapshot(&root,"retained-header-child-after",&repeated)?;
            for (snapshot,path) in [(&pristine,&original),(&before,&baseline),(&after,&final_copy),(&repeated,&child_copy)] {
                if access_snapshot(path)?!=*snapshot{return Err("raw-header retained copy differs".into());}
            }
            println!("INFO WV raw_header fixture={} case={case} pristine={} baseline={} after={} child_after={} child_logs={} header_version={value} original_bytes={} altered_bytes={} outcome={outcome} sqlite_events=[] complete_closed_bytes_unchanged=true side_files_absent=true all_copies_verified=true exclusive_data_opens=allowed child_lock_reacquired=true inventory_before={:?} inventory_after={:?} inventory_child_after={:?}",root.display(),original.display(),baseline.display(),final_copy.display(),child_copy.display(),logs.display(),main.len(),before.get("instance.db").and_then(Option::as_ref).ok_or("missing raw-header main")?.len(),stageb_inventory(&before)?,stageb_inventory(&after)?,stageb_inventory(&repeated)?);
        }
        Ok("nine exact malformed raw-header fields with max-version precedence and six future big-endian versions refused before SQLite;180 retained files, complete closed inventories and fresh-child ownership verified;65535 is the actual malformed upper page encoding, not131072; accepted0/1/2 and exhaustive PF16/PF17/W2 remain separate".into())
    }

    fn lock_soak()->Result<String,String>{
        let start=Instant::now();let i=instance("lock-soak")?;let root=i.root().to_owned();let until=Instant::now()+Duration::from_secs(20);let mut rounds=0;
        while rounds<64 && Instant::now()<until {
            let id=format!("soak-{rounds}");if !matches!(accept(&i.store(),&id,b"synthetic")?,AcceptOutcome::Accepted{..}){return Err("soak command not accepted".into());}
            i.store().submit_health_check(deadline()).map_err(|e|e.to_string())?.wait(deadline()).map_err(|e|e.to_string())?;
            let (mut child,_)=launch(command("__busy",&root)?,"soak-busy")?;
            if !wait(&mut child,Duration::from_secs(5))?.success(){return Err("second-process soak opener not Busy".into());}
            rounds+=1;std::thread::sleep(Duration::from_millis(250));
        }
        i.close().map_err(|e|e.to_string())?;if rounds<2 || start.elapsed()>Duration::from_secs(60){return Err("bounded soak interval did not complete".into());}
        let c=ro(&root)?;let commands:i64=c.query_row("SELECT count(*) FROM commands",[],|r|r.get(0)).map_err(|e|e.to_string())?;
        if commands!=rounds{return Err("soak durable count mismatch".into());}
        Ok(format!("{rounds} command/health commits and child Busy probes in {:?}; natural host activity only, no controlled Defender/indexer interference or SLA claim",start.elapsed()))
    }
}
