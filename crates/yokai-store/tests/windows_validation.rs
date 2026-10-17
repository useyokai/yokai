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
