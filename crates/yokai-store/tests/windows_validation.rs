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
mod native {
    use super::support;
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
            ("WV-LOCK-SOAK",lock_soak),
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
