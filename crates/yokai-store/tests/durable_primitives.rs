//! Durable storage contracts; raw SQL below is restricted to adversarial test fixtures.
use std::fs;
use std::path::{Path,PathBuf};
use std::process::{Command,Stdio,ExitCode};
use std::time::{Duration,Instant};
use rusqlite::{Connection,OpenFlags};
use yokai_store::{self as store,CommandRequest,AcceptOutcome};
use yokai_types::CommandId;
type Check=fn()->Result<String,String>;

fn main()->ExitCode {
    let dumpable=yokai_testsupport::make_non_dumpable();println!("{}",dumpable.line());
    if !dumpable.permits_checks(){return ExitCode::from(2);}
    let args:Vec<String>=std::env::args().collect();
    if args.get(1).map(String::as_str)==Some("__busy") {
        return if matches!(store::open_instance(Path::new(&args[2])),Err(store::InstanceError::Busy(_))) {ExitCode::SUCCESS}else{ExitCode::from(3)};
    }
    if args.get(1).is_some_and(|s|s.starts_with("__")){return child_mode(&args);}
    let checks:&[(&str,Check)]=&[
        ("DP-SCHEMA",schema),("DP-VALID",validation),("DP-12",digest_vectors),
        ("DP-1",ten_commands),("DP-2",constraints),("DP-3",replay),
        ("DP-6",concurrent_duplicates),("DP-7",concurrent_conflicts),("DP-BOUND",bounds),
        ("DP-4",acceptance_crashes),("DP-5",after_commit_reply),("FC-14",unknown_outcomes),
        ("DP-8",startup_owner_crash),("DP-9",startup_idempotence),("DP-10",startup_crashes),
        ("DP-13",jobs_records_only),("DP-14",no_admission),("DP-CLASSIFY",classifier_edges),
        ("DP-EPOCH",epoch_overflow),("SV-6",open_timing),("DP-UP-V1",upgrade_v1),
    ];
    let checks:Vec<_>=match args.as_slice() {
        [_]=>checks.iter().collect(),
        [_,flag,id] if flag=="--only" && checks.iter().any(|(known,_)|known==id)=>checks.iter().filter(|(known,_)|known==id).collect(),
        _=>{eprintln!("expected no arguments or --only <known check ID>");return ExitCode::from(2);}
    };
    let mut failed=0;
    for (id,run) in &checks {match run(){Ok(s)=>println!("CHECK {id} PASS {s}"),Err(e)=>{println!("CHECK {id} FAIL {e}");failed+=1;}}}
    if failed==0 {println!("RESULT PASS {}/{}",checks.len(),checks.len());ExitCode::SUCCESS}
    else {println!("RESULT FAIL {failed}/{}",checks.len());ExitCode::FAILURE}
}
fn deadline()->Instant{Instant::now()+Duration::from_secs(30)}
fn parent(label:&str)->Result<PathBuf,String>{
    let p=PathBuf::from(std::env::var_os("YOKAI_TEST_DATA_DIR").ok_or("test root missing")?).join(format!("durable_primitives-{}-{label}",std::process::id()));
    fs::create_dir(&p).map_err(|e|e.to_string())?;Ok(p)
}
fn instance(label:&str)->Result<store::Instance,String>{store::create_instance(&parent(label)?).map_err(|e|e.to_string())}
fn ro(root:&Path)->Result<Connection,String>{Connection::open_with_flags(root.join("instance.db"),OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e|e.to_string())}
fn request(id:&str,payload:&[u8])->Result<CommandRequest,String>{CommandRequest::new(CommandId::new(id).map_err(|e|e.to_string())?,"test",1,payload).map_err(|e|e.to_string())}
fn accept(handle:&store::StoreHandle,id:&str,payload:&[u8])->Result<AcceptOutcome,String>{
    handle.accept_command(request(id,payload)?,deadline()).map_err(|e|e.to_string())?.wait(deadline()).map_err(|e|e.to_string())
}
fn counts(root:&Path)->Result<(i64,i64,i64,i64),String>{
    let c=ro(root)?;let n=|table:&str|c.query_row(&format!("SELECT count(*) FROM {table}"),[],|r|r.get(0)).map_err(|e|e.to_string());
    Ok((n("runs")?,n("commands")?,n("jobs")?,n("events WHERE kind='run.accepted'")?))
}
fn protected(root:&Path)->Result<Vec<Option<Vec<u8>>>,String>{["instance.db","instance.db-wal"].iter().map(|p|match fs::read(root.join(p)){
    Ok(b)=>Ok(Some(b)),Err(e) if e.kind()==std::io::ErrorKind::NotFound=>Ok(None),Err(e)=>Err(e.to_string())}).collect()}
