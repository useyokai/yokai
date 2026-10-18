//! Test-only fault injection and observations at fixed lifecycle boundaries.
use std::fs::OpenOptions;
use std::io::{BufRead, Read, Write};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use yokai_types::InstanceId;

static NEXT_ID: OnceLock<Mutex<Option<InstanceId>>> = OnceLock::new();
static CONNECTION_EVENTS: OnceLock<Mutex<Vec<&'static str>>> = OnceLock::new();
static MUTATE_STAGE_A: OnceLock<Mutex<bool>> = OnceLock::new();
static HEADER_VERSIONS: OnceLock<Mutex<Vec<u32>>> = OnceLock::new();

pub fn set_next_identity(id: [u8; 16]) -> Result<(), String> {
    let mut value = NEXT_ID.get_or_init(|| Mutex::new(None)).lock()
        .map_err(|_| "identity override poisoned".to_string())?;
    *value = Some(InstanceId::from_bytes(id));
    Ok(())
}
pub fn identity_override() -> Result<Option<InstanceId>, String> {
    NEXT_ID.get_or_init(|| Mutex::new(None)).lock()
        .map(|value| *value).map_err(|_| "identity override poisoned".to_string())
}
pub fn record_connection(kind: &'static str) {
    if let Ok(mut events) = CONNECTION_EVENTS.get_or_init(|| Mutex::new(Vec::new())).lock() {
        events.push(kind);
    }
}
pub fn take_connection_events() -> Vec<&'static str> {
    CONNECTION_EVENTS.get_or_init(|| Mutex::new(Vec::new())).lock()
        .map(|mut events| std::mem::take(&mut *events)).unwrap_or_default()
}
pub fn record_header_version(version: u32) -> Result<(), String> {
    let mut values = HEADER_VERSIONS.get_or_init(|| Mutex::new(Vec::new())).lock()
        .map_err(|_| "header-version observation poisoned".to_string())?;
    values.push(version);
    Ok(())
}
pub fn take_header_versions() -> Result<Vec<u32>, String> {
    HEADER_VERSIONS.get_or_init(|| Mutex::new(Vec::new())).lock()
        .map(|mut values| std::mem::take(&mut *values))
        .map_err(|_| "header-version observation poisoned".to_string())
}
pub fn arm_stage_a_mutation() -> Result<(), String> {
    let mut armed = MUTATE_STAGE_A.get_or_init(|| Mutex::new(false)).lock()
        .map_err(|_| "Stage-A mutation hook poisoned".to_string())?;
    *armed = true;
    Ok(())
}
pub fn mutate_stage_a_once(path: &Path) -> Result<(), String> {
    let mut armed = MUTATE_STAGE_A.get_or_init(|| Mutex::new(false)).lock()
        .map_err(|_| "Stage-A mutation hook poisoned".to_string())?;
    if !*armed { return Ok(()); }
    *armed = false;
    drop(armed);
    let mut file = OpenOptions::new().append(true).open(path)
        .map_err(|e| format!("open protected database for injected write: {e}"))?;
    file.write_all(&[0x5a]).map_err(|e| format!("inject Stage-A write: {e}"))
}
pub fn clear() {
    if let Some(value) = NEXT_ID.get() {
        if let Ok(mut guard) = value.lock() { *guard = None; }
    }
    if let Some(value) = MUTATE_STAGE_A.get() {
        if let Ok(mut guard) = value.lock() { *guard = false; }
    }
    let _ = take_connection_events();
    let _ = take_header_versions();
}
pub fn hit(boundary: &str) -> Result<(), String> {
    let Ok(value) = std::env::var("YOKAI_TEST_FAILPOINT") else { return Ok(()) };
    let Some((selected, mode)) = value.split_once(':') else { return Ok(()) };
    if selected != boundary { return Ok(()) }
    match mode {
        "error" => Err(format!("test failpoint at {boundary}")),
        "exit" => std::process::exit(86),
        "wait" | "pause" => {
            let path=std::env::var_os("YOKAI_TEST_READY_FILE").ok_or("readiness marker missing")?;
            let mut file=OpenOptions::new().write(true).create_new(true).open(path).map_err(|e|e.to_string())?;
            file.write_all(boundary.as_bytes()).map_err(|e|e.to_string())?;
            file.sync_all().map_err(|e|e.to_string())?;
            if mode == "pause" {
                let input = std::io::stdin();
                let mut bounded = input.lock().take(10);
                let mut command = String::new();
                bounded.read_line(&mut command).map_err(|e| e.to_string())?;
                if command != "continue\n" && command != "continue\r\n" {
                    return Err("test close pause requires an explicit continue command".into());
                }
                Ok(())
            } else {
                loop {std::thread::park_timeout(std::time::Duration::from_secs(1));}
            }
        }
        _ => Ok(()),
    }
}
