#![cfg(windows)]

use std::error::Error;
use std::io;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};
use yokai_ipc::{
    Binding, CONTROL_MAX, Endpoint, Enrollment, Frame, Hello, Ready, Remote, Reply, Request,
    RESPONSE_MAX, encode, nonce, read_control, read_frame, write_frame,
};
use yokai_platform::local_ipc::{Client, ImagePin, IoError, Listener, PipeName, ProcessIdentity};

type Result<T = ()> = std::result::Result<T, Box<dyn Error>>;
const CONVERSATION: &str = "00000000-0000-4000-8000-000000000001";
const MESSAGE: &str = "00000000-0000-4000-8000-000000000002";

fn end(seconds: u64) -> Instant {
    Instant::now() + Duration::from_secs(seconds)
}

fn require(ok: bool, detail: &'static str) -> Result {
    if ok { Ok(()) } else { Err(io::Error::other(detail).into()) }
}

fn disconnected(error: &IoError) -> bool {
    matches!(error, IoError::System(e)
        if e.kind() == io::ErrorKind::UnexpectedEof || matches!(e.raw_os_error(), Some(109 | 232 | 233)))
}

fn connect(endpoint: &Endpoint, config: &Enrollment) -> std::result::Result<Remote, IoError> {
    let deadline = end(2);
    loop {
        match Remote::connect(endpoint, config) {
            Err(IoError::System(e)) if e.raw_os_error() == Some(231) && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(20));
            }
            result => return result,
        }
    }
}

fn send(client: &mut Remote, request: Request) -> Result<Reply> {
    let id = client.send(request)?;
    let frame = client.receive(end(3))?;
    require(frame.request == id, "response correlation")?;
    Ok(frame.body)
}

fn create(client: &mut Remote) -> Result {
    let request = || Request::Create { command: "create-1".into(), conversation: CONVERSATION.into() };
    require(matches!(send(client, request())?, Reply::Decision { code, .. } if code == "created"), "conversation creation")?;
    require(matches!(send(client, request())?, Reply::Decision { code, .. } if code == "duplicate"), "duplicate creation")?;
    Ok(())
}

fn submission(text: &str) -> Request {
    Request::Submit { command: "submit-1".into(), conversation: CONVERSATION.into(), message: MESSAGE.into(), text: text.into() }
}

fn replay(instance: &str, after: i64, limit: usize) -> Request {
    Request::Replay { instance: instance.into(), conversation: CONVERSATION.into(), after, limit }
}

fn accepted(reply: Reply) -> Result<i64> {
    if let Reply::Decision { code, run: Some(run), .. } = reply
        && code == "accepted"
    {
        return Ok(run);
    }
    Err("expected a confirmed admission".into())
}

fn check_committed(client: &mut Remote, endpoint: &Endpoint, text: &str, run: i64) -> Result {
    let request = replay(&endpoint.epoch.instance, 0, 256);
    let Reply::Replay { events, has_more: false, after } = send(client, request)? else {
        return Err("expected complete durable replay".into());
    };
    require(events.len() == 3, "expected exactly one create, user message, and assistant completion")?;
    require(events[0].kind == "created" && events[1].kind == "submitted" && events[2].kind == "completed", "ordered durable events")?;
    require(events[1].run == run && events[2].run == run, "durable run identity")?;
    require(events[1].message.as_deref() == Some(MESSAGE) && events[1].text.as_deref() == Some(text), "original user message")?;
    require(events[2].message.as_ref().is_some_and(|id| id != MESSAGE)
        && events[2].text.as_deref() == Some(&format!("fake: {text}")), "exact durable assistant reply")?;
    require(events.windows(2).all(|pair| pair[0].sequence < pair[1].sequence) && after == events[2].sequence, "durable cursor order")?;
    require(matches!(send(client, replay(&endpoint.epoch.instance, after, 256))?,
        Reply::Replay { events, has_more: false, .. } if events.is_empty()), "cursor cannot replay the same events twice")?;
    require(matches!(send(client, replay("00000000-0000-4000-8000-000000000003", 0, 256))?, Reply::Error { .. }),
        "foreign instance cursor must be refused")?;
    require(matches!(send(client, submission(text))?, Reply::Decision { code, run: Some(id), .. } if code == "duplicate" && id == run),
        "exact duplicate must resolve to original run")?;
    require(matches!(send(client, submission("different"))?, Reply::Decision { code, .. } if code == "conflict"), "changed command bytes must conflict")?;
    Ok(())
}