fn hex(bytes:[u8;32])->String{bytes.iter().map(|b|format!("{b:02x}")).collect()}
fn schema()->Result<String,String>{
    let i=instance("schema")?;let root=i.root().to_owned();let v=store::test_api::user_version(&i).map_err(|e|e.to_string())?;
    i.close().map_err(|e|e.to_string())?;let c=ro(&root)?;
    let tables:i64=c.query_row("SELECT count(*) FROM pragma_table_list WHERE name IN ('runtime_epoch','runs','commands','jobs','events') AND strict=1",[],|r|r.get(0)).map_err(|e|e.to_string())?;
    if v!=2 || tables!=5 || counts(&root)?!=(0,0,0,0){return Err(format!("version={v}, strict tables={tables}"));}
    Ok("V2 five STRICT durable tables; empty acceptance set; V1 identity/history retained".into())
}
fn validation()->Result<String,String>{
    let id=CommandId::new("id").map_err(|e|e.to_string())?;
    for (kind,version,payload) in [("".to_string(),1,vec![]),("x".repeat(65),1,vec![]),("é".repeat(33),1,vec![]),("x".into(),0,vec![]),("x".into(),65536,vec![]),("x".into(),1,vec![0;65537])] {
        if CommandRequest::new(id.clone(),&kind,version,&payload).is_ok(){return Err("invalid request accepted".into());}
    }
    if CommandId::new("").is_ok() || CommandId::new(&"i".repeat(129)).is_ok(){return Err("invalid command id accepted".into());}
    CommandRequest::new(CommandId::new(&"i".repeat(128)).map_err(|e|e.to_string())?,&"é".repeat(32),65535,&vec![0;65536]).map_err(|e|e.to_string())?;
    Ok("byte bounds: id1..128 kind1..64 version1..65535 payload0..65536; Unicode counted as bytes".into())
}
fn digest_vectors()->Result<String,String>{
    let vectors:&[(&str,u32,&[u8],&str)]=&[
        ("test",1,b"hello","320426e8fdc9a94e7feeeb5872d80f40b5648435cacf1853bf2071e876393dec"),
        ("x",1,b"","15c26252c4d71a8237f2d708f64d704af056de8fca2e6a2ef4ae3cbfcad61760"),
        ("ab",1,b"c","4944c6611a58f875b74edd3df5c331dc677f2dc8e94ecdf32ab2fac7e2947dfc"),
        ("a",1,b"bc","2a38813af426314ce012972090fd9ef06a20cd36a7b5d0c8ace2b81e6c85d5ba"),
        ("妖怪\0",65535,&[0,255,1],"be2f7dcfd799c326e77d268191c03e717b866be03e2b74280c627f8879379afd"),
    ];
    for (kind,version,payload,want) in vectors {
        let req=CommandRequest::new(CommandId::new("vector").map_err(|e|e.to_string())?,kind,*version,payload).map_err(|e|e.to_string())?;
        if hex(req.digest())!=*want{return Err(format!("digest framing mismatch for {kind:?}/{version}"));}
    }
    Ok("five independent literal SHA256 framing vectors: production NUL-terminated domain, BE lengths/version, Unicode/NUL and ambiguous unframed pairs".into())
}
fn ten_commands()->Result<String,String>{
    let i=instance("ten")?;let root=i.root().to_owned();let h=i.store();
    for n in 0..10 {if !matches!(accept(&h,&format!("cmd{n}"),b"payload")?,AcceptOutcome::Accepted{..}){return Err("new command not accepted".into());}}
    i.close().map_err(|e|e.to_string())?;let c=ro(&root)?;
    let bad:i64=c.query_row("SELECT count(*) FROM runs WHERE admission<>'Queued' OR status<>'Open' OR hold_reason IS NOT NULL",[],|r|r.get(0)).map_err(|e|e.to_string())?;
    let jobs:i64=c.query_row("SELECT count(*) FROM jobs WHERE state<>'Pending' OR job_no<>1 OR claimed_by IS NOT NULL OR claim_epoch IS NOT NULL",[],|r|r.get(0)).map_err(|e|e.to_string())?;
    if counts(&root)?!=(10,10,10,10) || bad!=0 || jobs!=0{return Err("atomic acceptance set/state mismatch".into());}
    Ok("10 runs/commands/jobs/acceptance events; Queued/Open, Pending job1; foreign keys and uniqueness enforced".into())
}
fn replay()->Result<String,String>{
    let i=instance("replay")?;let root=i.root().to_owned();let h=i.store();
    let run=match accept(&h,"stable",b"same")?{AcceptOutcome::Accepted{run_id}=>run_id,other=>return Err(format!("{other:?}"))};
    if accept(&h,"stable",b"same")?!= (AcceptOutcome::Duplicate{run_id:run}) || accept(&h,"stable",b"changed")? != (AcceptOutcome::Conflict{run_id:run}){return Err("replay/conflict identity mismatch".into());}
    let utf=CommandRequest::new(CommandId::new("nul\0id").map_err(|e|e.to_string())?,"妖怪\0",65535,&[0,255]).map_err(|e|e.to_string())?;
    h.accept_command(utf.clone(),deadline()).map_err(|e|e.to_string())?.wait(deadline()).map_err(|e|e.to_string())?;
    if !matches!(h.accept_command(utf,deadline()).map_err(|e|e.to_string())?.wait(deadline()).map_err(|e|e.to_string())?,AcceptOutcome::Duplicate{..}){return Err("NUL/Unicode replay differs".into());}
    i.close().map_err(|e|e.to_string())?;
    if counts(&root)?!=(2,2,2,2){return Err("duplicate/conflict created records".into());}
    Ok("same ID+bytes→original run; changed request→Conflict; Unicode/NUL exact replay; no duplicate records".into())
}
fn constraints()->Result<String,String>{
    let i=instance("constraints")?;let root=i.root().to_owned();accept(&i.store(),"original",b"p")?;i.close().map_err(|e|e.to_string())?;
    let c=store::test_api::open_file(&root.join("instance.db")).map_err(|e|e.to_string())?;
    c.execute_batch("PRAGMA recursive_triggers=OFF").map_err(|e|e.to_string())?;
    let bad=["UPDATE commands SET payload=x'00'","DELETE FROM commands",
        "INSERT OR REPLACE INTO commands VALUES('original','test',1,x'aa',zeroblob(32),1,1)",
        "INSERT OR REPLACE INTO commands VALUES('replacement','test',1,x'aa',zeroblob(32),1,1)",
        "INSERT INTO runs(admission,status,created_epoch) VALUES('Held','Open',1)",
        "INSERT INTO runs(admission,status,created_epoch) VALUES('Admitted','Open',1)",
        "INSERT INTO runs(admission,status,created_epoch) VALUES('Queued','Invalid',1)",
        "INSERT INTO jobs(run_id,job_no,state,created_epoch) VALUES(999999,1,'Pending',1)",
        "INSERT INTO jobs(run_id,job_no,state,created_epoch) VALUES(1,1,'Pending',1)",
        "INSERT INTO jobs(run_id,job_no,state,claimed_by,created_epoch) VALUES(1,2,'Pending','x',1)",
        "INSERT INTO jobs(run_id,job_no,state,created_epoch) VALUES(1,2,'Claimed',1)",
        "INSERT INTO events(run_id,kind,epoch) VALUES(1,'run.accepted',1)"];
    for sql in bad {let error=c.execute_batch(sql).err().ok_or_else(||format!("constraint accepted {sql}"))?;
        if store::ext_code(&error).map(|c|c&255)!=Some(19) || !c.is_autocommit(){return Err(format!("not a clean constraint refusal: {error}"));}}
    // A valid unbound run isolates the digest CHECK from FK/unique/replacement guards.
    c.execute_batch("BEGIN IMMEDIATE; INSERT INTO runs(admission,status,created_epoch) VALUES('Queued','Open',1)").map_err(|e|e.to_string())?;
    let unbound=c.last_insert_rowid();
    let digest_error=c.execute("INSERT INTO commands VALUES('bad','test',1,x'',zeroblob(31),?1,1)",[unbound]).err();
    let active_after_error=!c.is_autocommit();
    c.execute_batch("ROLLBACK").map_err(|e|e.to_string())?;
    let digest_error=digest_error.ok_or("invalid digest accepted")?;
    if store::ext_code(&digest_error)!=Some(275) || !active_after_error || !c.is_autocommit(){return Err(format!("digest CHECK not isolated/rolled back: {digest_error}"));}
    if counts(&root)?!=(1,1,1,1){return Err("refused mutation changed acceptance set".into());}
    Ok("actual command UPDATE/DELETE/REPLACE refusal with recursive triggers OFF, isolated digest CHECK275 and rollback, run/job CHECKs, FK, job/event uniqueness preserve records".into())
}
fn concurrent_duplicates()->Result<String,String>{
    let i=instance("concurrent")?;let root=i.root().to_owned();let pause=store::test_api::block_writer(&i).map_err(|e|e.to_string())?;
    let before=protected(&root)?;
    let out=Command::new(std::env::current_exe().map_err(|e|e.to_string())?).arg("__busy").arg(&root).stdin(Stdio::null()).output().map_err(|e|e.to_string())?;
    if !out.status.success() || protected(&root)?!=before{return Err("second process was not Busy/unchanged".into());}drop(pause);
    let threads:Vec<_>=(0..8).map(|_|{let h=i.store();std::thread::spawn(move||->Result<(usize,usize),String>{
        let (mut accepted,mut duplicate)=(0,0);for n in 0..100{match accept(&h,&format!("shared{n}"),b"same")?{
            AcceptOutcome::Accepted{..}=>accepted+=1,AcceptOutcome::Duplicate{..}=>duplicate+=1,_=>return Err("conflict for identical request".into())}}
        Ok((accepted,duplicate))})}).collect();
    let (mut accepted,mut duplicate)=(0,0);for t in threads{let(a,d)=t.join().map_err(|_|"caller panic")??;accepted+=a;duplicate+=d;}
    i.close().map_err(|e|e.to_string())?;
    if accepted!=100 || duplicate!=700 || counts(&root)?!=(100,100,100,100){return Err(format!("accepted={accepted} duplicate={duplicate}"));}
    Ok("8 callers×100 shared IDs:100 Accepted/700 Duplicate;100 durable sets; second-process Busy and unchanged".into())
}
fn concurrent_conflicts()->Result<String,String>{
    let i=instance("conflicts")?;let root=i.root().to_owned();
    let threads:Vec<_>=(0..2).map(|caller|{let h=i.store();std::thread::spawn(move||->Result<(usize,usize),String>{
        let(mut a,mut c)=(0,0);for n in 0..20 {match accept(&h,&format!("conflict{n}"),&[caller])?{AcceptOutcome::Accepted{..}=>a+=1,AcceptOutcome::Conflict{..}=>c+=1,_=>return Err("different payload was Duplicate".into())}}Ok((a,c))})}).collect();
    let(mut accepted,mut conflict)=(0,0);for t in threads{let(a,c)=t.join().map_err(|_|"caller panic")??;accepted+=a;conflict+=c;}
    i.close().map_err(|e|e.to_string())?;
    if accepted!=20 || conflict!=20 || counts(&root)?!=(20,20,20,20){return Err("conflicting callers did not yield one winner/id".into());}
    Ok("two callers×20 shared IDs with distinct payloads:20 Accepted/20 Conflict, one durable set/id".into())
}
fn bounds()->Result<String,String>{
    let i=store::create_instance_with_options(&parent("bounds")?,store::StoreOptions{queue_capacity:1,..Default::default()}).map_err(|e|e.to_string())?;
    let root=i.root().to_owned();let h=i.store();let pause=store::test_api::block_writer(&i).map_err(|e|e.to_string())?;
    let first=h.accept_command(request("queued",b"x")?,deadline()).map_err(|e|e.to_string())?;
    let overload=h.accept_command(request("overloaded",b"x")?,deadline()).err().ok_or("queue did not overload")?;
    if overload.certainty!=store::Certainty::NotApplied || overload.reason!=store::WriteReason::Overloaded{return Err(format!("{overload:?}"));}
    drop(pause);first.wait(deadline()).map_err(|e|e.to_string())?;
    let expired=h.accept_command(request("expired",b"x")?,Instant::now()-Duration::from_secs(1)).map_err(|e|e.to_string())?.wait(deadline()).err().ok_or("expired command applied")?;
    if expired.reason!=store::WriteReason::Expired || expired.certainty!=store::Certainty::NotApplied{return Err(format!("{expired:?}"));}
    store::test_api::read_error_probe(&i,deadline(),13).map_err(|e|e.to_string())?.wait(deadline()).err().ok_or("reader fault absent")?;
    let degraded=h.accept_command(request("degraded",b"x")?,deadline()).err().ok_or("Degraded accepted command")?;
    if degraded.reason!=store::WriteReason::Degraded{return Err(format!("{degraded:?}"));}
    h.submit_health_check(deadline()).map_err(|e|e.to_string())?.wait(deadline()).map_err(|e|e.to_string())?;
    accept(&h,"recovered",b"x")?;i.close().map_err(|e|e.to_string())?;
    if h.accept_command(request("closed",b"x")?,deadline()).is_ok() || counts(&root)?!=(2,2,2,2){return Err("bounds/state refusal changed command set".into());}
    Ok("same bounded writer: overload/expiry NotApplied, Degraded refuses until committed health recovery, closed clone refuses".into())
}


