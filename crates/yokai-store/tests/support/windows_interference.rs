// A separate operator profile: ordinary service liveness cannot pass this case.
use super::*;
use std::sync::{Arc,atomic::AtomicBool,mpsc};
use std::time::{SystemTime,UNIX_EPOCH};

fn utc_us()->Result<u128,String>{SystemTime::now().duration_since(UNIX_EPOCH).map(|d|d.as_micros()).map_err(|e|e.to_string())}
fn retain(p:&Path,bytes:&[u8])->Result<(),String>{keep_junction_record(p,bytes)}
fn final_inventory(root:&Path)->Result<String,String>{
    use std::os::windows::fs::MetadataExt;
    fn walk(root:&Path,p:&Path,rows:&mut BTreeMap<String,String>)->Result<(),String>{
        for entry in fs::read_dir(p).map_err(|e|e.to_string())?{
            let p=entry.map_err(|e|e.to_string())?.path();
            let name=p.strip_prefix(root).map_err(|e|e.to_string())?.to_str().ok_or("B4 inventory non-UTF8 path")?.replace('\\',"/");
            if !name.split('/').all(|s|!s.is_empty() && s!="." && s!=".." && s.bytes().all(|b|b.is_ascii_alphanumeric() || b"_.-".contains(&b))){return Err("B4 unsafe inventory name".into());}
            let meta=fs::symlink_metadata(&p).map_err(|e|e.to_string())?;
            if meta.file_attributes()&0x400!=0{return Err("B4 inventory reparse refused".into());}
            if meta.is_dir(){rows.insert(name.clone(),format!("D\t{name}\n"));walk(root,&p,rows)?;}
            else if meta.is_file(){
                let before=host_api::id(&p)?;let bytes=fs::read(&p).map_err(|e|e.to_string())?;let after=host_api::id(&p)?;
                if before!=after || after.2!=bytes.len() as u64{return Err("B4 inventory changed during read".into());}
                rows.insert(name.clone(),format!("F\t{name}\t{}\t{}\t{}\t{}\n",after.2,store::test_api::sha256_chunks_for_test(&[&bytes]),after.0,after.1));
            }else{return Err("B4 inventory unknown entry".into());}
        }Ok(())
    }
    host_budget(root)?;let mut rows=BTreeMap::new();walk(root,root,&mut rows)?;Ok(rows.into_values().collect())
}
fn control()->Result<(PathBuf,String),String>{
    let p=host_env("YOKAI_B4_CONTROL_ROOT")?;
    let nonce=std::env::var("YOKAI_B4_NONCE").map_err(|e|e.to_string())?;
    if nonce.len()<16 || nonce.len()>64 || !nonce.bytes().all(|b|b.is_ascii_alphanumeric()){return Err("invalid B4 nonce".into());}
    Ok((p,nonce))
}
fn ready(p:&Path,nonce:&str,phase:&str,root:&Path)->Result<(),String>{
    let now=utc_us()?;
    retain(&p.join(format!("ready-{phase}")),format!("{nonce}\n{phase}\n{}\n{now}\n",root.display()).as_bytes())?;
    println!("INFO B4 phase_begin phase={phase} utc_us={now} root={}",root.display());Ok(())
}
fn phase<F:FnMut()->Result<(),String>>(p:&Path,nonce:&str,name:&str,root:&Path,minimum:u64,mut check:F)->Result<(),String>{
    ready(p,nonce,name,root)?;let start=Instant::now();let mut probes=0;
    loop{
        if p.join("halt").exists(){return Err("external B4 controller stopped the attempt".into());}
        if start.elapsed()>Duration::from_secs(120){return Err(format!("B4 {name} external observations timed out"));}
        check()?;probes+=1;
        if start.elapsed()>Duration::from_secs(120){return Err(format!("B4 {name} check exceeded held interval limit"));}
        let release=p.join(format!("release-{name}"));
        if release.exists(){
            if fs::read(&release).map_err(|e|e.to_string())?!=format!("{nonce}\n{name}\n").as_bytes(){return Err("stale/wrong B4 release".into());}
            if start.elapsed()>=Duration::from_secs(minimum){break;}
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let now=utc_us()?;
    retain(&p.join(format!("done-{name}")),format!("{nonce}\n{name}\n{now}\n").as_bytes())?;
    println!("INFO B4 phase_end phase={name} utc_us={now} probes={probes} elapsed_ms={}",start.elapsed().as_millis());Ok(())
}
fn busy_child(root:&Path)->Result<(),String>{
    let(mut child,logs)=launch(command("__b4_busy",root)?,"interference-busy")?;
    if !wait(&mut child,Duration::from_secs(10))?.success(){return Err(format!("B4 contender failed: {}",logs.display()));}
    let out=fs::read_to_string(logs.join("child.out")).map_err(|e|e.to_string())?;
    if !out.lines().any(|s|s=="INFO B4 child Busy sqlite_events=[]"){return Err("B4 contender observation absent".into());}Ok(())
}
fn same_lock(root:&Path,id:(u32,u64,u64))->Result<(),String>{
    if host_api::id(&root.join("instance.lock"))?!=id{return Err("B4 persistent lock identity changed".into());}Ok(())
}
struct Release(Arc<AtomicBool>);
impl Drop for Release{fn drop(&mut self){self.0.store(true,Ordering::Release);}}
fn held_command(i:&store::Instance,id:&str)->Result<(Release,store::PendingCommand),String>{
    let(tx,rx)=mpsc::sync_channel(1);let release=Arc::new(AtomicBool::new(false));
    let pending=store::test_api::accept_command_fault(i,request(id,b"B4 synthetic")?,Instant::now()+Duration::from_secs(180),
        store::test_api::WriteFault::HoldBeforeCommit{ready:tx,released:release.clone()}).map_err(|e|e.to_string())?;
    let guard=Release(release);rx.recv_timeout(Duration::from_secs(10)).map_err(|e|e.to_string())?;Ok((guard,pending))
}
fn verify(root:&Path,expected:&[String])->Result<(),String>{
    let c=ro(root)?;
    let observed={let mut s=c.prepare("SELECT command_id,kind,op_version,payload FROM commands ORDER BY command_id").map_err(|e|e.to_string())?;
        let rows=s.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,Vec<u8>>(3)?))).map_err(|e|e.to_string())?.collect::<Result<Vec<_>,_>>().map_err(|e|e.to_string())?;rows};
    let mut ids=expected.to_vec();ids.sort();
    let want:Vec<_>=ids.into_iter().map(|id|(id,"test".to_string(),1,b"B4 synthetic".to_vec())).collect();
    if observed!=want{return Err("B4 durable command identity/payload set differs".into());}
    for sql in ["SELECT count(*) FROM runs","SELECT count(*) FROM jobs","SELECT count(*) FROM events WHERE kind='run.accepted'"]{
        let n:i64=c.query_row(sql,[],|r|r.get(0)).map_err(|e|e.to_string())?;
        if n!=expected.len() as i64{return Err("B4 durable dependent record count differs".into());}
    }
    let check:String=c.query_row("PRAGMA integrity_check",[],|r|r.get(0)).map_err(|e|e.to_string())?;
    let fk:i64=c.query_row("SELECT count(*) FROM pragma_foreign_key_check",[],|r|r.get(0)).map_err(|e|e.to_string())?;
    c.close().map_err(|(_,e)|e.to_string())?;
    if check!="ok" || fk!=0{return Err("B4 integrity/foreign-key check failed".into());}Ok(())
}
fn reopen(root:&Path,lock:(u32,u64,u64),expected_epoch:i64)->Result<store::Instance,String>{
    same_lock(root,lock)?;
    let i=store::open_instance(root).map_err(|e|e.to_string())?;
    if !i.startup_report().is_some_and(|r|r.epoch==expected_epoch){return Err("B4 reopen epoch transition differs".into());}
    same_lock(root,lock)?;Ok(i)
}
fn closing(p:&Path,nonce:&str,root:&Path,lock:(u32,u64,u64),name:&str,hook:&str,expected:&[String])->Result<(),String>{
    let epoch=host_epoch(root)?;let logs=parent("interference-close")?;let marker=logs.join("ready");
    let mut c=command("__b4_close",root)?;
    c.env("YOKAI_TEST_FAILPOINT",format!("{hook}:pause")).env("YOKAI_TEST_READY_FILE",&marker)
        .stdin(Stdio::piped()).stdout(Stdio::from(File::create_new(logs.join("child.out")).map_err(|e|e.to_string())?))
        .stderr(Stdio::from(File::create_new(logs.join("child.err")).map_err(|e|e.to_string())?));
    let mut child=OwnedChild{child:c.spawn().map_err(|e|e.to_string())?};
    let mut input=child.child.stdin.take().ok_or("B4 close control pipe absent")?;
    let until=Instant::now()+Duration::from_secs(15);
    while !marker.exists(){
        if child.child.try_wait().map_err(|e|e.to_string())?.is_some() || Instant::now()>until{return Err("B4 close pause not observed".into());}
        std::thread::sleep(Duration::from_millis(10));
    }
    if fs::read(&marker).map_err(|e|e.to_string())?!=hook.as_bytes(){return Err("wrong B4 close pause marker".into());}
    let stable=if hook=="close-after-workers-join"{Some(protected(root)?)}else{None};let identity=fs::read(root.join("instance.id")).map_err(|e|e.to_string())?;
    let mut first=true;
    phase(p,nonce,name,root,10,||{
        if child.child.try_wait().map_err(|e|e.to_string())?.is_some(){return Err("B4 closing owner exited while pause held".into());}
        junction_busy(root)?;same_lock(root,lock)?;
        if first{busy_child(root)?;first=false;}
        if fs::read(root.join("instance.id")).map_err(|e|e.to_string())?!=identity{return Err("B4 closing identity changed".into());}
        if let Some(before)=&stable{if &protected(root)?!=before{return Err("B4 post-join refusal bytes changed".into());}}Ok(())
    })?;
    input.write_all(b"continue\n").and_then(|_|input.flush()).map_err(|e|e.to_string())?;drop(input);
    if !wait(&mut child,Duration::from_secs(15))?.success(){return Err(format!("B4 close failed: {}",logs.display()));}
    let output=fs::read_to_string(logs.join("child.out")).map_err(|e|e.to_string())?;
    if !output.lines().any(|s|s=="INFO B4 child close_events=[\"connection-closed\", \"lock-released\"]"){return Err("B4 close/release ordering absent".into());}
    if host_epoch(root)?!=epoch+1{return Err("B4 closing child epoch differs".into());}
    reopen(root,lock,epoch+2)?.close().map_err(|e|e.to_string())?;verify(root,expected)?;
    println!("INFO B4 close_verified phase={name} child_epoch={} reopen_epoch={} lock_id={lock:?}",epoch+1,epoch+2);Ok(())
}
pub(super) fn child(mode:&str,root:&Path,args:&[String])->Result<(),String>{
    match mode{
        "__b4_busy"=>{junction_busy(root)?;println!("INFO B4 child Busy sqlite_events=[]");},
        "__b4_close"=>{
            let i=store::open_instance(root).map_err(|e|e.to_string())?;store::test_api::clear();i.close().map_err(|e|e.to_string())?;
            let events=store::test_api::connection_events();if events!=["connection-closed","lock-released"]{return Err("B4 close/release events differ".into());}
            println!("INFO B4 child close_events={events:?}");
        },
        "__b4_crash"=>{
            let ready_path=Path::new(args.get(3).ok_or("B4 crash ready path absent")?);
            let i=store::open_instance(root).map_err(|e|e.to_string())?;
            for n in 0..8{if !matches!(accept(&i.store(),&format!("b4-ack-{n}"),b"B4 synthetic")?,AcceptOutcome::Accepted{..}){return Err("B4 child command not newly accepted".into());}}
            let(_release,_pending)=held_command(&i,"b4-uncommitted")?;retain(ready_path,b"eight-acknowledged-one-uncommitted")?;
            println!("INFO B4 child acknowledged=8 uncommitted=1 epoch={}",i.startup_report().ok_or("B4 child startup report absent")?.epoch);
            loop{std::thread::park_timeout(Duration::from_secs(1));}
        },
        "__b4_recover"=>{
            let epoch=args.get(3).ok_or("B4 recovery epoch absent")?.parse::<i64>().map_err(|e|e.to_string())?;
            let volume=args.get(4).ok_or("B4 lock volume absent")?.parse::<u32>().map_err(|e|e.to_string())?;
            let index=args.get(5).ok_or("B4 lock index absent")?.parse::<u64>().map_err(|e|e.to_string())?;
            let size=args.get(6).ok_or("B4 lock size absent")?.parse::<u64>().map_err(|e|e.to_string())?;
            let mut expected:Vec<_>=(0..16).map(|n|format!("b4-warm-{n}")).collect();expected.push("b4-held-owned".into());
            expected.extend((0..8).map(|n|format!("b4-ack-{n}")));
            let i=reopen(root,(volume,index,size),epoch)?;verify(root,&expected)?;
            i.store().submit_health_check(deadline()).map_err(|e|e.to_string())?.wait(deadline()).map_err(|e|e.to_string())?;
            if !matches!(accept(&i.store(),"b4-after-recovery",b"B4 synthetic")?,AcceptOutcome::Accepted{..}){return Err("B4 post-recovery command not newly accepted".into());}
            expected.push("b4-after-recovery".into());i.close().map_err(|e|e.to_string())?;verify(root,&expected)?;
            println!("INFO B4 child fresh_recovery commands=26 uncommitted_absent=true integrity=ok expected_epoch={epoch} observed_epoch={}",host_epoch(root)?);
        },
        _=>return Err("unknown B4 child mode".into())
    }Ok(())
}
pub(super) fn run()->Result<String,String>{
    let(p,nonce)=control()?;let fixture=host_env("YOKAI_B4_FIXTURE_ROOT")?;host_budget(&fixture)?;
    let create=fixture.join("storage");fs::create_dir(&create).map_err(|e|e.to_string())?;
    let i=store::create_instance(&create).map_err(|e|e.to_string())?;let root=i.root().to_owned();i.close().map_err(|e|e.to_string())?;
    let lock=host_api::id(&root.join("instance.lock"))?;let identity=fs::read(root.join("instance.id")).map_err(|e|e.to_string())?;
    let i=reopen(&root,lock,host_epoch(&root)?+1)?;let mut expected=Vec::new();
    for n in 0..16{let id=format!("b4-warm-{n}");if !matches!(accept(&i.store(),&id,b"B4 synthetic")?,AcceptOutcome::Accepted{..}){return Err("B4 warm command not newly accepted".into());}expected.push(id);}
    let(release,pending)=held_command(&i,"b4-held-owned")?;let before=protected(&root)?;let mut first=true;
    phase(&p,&nonce,"owned-write",&root,15,||{junction_busy(&root)?;same_lock(&root,lock)?;if first{busy_child(&root)?;first=false;}
        if protected(&root)?!=before{return Err("B4 held-write refusal changed protected bytes".into());}Ok(())})?;
    drop(release);if !matches!(pending.wait(deadline()).map_err(|e|e.to_string())?,AcceptOutcome::Accepted{..}){return Err("B4 held command did not commit".into());}
    expected.push("b4-held-owned".into());i.close().map_err(|e|e.to_string())?;verify(&root,&expected)?;
    closing(&p,&nonce,&root,lock,"closing-before-join","close-before-workers-join",&expected)?;
    closing(&p,&nonce,&root,lock,"closing-after-join","close-after-workers-join",&expected)?;
    let epoch=host_epoch(&root)?;let crash_ready=p.join("crash-ready");let mut c=command("__b4_crash",&root)?;c.arg(&crash_ready);
    let(mut child,logs)=launch(c,"interference-crash")?;let until=Instant::now()+Duration::from_secs(20);
    while !crash_ready.exists(){if child.child.try_wait().map_err(|e|e.to_string())?.is_some() || Instant::now()>until{return Err(format!("B4 crash writer not ready: {}",logs.display()));}std::thread::sleep(Duration::from_millis(10));}
    if fs::read(&crash_ready).map_err(|e|e.to_string())?!=b"eight-acknowledged-one-uncommitted"{return Err("B4 crash witness differs".into());}
    let held=protected(&root)?;let mut first=true;
    phase(&p,&nonce,"process-recovery",&root,15,||{junction_busy(&root)?;same_lock(&root,lock)?;if first{busy_child(&root)?;first=false;}
        if protected(&root)?!=held{return Err("B4 crash-held transaction bytes changed".into());}Ok(())})?;
    child.child.kill().map_err(|e|e.to_string())?;if wait(&mut child,Duration::from_secs(10))?.success(){return Err("B4 killed owner unexpectedly returned success".into());}
    expected.extend((0..8).map(|n|format!("b4-ack-{n}")));
    let mut c=command("__b4_recover",&root)?;c.arg((epoch+2).to_string()).arg(lock.0.to_string()).arg(lock.1.to_string()).arg(lock.2.to_string());
    let(mut recovered,recovery_logs)=launch(c,"interference-fresh-recovery")?;
    if !wait(&mut recovered,Duration::from_secs(20))?.success(){return Err(format!("B4 fresh recovery failed: {}",recovery_logs.display()));}
    let recovered_out=fs::read_to_string(recovery_logs.join("child.out")).map_err(|e|e.to_string())?;
    if !recovered_out.lines().any(|s|s==format!("INFO B4 child fresh_recovery commands=26 uncommitted_absent=true integrity=ok expected_epoch={} observed_epoch={}",epoch+2,epoch+2)){return Err("B4 fresh-process recovery witness absent".into());}
    expected.push("b4-after-recovery".into());verify(&root,&expected)?;same_lock(&root,lock)?;
    let final_epoch=host_epoch(&root)?;
    if final_epoch!=epoch+2{return Err("B4 persisted fresh recovery epoch differs".into());}
    if fs::read(root.join("instance.id")).map_err(|e|e.to_string())?!=identity{return Err("B4 final identity bytes changed".into());}
    host_budget(&fixture)?;let closed=final_inventory(&fixture)?;
    if final_inventory(&fixture)?!=closed{return Err("B4 closed fixture drift".into());}
    retain(&p.join("final-fixture.tsv"),closed.as_bytes())?;
    println!("INFO B4 final_fixture_inventory sha256={}",store::test_api::sha256_chunks_for_test(&[closed.as_bytes()]));
    println!("INFO B4 storage_verified commands=26 uncommitted_absent=true integrity=ok foreign_keys=0 persistent_lock_id={lock:?} final_epoch={final_epoch}");
    Ok("four synchronized ownership/held-write/closing/kill-recovery phases; parent+fresh-child Busy before SQLite, ordered close, exact epochs/lock and 26 durable commands verified; external scanner/index overlap requires the separate pinned wrapper evidence gate; not full Windows/durability acceptance".into())
}