fn flow(endpoint: &Endpoint, config: &Enrollment, mode: &str) -> Result {
    let mut client = connect(endpoint, config)?;
    create(&mut client)?;
    let text = match mode {
        "slow" => "\0".repeat(16384),
        "leave" => "q".repeat(16384),
        _ => "aβ𐐀".repeat(640),
    };
    let id = client.send(submission(&text))?;
    let first = client.receive(end(3))?;
    require(first.request == id, "admission correlation")?;
    let run = accepted(first.body)?;
    println!("ADMITTED {run}");
    if mode == "leave" {
        // Process exit closes the client handle. The controller checks the same runtime handle.
        return Ok(());
    }
    if mode == "slow" {
        thread::sleep(Duration::from_millis(2400));
        let deadline = end(2);
        loop {
            match client.receive(deadline) {
                Err(error) if disconnected(&error) => break,
                Err(error) => return Err(error.into()),
                Ok(frame) => require(matches!(frame.body, Reply::Progress { .. }), "slow subscriber must detach before terminal delivery")?,
            }
        }
        println!("DETACHED {run}");
        return Ok(());
    }
    // Pipeline the exact retry while the original submission still owns its request stream.
    let duplicate = client.send(submission(&text))?;
    let deadline = end(6);
    let mut progress = String::new();
    let mut index = 0;
    let mut saw_duplicate = false;
    let mut saw_completion = false;
    while !saw_duplicate || !saw_completion {
        let frame = client.receive(deadline)?;
        match frame.body {
            Reply::Progress { run: actual, index: current, text } if frame.request == id => {
                require(actual == run && current == index && text.len() <= 1024, "ordered bounded progress")?;
                progress.push_str(&text);
                index += 1;
            }
            Reply::Decision { code, run: Some(actual), .. } if frame.request == duplicate => {
                require(code == "duplicate" && actual == run, "pipelined retry must deduplicate")?;
                saw_duplicate = true;
            }
            Reply::Decision { code, run: Some(actual), .. } if frame.request == id => {
                require(code == "completed" && actual == run, "writer-confirmed completion")?;
                saw_completion = true;
            }
            _ => return Err("unexpected progress or decision".into()),
        }
    }
    require(progress == format!("fake: {text}"), "progress must reconstruct exact answer")?;
    check_committed(&mut client, endpoint, &text, run)
}