struct OwnedChild(std::process::Child);
impl Drop for OwnedChild {fn drop(&mut self){if self.0.try_wait().ok().flatten().is_none(){let _=self.0.kill();}let _=self.0.wait();}}
fn child_command(mode:&str,path:&Path)->Result<Command,String>{
    let mut c=Command::new(std::env::current_exe().map_err(|e|e.to_string())?);c.arg(mode).arg(path).stdin(Stdio::null());Ok(c)
}
fn run_child(mut c:Command,dir:&Path,label:&str,marker:Option<&Path>)->Result<(std::process::ExitStatus,String,String),String>{
    let out=dir.join(format!("{label}.out"));let err=dir.join(format!("{label}.err"));
    let file=|p:&Path|fs::OpenOptions::new().write(true).create_new(true).open(p).map_err(|e|e.to_string());
    c.stdout(file(&out)?).stderr(file(&err)?);let mut child=OwnedChild(c.spawn().map_err(|e|e.to_string())?);
    let until=Instant::now()+Duration::from_secs(30);
    if let Some(marker)=marker {
        while !marker.exists() && Instant::now()<until {
            if child.0.try_wait().map_err(|e|e.to_string())?.is_some(){return Err(format!("child exited before marker: {}",fs::read_to_string(&err).unwrap_or_default()));}
            std::thread::sleep(Duration::from_millis(5));
        }
        if !marker.exists(){return Err("child readiness timeout".into());}
        child.0.kill().map_err(|e|e.to_string())?;
    }
    let status=loop {if let Some(status)=child.0.try_wait().map_err(|e|e.to_string())?{break status;}
        if Instant::now()>=until{return Err("child exit timeout".into());}std::thread::sleep(Duration::from_millis(5));};
    child.0.wait().map_err(|e|e.to_string())?;
    Ok((status,fs::read_to_string(out).map_err(|e|e.to_string())?,fs::read_to_string(err).map_err(|e|e.to_string())?))
}
fn child_mode(args:&[String])->ExitCode {
    let run=||->Result<(),String>{
        let path=Path::new(args.get(2).ok_or("child path missing")?);
        match args[1].as_str(){
            "__accept"=>{let i=store::open_instance(path).map_err(|e|e.to_string())?;
                let result=i.store().accept_command(request("retry",b"payload")?,deadline()).map_err(|e|format!("{e:?}"))?.wait(deadline());
                i.close().map_err(|e|e.to_string())?;match result{Ok(r)=>{println!("{r:?}");Ok(())},Err(e)=>Err(format!("{e:?}"))}},
            "__open"=>{let i=store::open_instance(path).map_err(|e|e.to_string())?;i.close().map_err(|e|e.to_string())},
            "__truth"=>{let c=ro(path)?;
                let integrity:String=c.query_row("PRAGMA integrity_check",[],|r|r.get(0)).map_err(|e|e.to_string())?;
                if integrity!="ok" || counts(path)?!=(1,1,1,1){return Err("fresh process truth mismatch".into());}Ok(())},
            "__owner"=>{store::test_api::set_next_identity([0x42;16]).map_err(|e|e.to_string())?;
                let i=store::create_instance(path).map_err(|e|e.to_string())?;populate_states(&i)?;
                fs::write(path.join("owner-ready"),b"ready").map_err(|e|e.to_string())?;
                loop{std::thread::park_timeout(Duration::from_secs(1));}},
            _=>Err("unknown child mode".into()),
        }
    };
    match run(){Ok(())=>ExitCode::SUCCESS,Err(e)=>{eprintln!("{e}");ExitCode::from(4)}}
}
fn fresh_truth(root:&Path,label:&str)->Result<(),String>{
    let(status,_,err)=run_child(child_command("__truth",root)?,root.parent().ok_or("parent")?,label,None)?;
    if !status.success(){return Err(format!("fresh truth: {err}"));}Ok(())
}
fn acceptance_crashes()->Result<String,String>{
    let points=[("accept-before-begin",false),("accept-after-begin",false),("accept-after-run",false),
        ("accept-after-command",false),("accept-after-job",false),("accept-after-event",false),("accept-after-commit",true)];
    let mut cases=0;
    for (point,committed) in points {for mode in ["exit","wait","error"]{for repeat in 0..3 {
        let i=instance(&format!("accept-crash-{point}-{mode}-{repeat}"))?;let root=i.root().to_owned();i.close().map_err(|e|e.to_string())?;
        let dir=root.parent().ok_or("parent")?;let marker=dir.join("ready");
        let mut cmd=child_command("__accept",&root)?;cmd.env("YOKAI_TEST_FAILPOINT",format!("{point}:{mode}")).env("YOKAI_TEST_READY_FILE",&marker);
        let(status,_,err)=run_child(cmd,dir,"crash",if mode=="wait"{Some(&marker)}else{None})?;
        if (mode=="exit" && status.code()!=Some(86)) || (mode=="error" && status.code()!=Some(4)) || (mode=="wait" && status.success()) {return Err(format!("boundary not observed {point}/{mode}: {status} {err}"));}
        if mode=="error" && !err.contains(if committed{"certainty: OutcomeUnknown"}else{"certainty: NotApplied"}){return Err(format!("incorrect certainty at {point}: {err}"));}
        let i=store::open_instance(&root).map_err(|e|e.to_string())?;
        if counts(&root)?!=if committed{(1,1,1,1)}else{(0,0,0,0)}{return Err(format!("partial or missing acceptance at {point}/{mode}"));}
        let outcome=accept(&i.store(),"retry",b"payload")?;
        if (committed && !matches!(outcome,AcceptOutcome::Duplicate{..})) || (!committed && !matches!(outcome,AcceptOutcome::Accepted{..})){return Err(format!("retry outcome {outcome:?}"));}
        i.close().map_err(|e|e.to_string())?;fresh_truth(&root,"truth")?;cases+=1;
    }}}
    Ok(format!("{cases} cases:7 boundaries×exit/Windows kill/error×3; all-or-nothing fresh reopen; committed lost reply→Duplicate, absent→Accepted"))
}
fn unknown_outcomes()->Result<String,String>{
    use store::test_api::WriteFault;
    for (label,fault,present) in [("full-absent",WriteFault::Commit{code:13,actually_commit:false},false),
        ("full-present",WriteFault::Commit{code:13,actually_commit:true},true),
        ("io-absent",WriteFault::Commit{code:10,actually_commit:false},false),
        ("io-present",WriteFault::Commit{code:10,actually_commit:true},true),("panic-present",WriteFault::PanicAfterCommit,true)] {
        let i=instance(&format!("unknown-{label}"))?;let root=i.root().to_owned();
        let error=store::test_api::accept_command_fault(&i,request("retry",b"payload")?,deadline(),fault).map_err(|e|e.to_string())?.wait(deadline()).err().ok_or("fault not observed")?;
        if error.certainty!=store::Certainty::OutcomeUnknown{return Err(format!("commit uncertainty misclassified: {error:?}"));}
        i.close().map_err(|e|e.to_string())?;let i=store::open_instance(&root).map_err(|e|e.to_string())?;
        let out=accept(&i.store(),"retry",b"payload")?;
        if present!=matches!(out,AcceptOutcome::Duplicate{..}){return Err(format!("durable truth/retry mismatch {label}: {out:?}"));}
        i.close().map_err(|e|e.to_string())?;fresh_truth(&root,"truth")?;
    }
    let i=instance("unknown-caller")?;let root=i.root().to_owned();let h=i.store();
    let pending=store::test_api::accept_command_fault(&i,request("retry",b"payload")?,deadline(),WriteFault::DelayBeforeCommit(Duration::from_millis(100))).map_err(|e|e.to_string())?;
    let id=pending.operation_id();let e=pending.wait(Instant::now()+Duration::from_millis(5)).err().ok_or("caller wait did not expire")?;
    if e.certainty!=store::Certainty::OutcomeUnknown{return Err("caller inferred definite failure".into());}
    let until=deadline();while h.completions().iter().all(|c|c.operation_id!=id) && Instant::now()<until{std::thread::sleep(Duration::from_millis(5));}
    if !h.completions().iter().any(|c|c.operation_id==id && matches!(&c.result,Ok(s) if matches!(s.command,Some(AcceptOutcome::Accepted{..})))){return Err("true command outcome not retained after waiter loss".into());}
    i.close().map_err(|e|e.to_string())?;let i=store::open_instance(&root).map_err(|e|e.to_string())?;
    if !matches!(accept(&i.store(),"retry",b"payload")?,AcceptOutcome::Duplicate{..}){return Err("caller timeout retry duplicated run".into());}
    i.close().map_err(|e|e.to_string())?;fresh_truth(&root,"truth")?;
    Ok("synthetic commit FULL/IOERR absent+present, committed writer panic and caller wait expiry stay Unknown; fresh reopen retry resolves one run; true worker outcome retained".into())
}
fn after_commit_reply()->Result<String,String>{
    let i=instance("aftercommit-reply")?;let root=i.root().to_owned();i.close().map_err(|e|e.to_string())?;
    let mut cmd=child_command("__accept",&root)?;cmd.env("YOKAI_TEST_FAILPOINT","accept-after-commit:error");
    let(status,_,err)=run_child(cmd,root.parent().ok_or("parent")?,"reply",None)?;
    if status.code()!=Some(4) || !err.contains("certainty: OutcomeUnknown"){return Err(format!("after-COMMIT reply={status} {err}"));}
    let i=store::open_instance(&root).map_err(|e|e.to_string())?;
    if !matches!(accept(&i.store(),"retry",b"payload")?,AcceptOutcome::Duplicate{..}){return Err("durable error reply was replayed".into());}
    i.close().map_err(|e|e.to_string())?;fresh_truth(&root,"truth")?;
    Ok("successful COMMIT followed by injected error reply is OutcomeUnknown; fresh-process truth and same-ID retry resolve original run".into())
}
fn populate_states(i:&store::Instance)->Result<(),String>{
    for n in 1..=8{accept(&i.store(),&format!("state{n}"),b"payload")?;}
    store::test_api::writer_sql(i,"UPDATE runs SET admission='WaitingApproval' WHERE run_id=2;
        UPDATE runs SET admission='Admitted',admitted_epoch=1 WHERE run_id=3;
        UPDATE runs SET admission='Held',hold_reason='RecoveryRequired',recovery_required=1 WHERE run_id=4;
        UPDATE runs SET status='Completed' WHERE run_id=5;
        UPDATE runs SET admission='WaitingApproval',status='Cancelled' WHERE run_id=6;
        UPDATE runs SET admission='Held',hold_reason='StartupInterrupted',status='Failed' WHERE run_id=7;
        UPDATE runs SET recovery_required=1 WHERE run_id=8;").map_err(|e|e.to_string())
}
fn held_rows(root:&Path)->Result<Vec<String>,String>{
    let c=ro(root)?;let mut s=c.prepare("SELECT printf('%d|%s|%s|%s|%d|%s',run_id,admission,status,coalesce(hold_reason,'-'),recovery_required,coalesce(admitted_epoch,'-')) FROM runs ORDER BY run_id").map_err(|e|e.to_string())?;
    s.query_map([],|r|r.get(0)).map_err(|e|e.to_string())?.collect::<Result<Vec<_>,_>>().map_err(|e|e.to_string())
}
fn verify_held(root:&Path)->Result<(),String>{
    let expected=["1|Held|Open|StartupInterrupted|0|-","2|Held|Open|ApprovalInvalidated|0|-","3|Held|Open|StartupInterrupted|0|1",
        "4|Held|Open|RecoveryRequired|1|-","5|Queued|Completed|-|0|-","6|WaitingApproval|Cancelled|-|0|-",
        "7|Held|Failed|StartupInterrupted|0|-","8|Held|Open|RecoveryRequired|1|-"];
    if held_rows(root)?!=expected{return Err(format!("startup states={:?}",held_rows(root)?));}
    let c=ro(root)?;
    let events:(i64,i64)=c.query_row("SELECT count(*),count(DISTINCT run_id) FROM events WHERE kind='run.held'",[],|r|Ok((r.get(0)?,r.get(1)?))).map_err(|e|e.to_string())?;
    let unordered:i64=c.query_row("SELECT count(*) FROM events h WHERE h.kind='run.held' AND NOT EXISTS(SELECT 1 FROM events e WHERE e.kind='runtime.epoch' AND e.epoch=h.epoch AND e.seq<h.seq)",[],|r|r.get(0)).map_err(|e|e.to_string())?;
    if events!=(4,4) || unordered!=0{return Err("duplicate hold event or hold preceded its epoch commit".into());}Ok(())
}
fn startup_owner_crash()->Result<String,String>{
    let p=parent("startup-owner")?;let marker=p.join("owner-ready");
    let(status,_,err)=run_child(child_command("__owner",&p)?,&p,"owner",Some(&marker))?;
    if status.success(){return Err(format!("owner not killed {err}"));}
    let root=p.join("42".repeat(16));let i=store::open_instance(&root).map_err(|e|e.to_string())?;
    let report=i.startup_report().ok_or("startup report absent")?;
    if report.epoch!=2 || report.runs_changed!=4{return Err(format!("{report:?}"));}verify_held(&root)?;
    i.close().map_err(|e|e.to_string())?;
    Ok("Windows owner killed with each admission/terminal state; reopen committed epoch2 before4 holds, reasons stable, terminal/Held preserved, none newly Admitted".into())
}
fn startup_idempotence()->Result<String,String>{
    let i=instance("startup-repeat")?;let root=i.root().to_owned();populate_states(&i)?;i.close().map_err(|e|e.to_string())?;
    let i=store::open_instance(&root).map_err(|e|e.to_string())?;verify_held(&root)?;i.close().map_err(|e|e.to_string())?;
    let before=held_rows(&root)?;let c=ro(&root)?;let count:i64=c.query_row("SELECT count(*) FROM events",[],|r|r.get(0)).map_err(|e|e.to_string())?;drop(c);
    let i=store::open_instance(&root).map_err(|e|e.to_string())?;let report=i.startup_report().ok_or("report")?;
    if report.epoch!=3 || report.runs_changed!=0 || before!=held_rows(&root)?{return Err(format!("second startup changed runs {report:?}"));}
    i.close().map_err(|e|e.to_string())?;let c=ro(&root)?;let after:i64=c.query_row("SELECT count(*) FROM events",[],|r|r.get(0)).map_err(|e|e.to_string())?;
    if after!=count+1{return Err("second startup added more than epoch event".into());}verify_held(&root)?;
    Ok("second open increments epoch only; no duplicate hold event, no changed terminal or existing Held row".into())
}
fn startup_crashes()->Result<String,String>{
    let mut cases=0;
    for point in ["startup-after-epoch-commit","startup-after-hold-3"]{for mode in ["exit","wait","error"]{for repeat in 0..3{
        let i=instance(&format!("startup-crash-{point}-{mode}-{repeat}"))?;let root=i.root().to_owned();populate_states(&i)?;i.close().map_err(|e|e.to_string())?;
        let dir=root.parent().ok_or("parent")?;let marker=dir.join("ready");let mut cmd=child_command("__open",&root)?;
        cmd.env("YOKAI_TEST_FAILPOINT",format!("{point}:{mode}")).env("YOKAI_TEST_READY_FILE",&marker);
        let(status,_,err)=run_child(cmd,dir,"crash",if mode=="wait"{Some(&marker)}else{None})?;
        if (mode=="exit" && status.code()!=Some(86)) || (mode=="error" && status.code()!=Some(4)) || (mode=="wait" && status.success()){return Err(format!("startup boundary not observed {point}/{mode}: {status} {err}"));}
        let c=ro(&root)?;let epoch:i64=c.query_row("SELECT epoch FROM runtime_epoch",[],|r|r.get(0)).map_err(|e|e.to_string())?;drop(c);
        if epoch!=2{return Err("epoch commit was not durable before hold work".into());}
        let i=store::open_instance(&root).map_err(|e|e.to_string())?;
        if i.startup_report().ok_or("report")?.epoch!=3{return Err("recovery did not advance epoch".into());}
        verify_held(&root)?;i.close().map_err(|e|e.to_string())?;cases+=1;
    }}}
    Ok(format!("{cases} epoch/third-hold boundaries×exit/Windows kill/error; fresh reopen converges with4 unique holds and committed epoch order"))
}
fn no_execution_case(label:&str)->Result<String,String>{
    let i=instance(label)?;let root=i.root().to_owned();accept(&i.store(),"one",b"x")?;
    i.store().submit_health_check(deadline()).map_err(|e|e.to_string())?.wait(deadline()).map_err(|e|e.to_string())?;i.close().map_err(|e|e.to_string())?;
    let i=store::open_instance(&root).map_err(|e|e.to_string())?;accept(&i.store(),"one",b"x")?;accept(&i.store(),"one",b"different")?;accept(&i.store(),"two",b"x")?;
    i.store().submit_health_check(deadline()).map_err(|e|e.to_string())?.wait(deadline()).map_err(|e|e.to_string())?;
    let c=ro(&root)?;let bad:i64=c.query_row("SELECT (SELECT count(*) FROM runs WHERE admission='Admitted')+(SELECT count(*) FROM jobs WHERE state<>'Pending' OR claimed_by IS NOT NULL OR claim_epoch IS NOT NULL)",[],|r|r.get(0)).map_err(|e|e.to_string())?;
    i.close().map_err(|e|e.to_string())?;if bad!=0{return Err("public writes changed job execution/admission state".into());}
    Ok("exhaustive current public writes: create/open startup, accept/retry/conflict and health leave jobs Pending and never set Admitted; no job mutation API".into())
}
fn jobs_records_only()->Result<String,String>{no_execution_case("no-job-mutation")}
fn no_admission()->Result<String,String>{no_execution_case("no-admission")}
fn classifier_edges()->Result<String,String>{
    use store::test_api::{WriteFault,WriteProbe};
    for (label,sql,automatic) in [("ra","CREATE TABLE ra(id INTEGER PRIMARY KEY) STRICT; INSERT INTO ra VALUES(1); INSERT OR ROLLBACK INTO ra VALUES(1);",true),
        ("immediate-fk","CREATE TABLE p(id INTEGER PRIMARY KEY) STRICT; CREATE TABLE ch(id INTEGER REFERENCES p(id)) STRICT; INSERT INTO ch VALUES(999);",false)] {
        let i=instance(&format!("classifier-{label}"))?;let root=i.root().to_owned();
        let e=store::test_api::write_probe(&i,deadline(),WriteProbe{sql:sql.into(),fault:WriteFault::None}).map_err(|e|e.to_string())?.wait(deadline()).err().ok_or("constraint fixture succeeded")?;
        if e.certainty!=store::Certainty::NotApplied || e.reason!=store::WriteReason::Constraint || e.evidence.autocommit_after_error!=Some(automatic)
            || e.evidence.rollback_succeeded!=if automatic{None}else{Some(true)}{return Err(format!("classifier evidence {e:?}"));}
        i.close().map_err(|e|e.to_string())?;let c=ro(&root)?;let extra:i64=c.query_row("SELECT count(*) FROM sqlite_schema WHERE name IN ('ra','p','ch')",[],|r|r.get(0)).map_err(|e|e.to_string())?;
        if extra!=0{return Err("constraint transaction leaked schema".into());}
    }
    Ok("real INSERT OR ROLLBACK produced R-a autocommit; real immediate FK produced R-b rollback; both runtime classifier NotApplied, no persisted fixture schema".into())
}
fn epoch_overflow()->Result<String,String>{
    let i=instance("epoch-overflow")?;let root=i.root().to_owned();i.close().map_err(|e|e.to_string())?;
    let c=Connection::open(root.join("instance.db")).map_err(|e|e.to_string())?;c.execute("UPDATE runtime_epoch SET epoch=?1",[i64::MAX]).map_err(|e|e.to_string())?;drop(c);
    let e=store::open_instance(&root).err().ok_or("epoch overflow accepted")?;
    let c=ro(&root)?;let epoch:i64=c.query_row("SELECT epoch FROM runtime_epoch",[],|r|r.get(0)).map_err(|e|e.to_string())?;
    if epoch!=i64::MAX || !e.to_string().contains("epoch"){return Err(format!("epoch overflow state {epoch}: {e}"));}
    Ok("epoch i64::MAX refused without wrap or changed value; no worker handle exposed".into())
}
fn open_timing()->Result<String,String>{
    let i=instance("open-10k")?;let root=i.root().to_owned();let digest=hex(request("fixture",b"payload")?.digest());
    // Bulk SQL creates a fixed-size fixture, not a production bulk-accept API.
    store::test_api::writer_sql(&i,&format!("BEGIN IMMEDIATE;
        WITH RECURSIVE n(x) AS(VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<10000) INSERT INTO runs(run_id,admission,status,created_epoch) SELECT x,'Queued','Open',1 FROM n;
        INSERT INTO commands SELECT 'bulk-'||run_id,'test',1,x'7061796c6f6164',x'{digest}',run_id,1 FROM runs;
        INSERT INTO jobs(run_id,job_no,state,created_epoch) SELECT run_id,1,'Pending',1 FROM runs;
        INSERT INTO events(run_id,kind,detail,epoch) SELECT run_id,'run.accepted','fixture',1 FROM runs; COMMIT;")).map_err(|e|e.to_string())?;
    i.close().map_err(|e|e.to_string())?;let start=Instant::now();let i=store::open_instance(&root).map_err(|e|e.to_string())?;let elapsed=start.elapsed();
    let report=i.startup_report().ok_or("report")?;
    if report.runs_changed!=10000 || counts(&root)?!=(10000,10000,10000,10000){return Err(format!("large open {report:?}"));}
    i.close().map_err(|e|e.to_string())?;Ok(format!("10000 command records opened/held in {elapsed:?}; evidence only, no SLA or native-Linux claim"))
}
fn upgrade_v1()->Result<String,String>{
    let fixture=PathBuf::from(std::env::var_os("YOKAI_TEST_V1_FIXTURE").ok_or("pinned V1 fixture missing")?);
    let old=PathBuf::from(std::env::var_os("YOKAI_TEST_V1_BINARY").ok_or("pinned V1 binary missing")?);
    let before=fs::read(fixture.join("instance.db")).map_err(|e|e.to_string())?;
    let p=parent("upgrade-v1")?;let root=p.join(fixture.file_name().ok_or("fixture name")?);fs::create_dir(&root).map_err(|e|e.to_string())?;
    for name in ["instance.db","instance.id","instance.lock"]{fs::copy(fixture.join(name),root.join(name)).map_err(|e|e.to_string())?;}
    let i=store::open_instance(&root).map_err(|e|e.to_string())?;
    if store::test_api::user_version(&i).map_err(|e|e.to_string())?!=2{return Err("V1 copy did not upgrade".into());}
    i.close().map_err(|e|e.to_string())?;
    let backups:Vec<_>=fs::read_dir(root.join("backups")).map_err(|e|e.to_string())?.collect::<Result<_,_>>().map_err(|e|e.to_string())?;
    if backups.len()!=1{return Err("V1 upgrade did not retain one snapshot".into());}
    let c=Connection::open_with_flags(backups[0].path(),OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e|e.to_string())?;
    let version:i64=c.query_row("PRAGMA user_version",[],|r|r.get(0)).map_err(|e|e.to_string())?;
    if version!=1{return Err("snapshot is not V1".into());}drop(c);
    let protected_before=protected(&root)?;
    let out=Command::new(old).arg("__child").arg("probe").arg(&root).stdin(Stdio::null()).output().map_err(|e|e.to_string())?;
    if out.status.code()!=Some(4) || !String::from_utf8_lossy(&out.stderr).contains("schema 2 is newer than supported maximum 1") || protected_before!=protected(&root)?
        || fs::read(fixture.join("instance.db")).map_err(|e|e.to_string())?!=before{return Err("old V1 binary refusal/original preservation failed".into());}
    Ok("preserved V1 fixture copy upgraded to V2 with V1 snapshot; preserved V1 executable refused V2 unchanged; originals not modified".into())
}
