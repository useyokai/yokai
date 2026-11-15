//! Finite, fixture-local witnesses. This module does not exist in product builds.
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

struct Control { root: PathBuf, mode: String, binding: String, events: Mutex<Vec<String>> }
static PENDING: OnceLock<(String, String)> = OnceLock::new();
pub fn prepare(mode: &str, token: &str) -> io::Result<()> {
    PENDING.set((mode.into(), token.into())).map_err(|_| io::Error::other("duplicate test configuration"))
}
pub(super) fn bind(root: &Path, epoch: &yokai_ipc::Epoch) -> io::Result<()> {
    if let Some((mode, token)) = PENDING.get() { configure(root, mode, token, epoch)?; }
    Ok(())
}
static CONTROL: OnceLock<Control> = OnceLock::new();

pub fn configure(root: &Path, mode: &str, token: &str, epoch: &yokai_ipc::Epoch) -> io::Result<()> {
    if !matches!(mode, "dedup" | "loss" | "backpressure" | "client-exit" | "shutdown" | "delayed" | "held" | "certainty" | "discovery") {
        return Err(io::Error::other("unknown synthetic test mode"));
    }
    yokai_platform::local_ipc::PipeName::from_nonce(token)?;
    let id = yokai_platform::local_ipc::current_identity()?;
    let binding = format!("{token}|{}|{}|{}|{}", id.pid, id.creation_time, epoch.runtime_epoch, epoch.nonce);
    let root = root.join(format!("test-epoch-{}", epoch.nonce));
    std::fs::create_dir(&root)?;
    CONTROL.set(Control { root, mode: mode.into(), binding, events: Mutex::new(Vec::new()) })
        .map_err(|_| io::Error::other("test controls already configured"))?;
    event("configured");
    Ok(())
}

pub(super) fn mode(name: &str) -> bool { CONTROL.get().is_some_and(|c| c.mode == name) }

pub(crate) fn event(name: &str) {
    let Some(c) = CONTROL.get() else { return };
    let mut events = c.events.lock().expect("test witness mutex");
    if events.iter().any(|s| s == name) { return }
    assert!(events.len() < 64 && name.len() < 80 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
    let pending = c.root.join(format!("witness-{name}.pending"));
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&pending).expect("create test witness");
    writeln!(file, "{}", c.binding).expect("write test witness");
    drop(file);
    std::fs::rename(pending,c.root.join(format!("witness-{name}"))).expect("publish test witness");
    events.push(name.into());
}

pub(crate) fn gate(name: &str, deadline: Instant) -> bool {
    let Some(c) = CONTROL.get() else { return true };
    event(name);
    loop {
        for (verb, run) in [("release", true), ("interrupt", false)] {
            let path = c.root.join(format!("{verb}-{name}"));
            match std::fs::File::open(path) {
                Ok(file) => {
                    let mut value = String::new();
                    file.take(513).read_to_string(&mut value).expect("read test release");
                    assert_eq!(value.trim(), c.binding, "test release belongs to another owner");
                    return run;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => panic!("test release unavailable: {e}"),
            }
        }
        if Instant::now() >= deadline { event("gate-expired"); return false; }
        std::thread::sleep(Duration::from_millis(1));
    }
}

pub(crate) fn before_fake(deadline: Instant) -> bool {
    if CONTROL.get().is_none() { return true }
    static INVOCATIONS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let count = INVOCATIONS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    event(&format!("fake-invocation-{count}"));
    event("fake-entered");
    if ["dedup", "loss", "client-exit", "shutdown", "delayed", "held"].iter().any(|m| mode(m)) {
        gate("fake-paused", deadline)
    } else { true }
}

pub(super) fn shutdown_duration() -> Duration {
    if mode("delayed") { Duration::from_millis(200) } else { Duration::from_secs(15) }
}

pub(super) fn fault_reply(owner: &crate::ConversationRuntime, request: &yokai_ipc::Request, deadline: Instant) -> Option<super::service::Action> {
    if !mode("certainty") { return None }
    let yokai_ipc::Request::Submit {command,conversation,message,text} = request else { return None };
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let index = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    if index >= 2 { return None }
    let parsed = (|| {
        let command = yokai_types::CommandId::new(command).ok()?;
        let conversation = conversation.parse().ok()?;
        let message = message.parse().ok()?;
        yokai_types::SubmitMessage::new(command, conversation, message, text).ok()
    })();
    let Some(request) = parsed else { return Some(super::service::Action::Reply(yokai_ipc::Reply::error("invalid_request"))) };
    let fault = if index == 0 { yokai_store::test_api::WriteFault::None }
        else { yokai_store::test_api::WriteFault::Commit {code:10, actually_commit:true} };
    // The operation expires before admission; its result still needs a bounded observation window.
    let operation_deadline = if index == 0 { Instant::now() } else { deadline };
    let result = owner.test_submit_fault(request,operation_deadline,deadline,fault);
    Some(super::service::Action::Reply(result.map_or_else(super::service::write_error, super::service::decision_reply)))
}

pub(super) fn pipe_nonce() -> Option<String> {
    PENDING.get().filter(|(mode,_)| mode=="discovery").map(|(_,token)|token.clone())
}