fn negative(endpoint: &Endpoint, config: &Enrollment, mode: &str) -> Result {
    let image = ImagePin::open(&config.peer_image, &config.peer_sha256)?;
    let name = PipeName::from_nonce(&endpoint.epoch.nonce)?;
    let expected = ProcessIdentity { pid: endpoint.pid, creation_time: endpoint.creation_time };
    let connect_end = end(2);
    let transport = loop {
        match Client::connect(&name, &image, expected) {
            Err(e) if e.raw_os_error() == Some(231) && Instant::now() < connect_end => {
                thread::sleep(Duration::from_millis(20));
            }
            result => break result?,
        }
    };
    let deadline = end(3);
    let mut hello = Hello { epoch: endpoint.epoch.clone(), client_nonce: nonce()? };
    if mode == "stale-hello" { hello.epoch.runtime_epoch += 1; }
    if mode == "version" { hello.epoch.version += 1; }
    write_frame(&transport.pipe, &hello, CONTROL_MAX, deadline)?;
    if matches!(mode, "stale-hello" | "version") {
        return require(read_frame::<Ready>(&transport.pipe, CONTROL_MAX, deadline, deadline).is_err_and(|e| disconnected(&e)),
            "invalid hello must disconnect without a Ready");
    }
    let ready: Ready = read_frame(&transport.pipe, CONTROL_MAX, deadline, deadline)?;
    let mut binding = Binding { epoch: ready.hello.epoch, session: ready.session };
    if mode == "stale-session" { binding.session = nonce()?; }
    let sequence = if mode == "sequence" { 2 } else { 1 };
    let request = Frame { binding, sequence, request: sequence, body: Request::Status };
    match mode {
        "zero" => transport.pipe.write_all(&0u32.to_le_bytes(), deadline)?,
        "malformed" | "trailing" | "utf8" | "deep" | "duplicate" => {
            let bytes = match mode {
                "malformed" => b"{".to_vec(),
                "trailing" => { let mut b=encode(&request,CONTROL_MAX)?;b.extend_from_slice(b"{}");b },
                "utf8" => vec![0xff],
                "deep" => format!("{}0{}", "[".repeat(256), "]".repeat(256)).into_bytes(),
                _ => { let mut b=encode(&request,CONTROL_MAX)?;b.pop();b.extend_from_slice(br#","sequence":1}"#);b },
            };
            transport.pipe.write_all(&(bytes.len() as u32).to_le_bytes(),deadline)?;
            transport.pipe.write_all(&bytes,deadline)?;
        }
        "coalesced" => {
            let first=encode(&request,CONTROL_MAX)?;
            let second=Frame {binding:request.binding.clone(),sequence:2,request:2,body:Request::Status};
            let second=encode(&second,CONTROL_MAX)?;
            let mut bytes=Vec::new();
            bytes.extend_from_slice(&(first.len() as u32).to_le_bytes());bytes.extend_from_slice(&first);
            bytes.extend_from_slice(&(second.len() as u32).to_le_bytes());bytes.extend_from_slice(&second);
            transport.pipe.write_all(&bytes,deadline)?;
            for id in 1..=2 {
                let f:Frame<Reply>=read_frame(&transport.pipe,RESPONSE_MAX,deadline,deadline)?;
                require(f.request==id && f.sequence==id && matches!(f.body,Reply::Status {..}),"coalesced frame correlation")?;
            }
            return Ok(());
        }
        "oversize" => transport.pipe.write_all(&u32::MAX.to_le_bytes(), deadline)?,
        "partial" => transport.pipe.write_all(&[1], deadline)?,
        "forged" => {
            let mut data = encode(&request, CONTROL_MAX)?;
            data.pop();
            data.extend_from_slice(br#","owner":"self-asserted"}"#);
            transport.pipe.write_all(&(data.len() as u32).to_le_bytes(), deadline)?;
            transport.pipe.write_all(&data, deadline)?;
        }
        "fragmented" => {
            let data = encode(&request, CONTROL_MAX)?;
            for byte in (data.len() as u32).to_le_bytes() {
                transport.pipe.write_all(&[byte], deadline)?;
            }
            for piece in data.chunks(11) {
                transport.pipe.write_all(piece, deadline)?;
            }
            let frame: Frame<Reply> = read_frame(&transport.pipe, RESPONSE_MAX, deadline, deadline)?;
            return require(frame.request == 1 && frame.sequence == 1 && frame.binding == request.binding
                && matches!(frame.body, Reply::Status { .. }), "fragmented authenticated frame");
        }
        _ => write_frame(&transport.pipe, &request, CONTROL_MAX, deadline)?,
    }
    let deadline = end(3);
    require(read_frame::<Frame<Reply>>(&transport.pipe, RESPONSE_MAX, deadline, deadline).is_err_and(|e| disconnected(&e)),
        "rejected frame must disconnect without a response")
}


fn ingress_probe(endpoint: &Endpoint, config: &Enrollment, mode: &str) -> Result {
    if mode == "ingress-quiet" {
        drop(connect(endpoint, config)?);
        return Ok(());
    }
    if mode == "ingress-burst" {
        let mut client = connect(endpoint, config)?;
        let mut sent = 0;
        for _ in 0..8 {
            match client.send(Request::Status) {
                Ok(_) => sent += 1,
                Err(error) if disconnected(&error) => return Ok(()),
                Err(error) => return Err(error.into()),
            }
        }
        let deadline = end(3);
        for expected in 1..=sent {
            match client.receive(deadline) {
                Ok(frame) => require(frame.request == expected && matches!(frame.body, Reply::Status { .. }),
                                     "bounded burst must correlate status replies")?,
                Err(error) if disconnected(&error) => return Ok(()),
                Err(error) => return Err(error.into()),
            }
        }
        return Ok(());
    }
    require(mode == "ingress-partial-exit", "unknown ingress probe")?;
    let image = ImagePin::open(&config.peer_image, &config.peer_sha256)?;
    let name = PipeName::from_nonce(&endpoint.epoch.nonce)?;
    let expected = ProcessIdentity { pid: endpoint.pid, creation_time: endpoint.creation_time };
    let deadline = end(2);
    let transport = loop {
        match Client::connect(&name, &image, expected) {
            Err(error) if error.raw_os_error() == Some(231) && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(20));
            }
            result => break result?,
        }
    };
    let hello = Hello { epoch: endpoint.epoch.clone(), client_nonce: nonce()? };
    let deadline = end(3);
    write_frame(&transport.pipe, &hello, CONTROL_MAX, deadline)?;
    let ready: Ready = read_frame(&transport.pipe, CONTROL_MAX, deadline, deadline)?;
    require(ready.hello.epoch == hello.epoch && ready.hello.client_nonce == hello.client_nonce,
            "ingress probe must authenticate the runtime epoch")?;
    PipeName::from_nonce(&ready.session)?;
    transport.pipe.write_all(&[1], end(2))?;
    // Dropping this client after a partial frame must not strand the server reader.
    Ok(())
}


fn witness(root: &Path, endpoint: &Endpoint, name: &str) -> Result<String> {
    let root = root.join(format!("test-epoch-{}", endpoint.epoch.nonce));
    let deadline = end(4);
    loop {
        match std::fs::read_to_string(root.join(format!("witness-{name}"))) {
            Ok(value) => {
                let suffix = format!("|{}|{}|{}|{}", endpoint.pid, endpoint.creation_time, endpoint.epoch.runtime_epoch, endpoint.epoch.nonce);
                require(value.trim().ends_with(&suffix) && value.len() < 512, "test witness owner binding")?;
                return Ok(value);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound && Instant::now() < deadline => thread::sleep(Duration::from_millis(1)),
            Err(e) => return Err(e.into()),
        }
    }
}

fn release(root: &Path, endpoint: &Endpoint, gate: &str, interrupt: bool) -> Result {
    use std::io::Write;
    let binding = witness(root, endpoint, gate)?;
    let root = root.join(format!("test-epoch-{}", endpoint.epoch.nonce));
    let verb = if interrupt { "interrupt" } else { "release" };
    let pending=root.join(format!("{verb}-{gate}.pending"));
    let mut f = std::fs::OpenOptions::new().create_new(true).write(true).open(&pending)?;
    f.write_all(binding.as_bytes())?;
    drop(f);
    std::fs::rename(pending,root.join(format!("{verb}-{gate}")))?;
    Ok(())
}

fn finish_shutdown(client: &mut Remote) -> Result {
    let request = client.send(Request::Shutdown)?;
    let until = end(5);
    loop {
        match client.receive(until) {
            Ok(frame) if frame.request == request && matches!(frame.body, Reply::Closed) => return Ok(()),
            Ok(frame) if frame.request == request && matches!(frame.body, Reply::ShutdownAccepted) => {},
            Err(e) if disconnected(&e) => return Ok(()),
            _ => return Err("shutdown did not settle".into()),
        }
    }
}

fn gap_work(endpoint: &Endpoint, config: &Enrollment, root: &Path, mode: &str) -> Result {
    let mut client = connect(endpoint, config)?;
    if mode == "gap-delayed-finish" {
        require(matches!(send(&mut client, Request::Status)?, Reply::Status { lifecycle, .. } if lifecycle == "close_incomplete"), "retryable close status")?;
        require(matches!(send(&mut client, submission("blocked"))?, Reply::Error {code,..} if code=="shutting_down"), "mutation admission stays closed")?;
        release(root, endpoint, "fake-paused", false)?;
        witness(root, endpoint, "writer-confirmed")?;
        return finish_shutdown(&mut client);
    }
    if mode == "gap-replay" {
        let Reply::Replay {events,..} = send(&mut client, replay(&endpoint.epoch.instance, 0, 256))? else { return Err("replay missing".into()) };
        let run=events.get(1).ok_or("missing admitted event")?.run;
        check_committed(&mut client, endpoint, "gap", run)?;
        require(matches!(send(&mut client, Request::Read {conversation:"00000000-0000-4000-8000-000000000099".into()})?,Reply::State {found:false,..}),"post-shutdown mutation was not applied")?;
        return finish_shutdown(&mut client);
    }
    create(&mut client)?;
    let id = client.send(submission("gap"))?;
    let frame = client.receive(end(3))?;
    require(frame.request==id, "gap admission request")?;
    let run=accepted(frame.body)?;
    println!("ADMITTED {run}");
    witness(root, endpoint, "fake-paused")?;
    witness(root, endpoint, "fake-invocation-1")?;
    require(!root.join(format!("test-epoch-{}", endpoint.epoch.nonce)).join("witness-writer-confirmed").exists(), "active witness precedes commit")?;
    if mode == "gap-leave" { return Ok(()); }
    if mode == "gap-delayed" || mode == "gap-shutdown" {
        let shutdown = client.send(Request::Shutdown)?;
        let ack=client.receive(end(3))?;
        require(ack.request==shutdown && matches!(ack.body,Reply::ShutdownAccepted), "shutdown admission")?;
        witness(root, endpoint, "draining")?;
        if mode == "gap-shutdown" {
            // Any pipelined mutation after Shutdown must never reach admission.
            let _ = client.send(Request::Create {command:"post-stop".into(),conversation:"00000000-0000-4000-8000-000000000099".into()});
            release(root, endpoint, "fake-paused", false)?;
        }
        let until=end(5);
        loop {
            match client.receive(until) {
                Ok(f) if matches!(f.body,Reply::Closed) => break,
                Ok(f) if matches!(f.body,Reply::Progress {..} | Reply::Decision {..}) => {},
                Err(e) if disconnected(&e) => break,
                _ => return Err("drain result unconfirmed".into()),
            }
        }
        if mode=="gap-delayed" {
            witness(root,endpoint,"close-incomplete")?;
            let mut client=connect(endpoint,config)?;
            require(matches!(send(&mut client,Request::Status)?,Reply::Status {lifecycle,..} if lifecycle=="close_incomplete"),"close remains incomplete")?;
            require(matches!(send(&mut client,submission("blocked"))?,Reply::Error {code,..} if code=="shutting_down"),"drain mutation refusal")?;
        }
        return Ok(());
    }
    if mode == "gap-loss" {
        drop(client);
        let mut client=connect(endpoint,config)?;
        require(matches!(send(&mut client,Request::Status)?,Reply::Status {..}),"status before gated commit")?;
        release(root,endpoint,"fake-paused",false)?;
        witness(root,endpoint,"writer-confirmed")?;
        witness(root,endpoint,"terminal-paused")?;
        drop(client);
        let mut client=connect(endpoint,config)?;
        check_committed(&mut client,endpoint,"gap",run)?;
        release(root,endpoint,"terminal-paused",false)?;
        return finish_shutdown(&mut client);
    }
    if mode == "relay-panic-active" {
        release(root, endpoint, "fake-paused", false)?;
        witness(root, endpoint, "writer-confirmed")?;
        witness(root, endpoint, "terminal-paused")?;
        witness(root, endpoint, "relay-active-session")?;

        let epoch_root = root.join(format!("test-epoch-{}", endpoint.epoch.nonce));
        let pending = epoch_root.join("release-terminal-paused.pending");
        std::fs::OpenOptions::new().write(true).create_new(true).open(&pending)?;
        std::fs::rename(pending, epoch_root.join("release-terminal-paused"))?;
        witness(root, endpoint, "relay-fault-active-loop")?;

        let probe = client.send(Request::Create {
            command: "post-relay-fault".into(),
            conversation: "00000000-0000-4000-8000-000000000099".into(),
        })?;
        let deadline = end(1);
        loop {
            match client.receive(deadline) {
                Ok(frame) => require(
                    frame.request != probe && !matches!(frame.body, Reply::Closed),
                    "retained runtime replied to a new mutation or reported Closed",
                )?,
                Err(IoError::Deadline) => break,
                Err(error) => return Err(error.into()),
            }
        }
        drop(client);
        match Remote::connect(endpoint, config) {
            Err(IoError::Deadline) => {}
            Err(IoError::System(error)) if error.raw_os_error() == Some(231) => {}
            _ => return Err("retained runtime should refuse a new authenticated session".into()),
        }
        println!("ACTIVE_RELAY_RETENTION_NO_NEW_REPLY");
        return Ok(());
    }
    require(mode=="gap-dedup", "unknown gap mode")?;
    for (text,code) in [("gap","duplicate"),("changed","conflict")] {
        let request=client.send(submission(text))?;
        let f=client.receive(end(3))?;
        require(f.request==request,"active duplicate correlation")?;
        require(matches!(f.body, Reply::Decision {code:actual,..} if actual==code),"active duplicate/conflict")?;
    }
    release(root,endpoint,"fake-paused",false)?;
    let mut completed=false;
    let mut text=String::new();
    while !completed {
        let f=client.receive(end(3))?;
        require(f.request==id,"original stream identity")?;
        match f.body {
            Reply::Progress {text:chunk,..} => text.push_str(&chunk),
            Reply::Decision {code,..} if code=="completed" => completed=true,
            _ => return Err("unexpected active stream result".into()),
        }
    }
    require(text=="fake: gap","bounded exact progress")?;
    require(!root.join(format!("test-epoch-{}", endpoint.epoch.nonce)).join("witness-fake-invocation-2").exists(),"exactly one invocation")?;
    drop(client);
    let mut client=connect(endpoint,config)?;
    check_committed(&mut client,endpoint,"gap",run)?;
    for (after,limit) in [(-1,256),(i64::MAX,256),(0,0),(0,257)] {
        require(matches!(send(&mut client,replay(&endpoint.epoch.instance,after,limit))?,Reply::Error {..}),"invalid cursor/page bound")?;
    }
    finish_shutdown(&mut client)
}


fn gap_certainty(endpoint: &Endpoint, config: &Enrollment, root: &Path, mode: &str) -> Result {
    let mut client=connect(endpoint,config)?;
    if mode=="gap-certainty" {
        create(&mut client)?;
        require(matches!(send(&mut client,submission("gap"))?,Reply::Error {code,certainty:Some(c),phase:Some(p)} if code=="operation_expired" && c=="not_applied" && p=="queued"),"queued certainty through dispatch")?;
        require(matches!(send(&mut client,submission("gap"))?,Reply::Error {code,certainty:Some(c),phase:Some(p)} if code=="storage_error" && c=="outcome_unknown" && p=="commit_issued"),"commit uncertainty through dispatch")?;
        return finish_shutdown(&mut client);
    }
    require(matches!(send(&mut client,submission("gap"))?,Reply::Decision {code,state:Some(yokai_ipc::RunStatus::Held),..} if code=="duplicate"),"committed uncertain admission resolves Held without redispatch")?;
    let Reply::Replay {events,..}=send(&mut client,replay(&endpoint.epoch.instance,0,256))? else{return Err("uncertain replay".into())};
    require(events.len()==3 && events[1].kind=="submitted" && events[2].kind=="held", "uncertain commit independently resolved")?;
    let second="00000000-0000-4000-8000-000000000011";
    if mode=="gap-held" {
        require(matches!(send(&mut client,Request::Create {command:"held-create".into(),conversation:second.into()})?,Reply::Decision {code,..} if code=="created"),"second conversation")?;
        let request=client.send(Request::Submit {command:"held-submit".into(),conversation:second.into(),message:"00000000-0000-4000-8000-000000000012".into(),text:"hold".into()})?;
        let f=client.receive(end(3))?;require(f.request==request,"held admission correlation")?;accepted(f.body)?;
        witness(root,endpoint,"fake-paused")?;
        release(root,endpoint,"fake-paused",true)?;
        // Drain the known missing completion notice before issuing Shutdown.
        let f=client.receive(end(3))?;
        require(f.request==request && matches!(f.body,Reply::Error {code,..} if code=="completion_unconfirmed"),"cooperative interruption stays unconfirmed")?;
    } else {
        require(matches!(send(&mut client,Request::Read {conversation:second.into()})?,Reply::State {latest:Some((_,yokai_ipc::RunStatus::Held)),..}),"interrupted work becomes Held on reopen")?;
        require(matches!(send(&mut client,Request::Submit {command:"held-submit".into(),conversation:second.into(),message:"00000000-0000-4000-8000-000000000012".into(),text:"hold".into()})?,Reply::Decision {code,state:Some(yokai_ipc::RunStatus::Held),..} if code=="duplicate"),"held duplicate never restarts")?;
    }
    finish_shutdown(&mut client)
}

fn gap_pressure(endpoint: &Endpoint, config: &Enrollment, root: &Path) -> Result {
    let mut client=connect(endpoint,config)?;
    create(&mut client)?;
    let text="q".repeat(16384);
    let run=accepted(send(&mut client,submission(&text))?)?;
    witness(root,endpoint,"delivery-paused")?;
    witness(root,endpoint,"progress-queue-full-16")?;
    // The writer is now held in its fixture gate; the real reader fills its one-slot queue.
    let _=client.send(Request::Status);
    let _=client.send(Request::Status);
    witness(root,endpoint,"ingress-queue-full-1")?;
    release(root,endpoint,"delivery-paused",false)?;
    drop(client);
    witness(root,endpoint,"writer-confirmed")?;
    let mut client=connect(endpoint,config)?;
    check_committed(&mut client,endpoint,&text,run)?;
    drop(client);
    negative(endpoint,config,"partial")?;
    let mut client=connect(endpoint,config)?;
    // No bytes are sent: this is the same idle-deadline branch with a fixture-shortened bound.
    require(client.receive(end(2)).is_err_and(|e| disconnected(&e)),"idle subscriber deadline")?;
    drop(client);
    let mut client=connect(endpoint,config)?;
    finish_shutdown(&mut client)
}

fn gap_wire(endpoint: &Endpoint, config: &Enrollment, root: &Path) -> Result {
    // Read two frames from one byte stream directly, independently of the service's
    // deliberately one-slot ingress queue. The first occupies the exact size cap.
    let own: Enrollment=read_control(&root.join("runtime.json"))?;
    let pin=ImagePin::open(&own.peer_image,&own.peer_sha256)?;
    let name=PipeName::from_nonce(&nonce()?)?;
    let listener=Listener::new(&name)?;
    let client=Client::connect(&name,&pin,yokai_platform::local_ipc::current_identity()?)?;
    listener.accept(end(2))?;
    listener.authenticate(&pin)?;
    let request=Request::Status;
    let mut first=encode(&request,yokai_ipc::REQUEST_MAX)?;
    first.resize(yokai_ipc::REQUEST_MAX,b' ');
    let second=encode(&Request::Shutdown,CONTROL_MAX)?;
    let mut wire=Vec::new();
    wire.extend_from_slice(&(first.len() as u32).to_le_bytes());wire.extend_from_slice(&first);
    wire.extend_from_slice(&(second.len() as u32).to_le_bytes());wire.extend_from_slice(&second);
    thread::scope(|scope| -> Result {
        let writer=scope.spawn(|| client.pipe.write_all(&wire,end(3)));
        require(matches!(read_frame::<Request>(listener.pipe(),yokai_ipc::REQUEST_MAX,end(3),end(3))?,Request::Status),"exact-cap first coalesced frame")?;
        require(matches!(read_frame::<Request>(listener.pipe(),yokai_ipc::REQUEST_MAX,end(3),end(3))?,Request::Shutdown),"second coalesced frame")?;
        writer.join().map_err(|_| "coalesced writer panic")??;
        Ok(())
    })?;
    listener.disconnect()?;
    drop(client);
    for mode in ["fragmented", "zero", "oversize", "malformed", "trailing", "utf8", "deep", "duplicate"] {
        negative(endpoint,config,mode)?;
    }
    let mut client=connect(endpoint,config)?;
    require(yokai_ipc::frame_high_water() <= RESPONSE_MAX,"owned frame allocation bound")?;
    println!("FRAME_HIGH_WATER {}",yokai_ipc::frame_high_water());
    finish_shutdown(&mut client)
}

fn main() -> Result {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let mode = args.first().and_then(|value| value.to_str()).ok_or("mode required")?;
    if matches!(mode, "helper-exit" | "helper-hold" | "impostor") {
        let directory = Path::new(args.get(1).ok_or("fixture control directory required")?);
        let _listener = if mode == "impostor" {
            Some(Listener::new(&PipeName::from_nonce(args.get(2).and_then(|v| v.to_str()).ok_or("pipe nonce required")?)?)?)
        } else { None };
        std::fs::OpenOptions::new().create_new(true).write(true).open(directory.join("helper.ready"))?;
        if mode == "helper-hold" {
            loop { thread::sleep(Duration::from_secs(1)); }
        }
        let deadline = end(10);
        while !directory.join("helper.stop").is_file() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        return require(directory.join("helper.stop").is_file(), "cooperative helper release not observed");
    }
    let endpoint: Endpoint = read_control(Path::new(args.get(1).ok_or("endpoint required")?))?;
    let config: Enrollment = read_control(Path::new(args.get(2).ok_or("enrollment required")?))?;
    match mode {
        "happy-shutdown" => { flow(&endpoint,&config,"happy")?; finish_shutdown(&mut connect(&endpoint,&config)?)?; },
        "replay-happy-shutdown" => {
            let mut client=connect(&endpoint,&config)?;
            let run: i64=args.get(3).and_then(|s|s.to_str()).ok_or("run")?.parse()?;
            check_committed(&mut client,&endpoint,&"aβ𐐀".repeat(640),run)?;
            finish_shutdown(&mut client)?;
        },
        "gap-discovery" => {
            let mut stale=endpoint.clone();stale.creation_time-=1;
            require(connect(&stale,&config).is_err(),"stale creation refusal")?;
            for m in ["stale-hello","stale-session","sequence"] { negative(&endpoint,&config,m)?; }
            finish_shutdown(&mut connect(&endpoint,&config)?)?;
            // Close acknowledgement is not process exit; the controller checks that separately.
            // Within this helper, require the old pipe name to disappear after shutdown.
            let deadline=end(3);
            loop {
                match connect(&endpoint,&config) {
                    Err(IoError::System(e)) if e.raw_os_error()==Some(2) => break,
                    Err(IoError::System(e)) if e.raw_os_error()==Some(231) && Instant::now()<deadline => thread::sleep(Duration::from_millis(1)),
                    _ => return Err("closed endpoint was not refused".into()),
                }
            }
        },
        "gap-certainty" | "gap-held" | "gap-held-reopen" => gap_certainty(&endpoint,&config,Path::new(&args[1]).parent().ok_or("control root")?,mode)?,
        "gap-pressure" => gap_pressure(&endpoint,&config,Path::new(&args[1]).parent().ok_or("control root")?)?,
        "gap-wire" => gap_wire(&endpoint,&config,Path::new(&args[1]).parent().ok_or("control root")?)?,
        "relay-panic-active" | "gap-dedup" | "gap-loss" | "gap-leave" | "gap-shutdown" | "gap-delayed" | "gap-delayed-finish" | "gap-replay" => gap_work(&endpoint, &config, Path::new(&args[1]).parent().ok_or("control root")?, mode)?,
        "happy" | "leave" | "slow" => flow(&endpoint, &config, mode)?,
        "ingress-quiet" | "ingress-partial-exit" | "ingress-burst" => ingress_probe(&endpoint, &config, mode)?,
        "replay-happy" | "replay-leave" | "replay-slow" => {
            let text = match mode {
                "replay-leave" => "q".repeat(16384),
                "replay-slow" => "\0".repeat(16384),
                _ => "aβ𐐀".repeat(640),
            };
            let run: i64 = args.get(3).and_then(|v| v.to_str()).ok_or("run ID required")?.parse()?;
            let mut client = connect(&endpoint, &config)?;
            check_committed(&mut client, &endpoint, &text, run)?;
        }
        "fragmented" | "stale-hello" | "version" | "stale-session" | "sequence" | "oversize" | "partial" | "forged" => negative(&endpoint, &config, mode)?,
        "dead-endpoint" | "wrong-identity" | "wrong-instance" => {
            let result = connect(&endpoint, &config);
            let refused = match (mode, result) {
                ("dead-endpoint", Err(IoError::System(e))) => e.raw_os_error() == Some(2),
                ("wrong-identity", Err(IoError::System(e))) => e.kind() == io::ErrorKind::PermissionDenied,
                ("wrong-instance", Err(IoError::System(e))) => e.kind() == io::ErrorKind::InvalidData,
                _ => false,
            };
            require(refused, "expected the selected connection refusal")?;
        }
        "status" => {
            let mut client = connect(&endpoint, &config)?;
            require(matches!(send(&mut client, Request::Status)?, Reply::Status { lifecycle, .. } if lifecycle == "running"), "runtime status")?;
        }
        "shutdown" => {
            let mut client = connect(&endpoint, &config)?;
            let request = client.send(Request::Shutdown)?;
            let deadline = end(14);
            loop {
                match client.receive(deadline) {
                    Ok(frame) if frame.request == request => match frame.body {
                        Reply::ShutdownAccepted => println!("SHUTDOWN_ACCEPTED"),
                        Reply::Closed => { println!("CLOSED_REPLY"); break; }
                        _ => return Err("unexpected shutdown response".into()),
                    },
                    Err(error) if disconnected(&error) => { println!("SHUTDOWN_REPLY_DISCONNECTED"); break; }
                    _ => return Err("shutdown transport outcome unconfirmed".into()),
                }
            }
        }
        _ => return Err("unknown focused helper mode".into()),
    }
    println!("CHECK {mode} PASS");
    Ok(())
}
