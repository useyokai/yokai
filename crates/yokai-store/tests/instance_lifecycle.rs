//! Standalone lifecycle harness for the store candidate.
//!
//! This is a standalone target. It preserves every CHECK ID selected by the
//! candidate configuration and emits a separate SHA256-1 known-vector check.
//! Linux clears the dumpable flag; Windows requires the explicit isolated-run opt-in
//! documented by yokai-testsupport and reports that dumpability is unsupported.
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::{Arc, Barrier, atomic::{AtomicUsize, Ordering}};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(windows)]
#[path = "support/windows_process.rs"]
mod windows_process;

use rusqlite::Connection;
use yokai_store::{self as store, IncompleteReason, Instance, InstanceError};
use yokai_types::InstanceId;

const CHECK_IDS: &[&str] = &[
    "IC-1", "IC-2", "IC-3", "IC-4", "IC-5", "IC-6", "IC-8", "IC-9", "IC-F",
    "IL-1", "IL-2", "IL-3", "IL-4", "IL-5", "IL-6", "IL-7", "IL-8", "IL-10",
    "IP-1", "IP-2", "IP-3", "LD-1", "LD-2", "LD-5",
    "PF-1", "PF-2a", "PF-2b", "PF-3a", "PF-3b", "PF-4", "PF-5", "PF-6",
    "PF-8", "PF-9", "PF-10", "PF-11", "PF-12", "PF-13", "PF-14", "PF-15",
    "PF-16", "PF-17", "PF-18", "SV-2", "SV-4", "SHA256-1",
];

struct Checks { total: usize, failed: usize }
impl Checks {
    fn record(&mut self, id: &str, result: Result<String, String>) {
        self.total += 1;
        match result {
            Ok(detail) => println!("CHECK {id} PASS {detail}"),
            Err(detail) => {
                self.failed += 1;
                println!("CHECK {id} FAIL {detail}");
            }
        }
    }
}

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

fn main() -> ExitCode {
    let dumpable = yokai_testsupport::make_non_dumpable();
    println!("{}", dumpable.line());
    if !dumpable.permits_checks() {
        println!("RESULT FAIL dumpability status unavailable or not opted in; no check was run");
        return ExitCode::from(2);
    }
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|arg| arg == "__child") {
        return ExitCode::from(child_main(&args).unwrap_or(90));
    }
    let mut checks = Checks { total: 0, failed: 0 };
    if let Err(error) = run(&mut checks) {
        checks.record("S0_setup", Err(error));
    }
    if checks.total != CHECK_IDS.len() {
        checks.record("S0_check_count", Err(format!(
            "emitted {} checks; configuration requires {}", checks.total, CHECK_IDS.len()
        )));
    }
    if checks.failed == 0 && checks.total == CHECK_IDS.len() {
        println!("RESULT PASS {}/{}", checks.total, checks.total);
        ExitCode::SUCCESS
    } else {
        println!("RESULT FAIL {}/{}", checks.failed, checks.total);
        ExitCode::FAILURE
    }
}

fn child_main(args: &[String]) -> Option<u8> {
    let mode = args.get(2)?.as_str();
    match mode {
        "create" => {
            let parent = PathBuf::from(args.get(3)?);
            match store::create_instance(&parent).and_then(Instance::close) {
                Ok(()) => Some(0),
                Err(error) => { eprintln!("{error}"); Some(4) }
            }
        }
        "failpoint" => {
            let parent = PathBuf::from(args.get(3)?);
            match store::create_instance(&parent) {
                Ok(instance) => { let _ = instance.close(); Some(5) }
                Err(error) => { eprintln!("{error}"); Some(4) }
            }
        }
        "probe" => {
            let root = PathBuf::from(args.get(3)?);
            match store::open_instance(&root) {
                Err(InstanceError::Busy(_)) => Some(20),
                Ok(instance) => { let _ = instance.close(); Some(0) }
                Err(error) => { eprintln!("{error}"); Some(4) }
            }
        }
        "hold" => {
            let root = PathBuf::from(args.get(3)?);
            let marker = PathBuf::from(args.get(4)?);
            let Ok(_instance) = store::open_instance(&root) else { return Some(4) };
            if fs::write(marker, b"locked").is_err() { return Some(6); }
            match std::io::stdin().lock().lines().next() {
                Some(Ok(command)) if command == "exit" => std::process::exit(0),
                Some(Ok(_)) | Some(Err(_)) => Some(7),
                None => Some(0),
            }
        }
        "cycles" => {
            let root = PathBuf::from(args.get(3)?);
            match child_cycles(&root) { Ok(()) => Some(0), Err(error) => { eprintln!("IL8_CHILD_FAIL {error}"); Some(15) } }
        }
        "wal-version" => {
            let path = PathBuf::from(args.get(3)?);
            let Ok(connection) = Connection::open(path) else { return Some(11) };
            if connection.execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; PRAGMA user_version=3;"
            ).is_err() { return Some(12); }
            // Leave the committed WAL files for the parent to inspect.
            std::process::exit(0);
        }
        _ => Some(89),
    }
}

fn run(checks: &mut Checks) -> Result<(), String> {
    let configured = std::env::var_os("YOKAI_TEST_DATA_DIR")
        .map(PathBuf::from).ok_or("YOKAI_TEST_DATA_DIR is not set")?;
    if !configured.is_absolute() || !configured.is_dir() {
        return Err(format!("{} is not an existing absolute directory", configured.display()));
    }
    let run_dir = fresh_dir(&configured, "instance_lifecycle")?;
    println!("INFO test_data={}", run_dir.display());

    checks.record("IC-1", case_create_shape(&run_dir));
    checks.record("IC-2", case_invalid_parents(&run_dir));
    checks.record("IC-3", case_existing_roots(&run_dir));
    checks.record("IC-4", case_concurrent_creators(&run_dir));
    checks.record("IC-5", case_same_identity_creators(&run_dir));
    checks.record("IC-6", case_creation_process_exits(&run_dir));
    checks.record("IC-8", case_directory_flushes(&run_dir));
    checks.record("IC-9", case_incomplete_untouched(&run_dir));
    checks.record("IC-F", case_fixture_instance(&run_dir));
    checks.record("IL-1", case_same_process_busy(&run_dir));
    checks.record("IL-2", case_other_process_busy(&run_dir));
    checks.record("IL-3", case_owner_process_death(&run_dir));
    checks.record("IL-4", case_open_aliases(&run_dir));
    checks.record("IL-5", case_open_creates_nothing(&run_dir));
    checks.record("IL-6", case_lock_file_removal_limit(&run_dir));
    checks.record("IL-7", case_close_order(&run_dir));
    checks.record("IL-8", case_repeated_open_close(&run_dir));
    checks.record("IL-10", case_lock_file_persists(&run_dir));
    checks.record("IP-1", case_root_symlink(&run_dir));
    checks.record("IP-2", case_entry_symlinks(&run_dir));
    checks.record("IP-3", case_symlinked_parent(&run_dir));
    checks.record("LD-1", case_location_policy(&run_dir));
    checks.record("LD-2", case_mountinfo_fixtures());
    checks.record("LD-5", case_tmpfs_test_override(&run_dir));
    checks.record("PF-1", case_pf1(&run_dir));
    checks.record("PF-2a", case_pf2a(&run_dir));
    checks.record("PF-2b", case_pf2b(&run_dir));
    checks.record("PF-3a", case_pf3(&run_dir, false));
    checks.record("PF-3b", case_pf3(&run_dir, true));
    checks.record("PF-4", case_pf4(&run_dir));
    checks.record("PF-5", case_pf5(&run_dir));
    checks.record("PF-6", case_pf6(&run_dir));
    checks.record("PF-8", case_pf8(&run_dir));
    checks.record("PF-9", case_pf9(&run_dir));
    checks.record("PF-10", case_pf10(&run_dir));
    checks.record("PF-11", case_pf11(&run_dir));
    checks.record("PF-12", case_pf12(&run_dir));
    checks.record("PF-13", case_pf13(&run_dir));
    checks.record("PF-14", case_pf14(&run_dir));
    checks.record("PF-15", case_pf15(&run_dir));
    checks.record("PF-16", case_pf16(&run_dir));
    checks.record("PF-17", case_pf17(&run_dir));
    checks.record("PF-18", case_pf18(&run_dir));
    checks.record("SV-2", case_sv2(&run_dir));
    checks.record("SV-4", case_sv4(&run_dir));
    checks.record("SHA256-1", case_sha256_vectors());
    Ok(())
}

fn fresh_dir(base: &Path, label: &str) -> Result<PathBuf, String> {
    let serial = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?.as_nanos();
    let path = base.join(format!("{label}-{}-{now}-{serial}", std::process::id()));
    fs::create_dir(&path).map_err(|e| format!("create {}: {e}", path.display()))?;
    Ok(path)
}

fn new_root(base: &Path, label: &str) -> Result<PathBuf, String> {
    let parent = fresh_dir(base, label)?;
    let instance = store::create_instance(&parent).map_err(|e| format!("create Instance: {e}"))?;
    let root = instance.root().to_path_buf();
    instance.close().map_err(|e| format!("close Instance: {e}"))?;
    Ok(root)
}

fn describe_instance_result(result: &Result<Instance, InstanceError>) -> String {
    match result {
        Ok(_) => "Ok(Instance)".to_owned(),
        Err(error) => format!("Err({error:?})"),
    }
}

fn expect_incomplete(root: &Path, reason: IncompleteReason) -> Result<(), String> {
    match store::open_instance(root) {
        Err(InstanceError::Incomplete(found)) if found == reason => Ok(()),
        other => Err(format!("expected Incomplete({reason:?}), got {}", describe_instance_result(&other))),
    }
}

fn root_snapshot(root: &Path) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut result = BTreeMap::new();
    for item in fs::read_dir(root).map_err(|e| e.to_string())? {
        let item = item.map_err(|e| e.to_string())?;
        let name = item.file_name().to_string_lossy().into_owned();
        let meta = fs::symlink_metadata(item.path()).map_err(|e| e.to_string())?;
        let bytes = if meta.file_type().is_symlink() {
            #[cfg(unix)]
            { std::os::unix::ffi::OsStrExt::as_bytes(fs::read_link(item.path()).map_err(|e| e.to_string())?.as_os_str()).to_vec() }
            #[cfg(windows)]
            { fs::read_link(item.path()).map_err(|e| e.to_string())?.to_string_lossy().as_bytes().to_vec() }
        } else if meta.is_file() {
            fs::read(item.path()).map_err(|e| e.to_string())?
        } else {
            Vec::new()
        };
        result.insert(name, bytes);
    }
    Ok(result)
}

fn db_path(root: &Path) -> PathBuf { root.join("instance.db") }
fn side_path(root: &Path, suffix: &str) -> PathBuf { root.join(format!("instance.db-{suffix}")) }

fn read_optional_file(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn edit_db(root: &Path, edit: impl FnOnce(&mut Vec<u8>)) -> Result<(), String> {
    let path = db_path(root);
    let mut bytes = fs::read(&path).map_err(|e| e.to_string())?;
    edit(&mut bytes);
    fs::write(path, bytes).map_err(|e| e.to_string())
}

fn set_header_u32(root: &Path, offset: usize, value: u32) -> Result<(), String> {
    edit_db(root, |bytes| bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes()))
}

fn child_command(mode: &str, args: &[&Path]) -> Result<Command, String> {
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut command = Command::new(executable);
    command.arg("__child").arg(mode);
    for arg in args { command.arg(arg); }
    Ok(command)
}

fn wait_for_marker(child: &mut Child, path: &Path) -> Result<(), String> {
    for _ in 0..500 {
        if path.exists() { return Ok(()); }
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Err(format!("child exited before writing its marker: {status}"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err(format!("child did not write its marker at {}", path.display()))
}

fn start_owner_process(root: &Path, marker: &Path) -> Result<(Child, std::process::ChildStdin), String> {
    let mut command = child_command("hold", &[root, marker])?;
    command.stdin(Stdio::piped());
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let control = child.stdin.take().ok_or("owner process stdin unavailable")?;
    if let Err(error) = wait_for_marker(&mut child, marker) {
        drop(control);
        let _ = child.wait();
        return Err(error);
    }
    Ok((child, control))
}

fn end_owner_process(child: &mut Child, mut control: std::process::ChildStdin) -> Result<(), String> {
    let request = control.write_all(b"exit\n").and_then(|_| control.flush());
    drop(control);
    let status = child.wait().map_err(|e| e.to_string())?;
    request.map_err(|e| e.to_string())?;
    if status.success() { Ok(()) } else { Err(format!("owner process exit status={status}")) }
}

fn child_instance_roots(parent: &Path, before: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let mut roots = Vec::new();
    for item in fs::read_dir(parent).map_err(|e| e.to_string())? {
        let path = item.map_err(|e| e.to_string())?.path();
        if path.is_dir() && !before.contains(&path) { roots.push(path); }
    }
    Ok(roots)
}

fn child_dirs(parent: &Path) -> Result<Vec<PathBuf>, String> {
    Ok(fs::read_dir(parent).map_err(|e| e.to_string())?
        .filter_map(Result::ok).map(|item| item.path()).collect())
}

fn case_create_shape(base: &Path) -> Result<String, String> {
    let parent = fresh_dir(base, "ic1")?;
    let instance = store::create_instance(&parent).map_err(|e| e.to_string())?;
    let root = instance.root().to_path_buf();
    let entries: Vec<String> = fs::read_dir(&root).map_err(|e| e.to_string())?
        .map(|item| item.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<Result<_, _>>().map_err(|e| e.to_string())?;
    let mut expected = vec!["instance.db".to_owned(), "instance.id".to_owned(), "instance.lock".to_owned()];
    let mut actual = entries;
    for name in ["instance.db-shm", "instance.db-wal"] {
        if actual.iter().any(|entry| entry == name) { expected.push(name.to_owned()); }
    }
    expected.sort(); actual.sort();
    let identity = fs::read_to_string(root.join("instance.id")).map_err(|e| e.to_string())?;
    let version = store::test_api::user_version(&instance).map_err(|e| e.to_string())?;
    let objects = store::test_api::schema_objects(&instance).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    let private_root = {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(&root).map_err(|e| e.to_string())?.permissions().mode() & 0o777 == 0o700
    };
    #[cfg(not(unix))]
    let private_root = true;
    let good = root.file_name().and_then(|name| name.to_str()) == Some(instance.id().to_hex().as_str())
        && actual == expected
        && identity.lines().count() == 3
        && identity.starts_with("yokai-instance format 1\nid ")
        && version == 2 && objects == 11 && private_root && instance.is_locked();
    instance.close().map_err(|e| e.to_string())?;
    if good { Ok(format!("root={} entries={actual:?} version={version} objects={objects}", root.display())) }
    else { Err(format!("root={} entries={actual:?} expected={expected:?} version={version} objects={objects} private={private_root}", root.display())) }
}

fn case_invalid_parents(base: &Path) -> Result<String, String> {
    let dir = fresh_dir(base, "ic2")?;
    let file = dir.join("file"); fs::write(&file, b"fixed").map_err(|e| e.to_string())?;
    let missing = dir.join("missing");
    let relative = PathBuf::from(format!(
        ".yokai-instance-relative-parent-{}",
        std::process::id()
    ));
    let mut outcomes = Vec::new();
    for path in [relative.clone(), missing.clone(), file.clone()] {
        outcomes.push(matches!(store::create_instance(&path), Err(InstanceError::ParentInvalid(_))));
    }
    let link = dir.join("parent-link");
    symlink_dir(&dir, &link)?;
    outcomes.push(matches!(store::create_instance(&link), Err(InstanceError::ParentInvalid(_))));
    let unchanged = fs::read(&file).map_err(|e| e.to_string())? == b"fixed"
        && !missing.exists() && !relative.exists();
    if outcomes.iter().all(|value| *value) && unchanged {
        Ok("relative, missing, regular-file, and final-link parents refused".into())
    } else { Err(format!("outcomes={outcomes:?} unchanged={unchanged}")) }
}

fn case_existing_roots(base: &Path) -> Result<String, String> {
    let mut accepted = true;
    for (index, kind) in ["directory", "file", "dangling-link"].iter().enumerate() {
        let parent = fresh_dir(base, "ic3")?;
        let id = [0x53 + index as u8; 16];
        store::test_api::set_next_identity(id).map_err(|e| e.to_string())?;
        let target = parent.join(InstanceId::from_bytes(id).to_hex());
        match *kind {
            "directory" => fs::create_dir(&target).map_err(|e| e.to_string())?,
            "file" => fs::write(&target, b"preserve").map_err(|e| e.to_string())?,
            _ => symlink_file(&parent.join("absent-target"), &target)?,
        }
        let before = fs::symlink_metadata(&target).map_err(|e| e.to_string())?;
        let result = store::create_instance(&parent);
        let after = fs::symlink_metadata(&target).map_err(|e| e.to_string())?;
        accepted &= matches!(result, Err(InstanceError::AlreadyExists(_)))
            && before.file_type() == after.file_type() && before.len() == after.len();
        store::test_api::clear();
    }
    if accepted { Ok("directory, file, and dangling-link collisions preserved".into()) }
    else { Err("an existing root entry was modified or accepted".into()) }
}

fn case_concurrent_creators(base: &Path) -> Result<String, String> {
    let parent = fresh_dir(base, "ic4")?;
    let barrier = Arc::new(Barrier::new(16));
    let outcomes = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..16).map(|_| {
            let barrier = barrier.clone();
            let parent = parent.clone();
            scope.spawn(move || {
                barrier.wait();
                store::create_instance(&parent).and_then(Instance::close)
            })
        }).collect();
        handles.into_iter().map(|handle| handle.join().unwrap_or_else(|_| Err(InstanceError::Io("thread panicked".into()))))
            .collect::<Vec<_>>()
    });
    if outcomes.iter().any(Result::is_err) { return Err(format!("thread creation outcomes={outcomes:?}")); }
    for _ in 0..4 {
        let status = child_command("create", &[&parent])?.status().map_err(|e| e.to_string())?;
        if !status.success() { return Err(format!("child creator status={status}")); }
    }
    let roots: Vec<PathBuf> = fs::read_dir(&parent).map_err(|e| e.to_string())?
        .filter_map(Result::ok).map(|item| item.path()).filter(|path| path.is_dir()).collect();
    let mut openable = 0;
    for root in &roots {
        if let Ok(instance) = store::open_instance(root) {
            openable += 1;
            instance.close().map_err(|e| e.to_string())?;
        }
    }
    if roots.len() == 20 && openable == 20 { Ok("16 threads and 4 processes created 20 openable roots".into()) }
    else { Err(format!("roots={} openable={openable}", roots.len())) }
}

fn case_same_identity_creators(base: &Path) -> Result<String, String> {
    let parent = fresh_dir(base, "ic5")?;
    let id = [0x5a; 16];
    store::test_api::set_next_identity(id).map_err(|e| e.to_string())?;
    let barrier = Arc::new(Barrier::new(8));
    let outcomes = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8).map(|_| {
            let barrier = barrier.clone();
            let parent = parent.clone();
            scope.spawn(move || {
                barrier.wait();
                match store::create_instance(&parent) {
                    Ok(instance) => instance.close().map(|_| "created"),
                    Err(InstanceError::AlreadyExists(_)) => Ok("already-exists"),
                    Err(error) => Err(error),
                }
            })
        }).collect();
        handles.into_iter().map(|handle| handle.join().unwrap_or_else(|_| Err(InstanceError::Io("thread panicked".into()))))
            .collect::<Vec<_>>()
    });
    store::test_api::clear();
    let created = outcomes.iter().filter(|result| matches!(result, Ok("created"))).count();
    let refused = outcomes.iter().filter(|result| matches!(result, Ok("already-exists"))).count();
    let root = parent.join(InstanceId::from_bytes(id).to_hex());
    let opens = store::open_instance(&root).and_then(Instance::close).is_ok();
    if created == 1 && refused == 7 && opens { Ok("one of eight same-identity creators won; winner reopened".into()) }
    else { Err(format!("outcomes={outcomes:?} reopened={opens}")) }
}

fn case_creation_process_exits(base: &Path) -> Result<String, String> {
    for boundary in ["after-root", "after-lock", "after-db", "after-migration", "during-identity"] {
        for round in 0..3 {
            let parent = fresh_dir(base, "ic6")?;
            let before_dirs = child_dirs(&parent)?;
            let mut command = child_command("failpoint", &[&parent])?;
            command.env("YOKAI_TEST_FAILPOINT", format!("{boundary}:exit"));
            let status = command.status().map_err(|e| e.to_string())?;
            if status.code() != Some(86) {
                return Err(format!("{boundary} exit round {round}: {status}"));
            }
            let roots = child_instance_roots(&parent, &before_dirs)?;
            if roots.len() != 1 { return Err(format!("{boundary} produced {} roots", roots.len())); }
            let root = &roots[0];
            let before = root_snapshot(root)?;
            let reason = match boundary {
                "after-root" => IncompleteReason::NoLockFile,
                "during-identity" => IncompleteReason::BadIdentityFile,
                _ => IncompleteReason::NoIdentityFile,
            };
            expect_incomplete(root, reason)?;
            if before != root_snapshot(root)? {
                return Err(format!("open changed incomplete root at {boundary}"));
            }
        }
    }
    Ok("process exited at five creation boundaries, including committed V1 before identity, three rounds each".into())
}

fn case_directory_flushes(base: &Path) -> Result<String, String> {
    store::test_api::clear();
    let parent = fs::canonicalize(fresh_dir(base, "ic8")?).map_err(|e| e.to_string())?;
    let instance = store::create_instance(&parent).map_err(|e| e.to_string())?;
    let root = instance.root().to_path_buf();
    let events = store::test_api::directory_flush_observations();
    instance.close().map_err(|e| e.to_string())?;
    let performed = cfg!(unix);
    let root_seen = events.iter().any(|(path, did_flush)| path == &root && *did_flush == performed);
    let parent_seen = events.iter().any(|(path, did_flush)| path == &parent && *did_flush == performed);
    if root_seen && parent_seen {
        Ok(format!("root and parent flush calls observed; sync_all_performed={performed}; power-loss durability untested"))
    } else { Err(format!("flush observations={events:?}")) }
}

fn case_incomplete_untouched(base: &Path) -> Result<String, String> {
    let parent = fresh_dir(base, "ic9")?;
    let before_dirs = child_dirs(&parent)?;
    let mut command = child_command("failpoint", &[&parent])?;
    command.env("YOKAI_TEST_FAILPOINT", "after-root:exit");
    let status = command.status().map_err(|e| e.to_string())?;
    let roots = child_instance_roots(&parent, &before_dirs)?;
    if status.code() != Some(86) || roots.len() != 1 { return Err("failed to construct incomplete root".into()); }
    let before = root_snapshot(&roots[0])?;
    for _ in 0..5 { expect_incomplete(&roots[0], IncompleteReason::NoLockFile)?; }
    if before == root_snapshot(&roots[0])? {
        let instance_source = include_str!("../src/instance.rs");
        let platform_source = include_str!("../../yokai-platform/src/location.rs");
        if ["remove_file(", "remove_dir(", "rename("].iter().any(|word| instance_source.contains(word) || platform_source.contains(word)) {
            return Err("a discard-like path operation appeared in reviewed source".into());
        }
        Ok("five repeated opens preserved the incomplete root; source scan found no discard calls".into())
    } else { Err("repeated opens changed the incomplete root".into()) }
}

fn case_fixture_instance(base: &Path) -> Result<String, String> {
    let root = new_root(base, "ic-fixture")?;
    let db = fs::read(db_path(&root)).map_err(|e| e.to_string())?;
    let hash = store::test_api::sha256_chunks_for_test(&[&db]);
    println!("INFO IC-F fixture={} instance.db_sha256={hash}", root.display());
    Ok(format!("current version-2 fixture preserved; historical accepted version-0 fixture remains separate; sha256={hash}"))
}

fn case_same_process_busy(base: &Path) -> Result<String, String> {
    let root = new_root(base, "il1")?;
    let owner = store::open_instance(&root).map_err(|e| e.to_string())?;
    // Successful startup commits an epoch. Measure only the refused second open, with the
    // writer paused; the Windows byte-range-locked instance.lock cannot be read while owned.
    let pause = store::test_api::block_writer(&owner).map_err(|e|e.to_string())?;
    let protected = || ["instance.db","instance.db-wal","instance.id"].iter()
        .map(|name|read_optional_file(&root.join(name))).collect::<Result<Vec<_>,_>>();
    let before = protected()?;
    let busy = matches!(store::open_instance(&root), Err(InstanceError::Busy(_)));
    let unchanged = before == protected()?;
    drop(pause);
    owner.close().map_err(|e| e.to_string())?;
    if busy && unchanged { Ok("second same-process open was Busy; database/WAL/identity bytes unchanged across refusal".into()) }
    else { Err(format!("busy={busy} unchanged={unchanged}")) }
}

fn case_other_process_busy(base: &Path) -> Result<String, String> {
    let root = new_root(base, "il2")?;
    let owner = store::open_instance(&root).map_err(|e| e.to_string())?;
    let status = child_command("probe", &[&root])?.status().map_err(|e| e.to_string())?;
    owner.close().map_err(|e| e.to_string())?;
    if status.code() == Some(20) { Ok("second process observed Busy".into()) }
    else { Err(format!("probe status={status}")) }
}

fn case_owner_process_death(base: &Path) -> Result<String, String> {
    let root = new_root(base, "il3")?;
    let marker = fresh_dir(base, "il3-owner")?.join("owner-locked");
    let (mut child, control) = start_owner_process(&root, &marker)?;
    let busy = matches!(store::open_instance(&root), Err(InstanceError::Busy(_)));
    end_owner_process(&mut child, control)?;
    let reopened = store::open_instance(&root).map_err(|e| e.to_string())?;
    reopened.close().map_err(|e| e.to_string())?;
    if busy {
        Ok("competing open was Busy while owner lived; open succeeded after owner process exit".into())
    } else { Err("competing open was not Busy while owner lived".into()) }
}

fn case_open_aliases(base: &Path) -> Result<String, String> {
    let root = new_root(base, "il4")?;
    let parent = root.parent().ok_or("root has no parent")?;
    let link = parent.join("root-link");
    symlink_dir(&root, &link)?;
    let linked = matches!(store::open_instance(&link), Err(InstanceError::PathInvalid(_)));
    let dotted = root.join("..").join(root.file_name().ok_or("no name")?);
    let owner = store::open_instance(&root).map_err(|e| e.to_string())?;
    let alias_busy = matches!(store::open_instance(&dotted), Err(InstanceError::Busy(_)));
    owner.close().map_err(|e| e.to_string())?;
    if linked && alias_busy { Ok("root symlink refused; dot-dot alias did not create a second owner".into()) }
    else { Err(format!("linked={linked} dotdot_busy={alias_busy}")) }
}

fn case_open_creates_nothing(base: &Path) -> Result<String, String> {
    let mut unchanged = true;
    for kind in ["missing-lock", "missing-id", "empty"] {
        let root = if kind == "empty" {
            let parent = fresh_dir(base, "il5")?;
            let root = parent.join("empty-root"); fs::create_dir(&root).map_err(|e| e.to_string())?; root
        } else {
            let root = new_root(base, "il5")?;
            let path = root.join(if kind == "missing-lock" { "instance.lock" } else { "instance.id" });
            fs::remove_file(path).map_err(|e| e.to_string())?;
            root
        };
        let before = root_snapshot(&root)?;
        let _ = store::open_instance(&root);
        unchanged &= before == root_snapshot(&root)?;
    }
    if unchanged { Ok("missing lock, missing identity, and empty root stayed unchanged".into()) }
    else { Err("open_instance created or changed a file on an incomplete root".into()) }
}

fn case_lock_file_removal_limit(base: &Path) -> Result<String, String> {
    let root = new_root(base, "il6")?;
    let owner = store::open_instance(&root).map_err(|e| e.to_string())?;
    let lock = root.join("instance.lock");
    match fs::remove_file(&lock) {
        Ok(()) => {
            let absent = expect_incomplete(&root, IncompleteReason::NoLockFile).is_ok();
            OpenOptions::new().write(true).create_new(true).open(&lock).map_err(|e| e.to_string())?;
            let second = store::open_instance(&root).and_then(Instance::close).is_ok();
            owner.close().map_err(|e| e.to_string())?;
            if absent && second { Ok("Linux same-user unlink limitation reproduced; replacement can be locked".into()) }
            else { Err(format!("absent_refusal={absent} replacement_lock={second}")) }
        }
        Err(error) => {
            owner.close().map_err(|e| e.to_string())?;
            #[cfg(windows)]
            { if lock.exists() { Ok(format!("open handle prevented deletion: {error}")) } else { Err("lock disappeared".into()) } }
            #[cfg(not(windows))]
            { Err(format!("unexpected lock deletion failure: {error}")) }
        }
    }
}

fn case_close_order(base: &Path) -> Result<String, String> {
    let root = new_root(base, "il7")?;
    let owner = store::open_instance(&root).map_err(|e| e.to_string())?;
    let busy = matches!(store::open_instance(&root), Err(InstanceError::Busy(_)));
    store::test_api::clear();
    owner.close().map_err(|e| e.to_string())?;
    let events = store::test_api::connection_events();
    let reopened = store::open_instance(&root).and_then(Instance::close).is_ok();
    if busy && events == ["connection-closed", "lock-released"] && reopened {
        Ok("SQLite close precedes lock release; Busy while held and reopen after release".into())
    } else { Err(format!("busy={busy} close_events={events:?} reopened={reopened}")) }
}

#[cfg(unix)]
fn resource_counts() -> Result<(usize, usize), String> {
    fn count(path: &str) -> std::io::Result<usize> {
        fs::read_dir(path)?.try_fold(0, |n, entry| entry.map(|_| n + 1))
    }
    let counts = (count("/proc/self/fd").map_err(|e| e.to_string())?,
                  count("/proc/self/task").map_err(|e| e.to_string())?);
    if counts.0 == 0 || counts.1 == 0 { return Err("resource counter returned zero".into()); }
    Ok(counts)
}
#[cfg(windows)]
fn resource_counts() -> Result<(usize, usize), String> {
    windows_process::sample().map_err(|e| format!("native resource count: {e}"))
}
#[cfg(not(any(unix, windows)))]
fn resource_counts() -> Result<(usize, usize), String> { Err("resource counters unsupported".into()) }

fn cycle_once(root: &Path) -> Result<(), String> {
    store::open_instance(root).map_err(|e| e.to_string())?.close().map_err(|e| e.to_string())
}
fn child_cycles(root: &Path) -> Result<(), String> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut input = stdin.lock().lines();
    let mut out = stdout.lock();
    cycle_once(root)?; // Explicit warmup excluded from the measured100 cycles.
    let before = resource_counts()?;
    writeln!(out, "RESOURCE baseline {} {} {}", std::process::id(), before.0, before.1).map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())?;
    for _ in 0..100 {
        if input.next().transpose().map_err(|e| e.to_string())?.as_deref() != Some("cycle") {
            return Err("missing child cycle command".into());
        }
        cycle_once(root)?;
        writeln!(out, "done").map_err(|e| e.to_string())?;
        out.flush().map_err(|e| e.to_string())?;
    }
    if input.next().transpose().map_err(|e| e.to_string())?.as_deref() != Some("finish") {
        return Err("missing child finish command".into());
    }
    let after = resource_counts()?;
    if before != after { return Err(format!("child resources={before:?}->{after:?}")); }
    writeln!(out, "RESOURCE final {} {} {}", std::process::id(), after.0, after.1).map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())
}
fn read_cycle_counts(input: &mut impl BufRead, phase: &str, pid: u32) -> Result<(usize, usize), String> {
    let mut line = String::new();
    input.read_line(&mut line).map_err(|e| e.to_string())?;
    let fields: Vec<_> = line.split_whitespace().collect();
    if fields.len() != 5 || fields[0] != "RESOURCE" || fields[1] != phase
        || fields[2].parse::<u32>().ok() != Some(pid) { return Err(format!("invalid resource protocol: {line:?}")); }
    let counts = (fields[3].parse::<usize>().map_err(|e| e.to_string())?, fields[4].parse::<usize>().map_err(|e| e.to_string())?);
    if counts.0 == 0 || counts.1 == 0 { return Err("child resource counter returned zero".into()); }
    Ok(counts)
}

fn case_repeated_open_close(base: &Path) -> Result<String, String> {
    struct CycleChild(Child);
    impl Drop for CycleChild {
        fn drop(&mut self) {
            if !matches!(self.0.try_wait(), Ok(Some(_))) { let _ = self.0.kill(); let _ = self.0.wait(); }
        }
    }
    let root = new_root(base, "il8")?;
    cycle_once(&root)?;
    let before = resource_counts()?;
    let mut command = child_command("cycles", &[&root])?;
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
    let mut child = CycleChild(command.spawn().map_err(|e| e.to_string())?);
    let pid = child.0.id();
    let mut input = child.0.stdin.take().ok_or("child stdin unavailable")?;
    let output = child.0.stdout.take().ok_or("child stdout unavailable")?;
    let mut lines = BufReader::new(output);
    let mut dumpable_line = String::new();
    lines.read_line(&mut dumpable_line).map_err(|e| e.to_string())?;
    if !dumpable_line.starts_with("DUMPABLE") { return Err(format!("child dumpability line={dumpable_line:?}")); }
    let child_before = read_cycle_counts(&mut lines, "baseline", pid)?;
    for _ in 0..100 {
        cycle_once(&root)?;
        input.write_all(b"cycle\n").map_err(|e| e.to_string())?;
        input.flush().map_err(|e| e.to_string())?;
        let mut response = String::new();
        lines.read_line(&mut response).map_err(|e| e.to_string())?;
        if response.trim() != "done" { return Err(format!("child cycle response={response:?}")); }
    }
    input.write_all(b"finish\n").map_err(|e| e.to_string())?;
    input.flush().map_err(|e| e.to_string())?;
    let child_after = read_cycle_counts(&mut lines, "final", pid)?;
    drop(input);
    let status = child.0.wait().map_err(|e| e.to_string())?;
    drop(lines);
    drop(child);
    let after = resource_counts()?;
    println!("INFO IL-8 resources actor=parent pid={} before_handles_or_fds={} before_threads={} after_handles_or_fds={} after_threads={} warmup=1 cycles=100", std::process::id(), before.0, before.1, after.0, after.1);
    println!("INFO IL-8 resources actor=child pid={pid} before_handles_or_fds={} before_threads={} after_handles_or_fds={} after_threads={} warmup=1 cycles=100", child_before.0, child_before.1, child_after.0, child_after.1);
    if status.success() && before == after && child_before == child_after {
        Ok("200 alternating measured open/close cycles after warmup; both processes' native resource counts stable".into())
    } else { Err(format!("child={status} parent={before:?}->{after:?} child_resources={child_before:?}->{child_after:?}")) }
}

#[cfg(unix)]
fn file_identity(path: &Path) -> Result<(u64, u64, u64), String> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(path).map_err(|e| e.to_string())?;
    Ok((meta.dev(), meta.ino(), meta.len()))
}

#[cfg(windows)]
#[repr(C)]
struct ByHandleFileInformation {
    _file_attributes: u32,
    _creation_time: [u32; 2],
    _last_access_time: [u32; 2],
    _last_write_time: [u32; 2],
    volume_serial_number: u32,
    _file_size_high: u32,
    _file_size_low: u32,
    _number_of_links: u32,
    file_index_high: u32,
    file_index_low: u32,
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetFileInformationByHandle(
        file: *mut std::ffi::c_void,
        information: *mut ByHandleFileInformation,
    ) -> i32;
}

#[cfg(windows)]
fn file_identity(path: &Path) -> Result<(u64, u64, u64), String> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;

    let file = fs::File::open(path).map_err(|e| e.to_string())?;
    let mut information = MaybeUninit::<ByHandleFileInformation>::uninit();
    let succeeded = unsafe {
        GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr())
    };
    if succeeded == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let information = unsafe { information.assume_init() };
    let length = file.metadata().map_err(|e| e.to_string())?.len();
    let index = (u64::from(information.file_index_high) << 32)
        | u64::from(information.file_index_low);
    Ok((u64::from(information.volume_serial_number), index, length))
}

#[cfg(not(any(unix, windows)))]
fn file_identity(path: &Path) -> Result<(u64, u64, u64), String> {
    let meta = fs::metadata(path).map_err(|e| e.to_string())?;
    Ok((0, meta.len(), meta.len()))
}

fn case_lock_file_persists(base: &Path) -> Result<String, String> {
    let root = new_root(base, "il10")?;
    let lock = root.join("instance.lock");
    let original = file_identity(&lock)?;
    let instance = store::open_instance(&root).map_err(|e| e.to_string())?;
    instance.close().map_err(|e| e.to_string())?;
    if file_identity(&lock)? != original { return Err("lock identity changed after orderly close".into()); }
    let marker = fresh_dir(base, "il10-owner")?.join("owner-locked");
    let (mut child, control) = start_owner_process(&root, &marker)?;
    end_owner_process(&mut child, control)?;
    if file_identity(&lock)? == original && fs::metadata(&lock).map_err(|e| e.to_string())?.len() == 0 {
        Ok("persistent zero-length lock kept its file identity after close and owner death".into())
    } else { Err("lock file identity or size changed".into()) }
}

fn case_root_symlink(base: &Path) -> Result<String, String> {
    let root = new_root(base, "ip1")?;
    let link = root.parent().ok_or("missing parent")?.join("instance-root-link");
    symlink_dir(&root, &link)?;
    let before = root_snapshot(&root)?;
    let refused = matches!(store::open_instance(&link), Err(InstanceError::PathInvalid(_)));
    if refused && before == root_snapshot(&root)? { Ok("root symlink refused; target unchanged".into()) }
    else { Err(format!("refused={refused}")) }
}

fn case_entry_symlinks(base: &Path) -> Result<String, String> {
    for name in ["instance.db", "instance.lock", "instance.id"] {
        let root = new_root(base, "ip2")?;
        let outside = root.parent().ok_or("missing parent")?.join(format!("outside-{name}"));
        fs::write(&outside, b"outside-preserved").map_err(|e| e.to_string())?;
        let target = root.join(name);
        fs::remove_file(&target).map_err(|e| e.to_string())?;
        symlink_file(&outside, &target)?;
        let refused = matches!(store::open_instance(&root), Err(InstanceError::PathInvalid(_)));
        if !refused || fs::read(&outside).map_err(|e| e.to_string())? != b"outside-preserved" {
            return Err(format!("{} symlink was accepted or target changed", target.display()));
        }
    }
    let mut hard_links = 0;
    for name in ["instance.lock", "instance.id", "instance.db"] {
        let root = new_root(base, "ip2-hardlink")?;
        let alias = root.parent().ok_or("missing parent")?.join(format!("outside-hardlink-{name}"));
        fs::hard_link(root.join(name), &alias).map_err(|e| format!("create {name} hard link: {e}"))?;
        let outside_before = fs::read(&alias).map_err(|e| e.to_string())?;
        let refused = matches!(store::open_instance(&root), Err(InstanceError::PathInvalid(_)));
        let outside_unchanged = fs::read(&alias).map_err(|e| e.to_string())? == outside_before;
        if !refused || !outside_unchanged {
            return Err(format!("{} hard link was accepted or outside bytes changed", root.join(name).display()));
        }
        hard_links += 1;
    }
    Ok(format!("database, lock, and identity symlinks refused; outside bytes unchanged; {hard_links} hard-linked entries refused"))
}

fn case_symlinked_parent(base: &Path) -> Result<String, String> {
    let real = fresh_dir(base, "ip3-real")?;
    let real_parent = real.join("parent");
    fs::create_dir(&real_parent).map_err(|e| e.to_string())?;
    let alias = base.join(format!("ip3-alias-{}", NEXT_DIR.fetch_add(1, Ordering::Relaxed)));
    symlink_dir(&real, &alias)?;
    let aliased_parent = alias.join("parent");
    let instance = store::create_instance(&aliased_parent).map_err(|e| e.to_string())?;
    let canonical = fs::canonicalize(&real_parent).map_err(|e| e.to_string())?;
    let good = instance.root().parent() == Some(canonical.as_path());
    instance.close().map_err(|e| e.to_string())?;
    if good { Ok(format!("symlinked ancestor accepted; canonical parent={}", canonical.display())) }
    else { Err("created root did not use canonical parent".into()) }
}

fn case_location_policy(base: &Path) -> Result<String, String> {
    let allowed = new_root(base, "ld1")?;
    let mut detail = format!("test-data parent accepted at {}", allowed.display());
    #[cfg(feature = "test-api")]
    {
        let extended_drive = r"\\?\C:\ProgramData\Yokai";
        let drive = r"C:\ProgramData\Yokai";
        let unc = r"\\server\share\Yokai";
        let extended_unc = r"\\?\UNC\server\share\Yokai";
        if yokai_platform::location::windows_path_is_network_for_test(extended_drive)
            || yokai_platform::location::windows_path_is_network_for_test(drive)
            || !yokai_platform::location::windows_path_is_network_for_test(unc)
            || !yokai_platform::location::windows_path_is_network_for_test(extended_unc)
        {
            return Err("Windows drive and UNC path prefixes were classified incorrectly".into());
        }
        detail.push_str("; Windows extended drive path distinguished from UNC");
    }
    #[cfg(target_os = "linux")]
    if Path::new("/dev/shm").is_dir() {
        let tmp = fresh_dir(Path::new("/dev/shm"), "yokai-ld1")?;
        let refused = matches!(store::create_instance(&tmp), Err(InstanceError::UnsupportedLocation(_)));
        if !refused { return Err("unregistered tmpfs parent was not refused".into()); }
        detail.push_str("; unregistered tmpfs refused");
    }
    Ok(detail)
}

fn case_mountinfo_fixtures() -> Result<String, String> {
    #[cfg(target_os = "linux")]
    {
        let mountinfo = concat!(
            "30 1 8:1 / /fixture rw,relatime - ext4 /dev/a rw\n",
            "31 30 0:42 / /fixture/nested\\040mount rw - nfs4 server:/share rw\n",
            "32 30 0:43 / /fixture/escaped\\040space rw - cifs //server/share rw\n",
        );
        let nested = yokai_platform::location::mount_type_for_test(Path::new("/fixture/nested mount/file"), mountinfo)
            .map_err(|e| e.to_string())?;
        let escaped = yokai_platform::location::mount_type_for_test(Path::new("/fixture/escaped space/file"), mountinfo)
            .map_err(|e| e.to_string())?;
        if nested == "nfs4" && escaped == "cifs" {
            Ok("longest mount prefix and escaped-space mount fields parsed".into())
        } else { Err(format!("nested={nested} escaped={escaped}")) }
    }
    #[cfg(not(target_os = "linux"))]
    { Ok("Linux mountinfo parser is not applicable on this target".into()) }
}

fn case_tmpfs_test_override(base: &Path) -> Result<String, String> {
    let _ = base;
    #[cfg(target_os = "linux")]
    if Path::new("/dev/shm").is_dir() {
        let allowed = fresh_dir(Path::new("/dev/shm"), "yokai-ld5-allowed")?;
        store::test_api::allow_tmpfs_for_test(&allowed).map_err(|e| e.to_string())?;
        let instance = store::create_instance(&allowed).map_err(|e| e.to_string())?;
        instance.close().map_err(|e| e.to_string())?;
        let outside = fresh_dir(Path::new("/dev/shm"), "yokai-ld5-outside")?;
        if !matches!(store::create_instance(&outside), Err(InstanceError::UnsupportedLocation(_))) {
            return Err("tmpfs outside the registered test root was accepted".into());
        }
        return Ok("registered tmpfs descendant accepted; sibling tmpfs refused".into());
    }
    Ok("tmpfs unavailable; test-only registration remains cfg-gated".into())
}

fn case_pf1(base: &Path) -> Result<String, String> {
    let root = new_root(base, "pf1")?;
    if side_path(&root, "wal").exists() || side_path(&root, "shm").exists() || side_path(&root, "journal").exists() {
        return Err("new version-2 fixture unexpectedly retained side files".into());
    }
    store::test_api::clear();
    let instance = store::open_instance(&root).map_err(|e| e.to_string())?;
    let events = store::test_api::connection_events();
    let version = store::test_api::user_version(&instance).map_err(|e| e.to_string())?;
    let objects = store::test_api::schema_objects(&instance).map_err(|e| e.to_string())?;
    instance.close().map_err(|e| e.to_string())?;
    if events == ["stage-b-read-write"] && version == 2 && objects == 11 {
        Ok("raw-header Stage A opened no SQLite connection; Stage B accepted current version 2".into())
    } else { Err(format!("events={events:?} version={version} objects={objects}")) }
}

fn case_pf2a(base: &Path) -> Result<String, String> {
    let root = new_root(base, "pf2a")?;
    set_header_u32(&root, 60, 3)?;
    let before = root_snapshot(&root)?;
    store::test_api::clear();
    let result = store::open_instance(&root);
    let events = store::test_api::connection_events();
    if matches!(&result, Err(InstanceError::SchemaTooNew { found: 3, maximum: 2 }))
        && events.is_empty() && before == root_snapshot(&root)? {
        Ok("future raw-header version refused without SQLite or inventory changes".into())
    } else { Err(format!("result={} events={events:?}", describe_instance_result(&result))) }
}

fn case_pf2b(base: &Path) -> Result<String, String> {
    let root = new_root(base, "pf2b")?;
    set_header_u32(&root, 60, 2)?;
    fs::write(side_path(&root, "shm"), b"stale").map_err(|e| e.to_string())?;
    let before = root_snapshot(&root)?;
    store::test_api::clear();
    let result = store::open_instance(&root);
    let events = store::test_api::connection_events();
    if matches!(&result, Err(InstanceError::SideFileStateUnverified { header_version: 2 }))
        && events.is_empty() && before == root_snapshot(&root)? {
        Ok("stale SHM without WAL refused before SQLite; inventory unchanged".into())
    } else { Err(format!("result={} events={events:?}", describe_instance_result(&result))) }
}

fn make_wal_future(root: &Path) -> Result<(), String> {
    let mut command = child_command("wal-version", &[&db_path(root)])?;
    let status = command.status().map_err(|e| e.to_string())?;
    if status.success() && side_path(root, "wal").exists() { Ok(()) }
    else { Err(format!("WAL fixture child status={status}")) }
}

fn case_pf3(base: &Path, keep_shm: bool) -> Result<String, String> {
    let root = new_root(base, if keep_shm { "pf3b" } else { "pf3a" })?;
    make_wal_future(&root)?;
    if !keep_shm && side_path(&root, "shm").exists() {
        fs::remove_file(side_path(&root, "shm")).map_err(|e| e.to_string())?;
    }
    let db_before = fs::read(db_path(&root)).map_err(|e| e.to_string())?;
    let wal_before = fs::read(side_path(&root, "wal")).map_err(|e| e.to_string())?;
    store::test_api::clear();
    let result = store::open_instance(&root);
    let events = store::test_api::connection_events();
    let db_same = fs::read(db_path(&root)).map_err(|e| e.to_string())? == db_before;
    let wal_same = fs::read(side_path(&root, "wal")).map_err(|e| e.to_string())? == wal_before;
    match result {
        Err(InstanceError::SchemaTooNew { found: 3, maximum: 2 }) if db_same && wal_same
            && events.iter().any(|event| *event == "stage-a-read-only") =>
            Ok(format!("WAL-only version was read; main and WAL preserved; shm_present={}", side_path(&root, "shm").exists())),
        other => Err(format!("result={} db_same={db_same} wal_same={wal_same} events={events:?}", describe_instance_result(&other))),
    }
}

fn case_pf4(base: &Path) -> Result<String, String> {
    let root = new_root(base, "pf4")?;
    edit_db(&root, |bytes| { bytes[18] = 1; bytes[19] = 1; })?;
    let before = root_snapshot(&root)?;
    store::test_api::clear();
    let result = store::open_instance(&root);
    if matches!(&result, Err(InstanceError::UnexpectedJournal))
        && store::test_api::connection_events().is_empty() && before == root_snapshot(&root)? {
        Ok("rollback-journal header refused without a connection".into())
    } else { Err(format!("result={}", describe_instance_result(&result))) }
}

fn case_pf5(base: &Path) -> Result<String, String> {
    for wal in [false, true] {
        let root = new_root(base, "pf5")?;
        if wal { make_wal_future(&root)?; }
        fs::write(root.join("instance.db-journal"), b"journal").map_err(|e| e.to_string())?;
        store::test_api::clear();
        let result = store::open_instance(&root);
        if !matches!(&result, Err(InstanceError::UnexpectedJournal)) {
            return Err(format!("journal with wal={wal}: {}", describe_instance_result(&result)));
        }
    }
    Ok("rollback journal refused with and without WAL".into())
}

fn case_pf6(base: &Path) -> Result<String, String> {
    for damage in ["magic", "page", "truncate"] {
        let root = new_root(base, "pf6")?;
        match damage {
            "magic" => edit_db(&root, |bytes| bytes[0] ^= 0xff)?,
            "page" => edit_db(&root, |bytes| { if bytes.len() > 100 { bytes[100] = 0xff; } })?,
            _ => {
                let path = db_path(&root);
                let bytes = fs::read(&path).map_err(|e| e.to_string())?;
                fs::write(&path, &bytes[..bytes.len() / 2]).map_err(|e| e.to_string())?;
            }
        }
        let db_before = fs::read(db_path(&root)).map_err(|e| e.to_string())?;
        let wal_path = side_path(&root, "wal");
        let wal_before = read_optional_file(&wal_path)?;
        let result = store::open_instance(&root);
        let accepted = matches!(&result, Err(InstanceError::HeaderInvalid(_)
            | InstanceError::Integrity(_) | InstanceError::PreflightUnavailable(_)
            | InstanceError::SchemaInconsistent(_) | InstanceError::Store(_)))
            || matches!(&result, Err(InstanceError::StageB { cause, observed_effects })
                if cause.contains("malformed") && observed_effects == "no inventory changes");
        let wal_after = read_optional_file(&wal_path)?;
        if !accepted || fs::read(db_path(&root)).map_err(|e| e.to_string())? != db_before
            || wal_after != wal_before {
            return Err(format!("{damage} result={}", describe_instance_result(&result)));
        }
    }
    Ok("header, page, and truncation corruption refused or classified; DB/WAL bytes retained".into())
}

fn legacy_root(base:&Path,label:&str)->Result<PathBuf,String>{
    let root=new_root(base,label)?;
    let c=Connection::open(db_path(&root)).map_err(|e|e.to_string())?;
    c.execute_batch("DROP TABLE events; DROP TABLE jobs; DROP TABLE commands; DROP TABLE runs; DROP TABLE runtime_epoch; DROP TABLE instance_meta; DROP TABLE yokai_schema_history; PRAGMA user_version=0;").map_err(|e|e.to_string())?;
    drop(c);Ok(root)
}
fn case_pf8(base: &Path) -> Result<String, String> {
    // Only the literal sqlite_ prefix is reserved; sqliteevil is a legal user table.
    for table in ["planted", "sqliteevil", "SQLITEevil"] {
        let root = legacy_root(base, "pf8")?;
        let connection = Connection::open(db_path(&root)).map_err(|e| e.to_string())?;
        connection.execute_batch(&format!("CREATE TABLE {table}(id INTEGER);")).map_err(|e| e.to_string())?;
        drop(connection);
        fs::write(side_path(&root, "wal"), b"").map_err(|e| e.to_string())?;
        store::test_api::clear();
        let result = store::open_instance(&root);
        if !matches!(&result, Err(InstanceError::SchemaInconsistent(_)))
            || !store::test_api::connection_events().iter().any(|event| *event == "stage-a-read-only") {
            return Err(format!("table={table} result={}", describe_instance_result(&result)));
        }
    }
    Ok("version-0 planted/sqliteevil/SQLITEevil tables refused on the WAL read-only path".into())
}

fn case_pf9(base: &Path) -> Result<String, String> {
    let root = new_root(base, "pf9")?;
    let before = fs::metadata(db_path(&root)).map_err(|e| e.to_string())?.len();
    store::test_api::mutate_during_stage_a_once().map_err(|e| e.to_string())?;
    let result = store::open_instance(&root);
    let after = fs::metadata(db_path(&root)).map_err(|e| e.to_string())?.len();
    if matches!(&result, Err(InstanceError::PreflightWrote(_))) && after == before + 1 {
        Ok(format!("one-shot test hook changed instance.db during Stage A; size {before}->{after}"))
    } else { Err(format!("result={} size={before}->{after}", describe_instance_result(&result))) }
}

fn case_pf10(base: &Path) -> Result<String, String> {
    let root_a = new_root(base, "pf10a")?;
    fs::write(side_path(&root_a, "shm"), b"shm").map_err(|e| e.to_string())?;
    if !matches!(store::open_instance(&root_a), Err(InstanceError::SideFileStateUnverified { .. })) {
        return Err("SHM-only case was not refused".into());
    }
    let root_b = new_root(base, "pf10b")?;
    fs::write(side_path(&root_b, "wal"), b"").map_err(|e| e.to_string())?;
    let input_wal_empty = fs::metadata(side_path(&root_b,"wal")).map_err(|e|e.to_string())?.len()==0;
    let instance_b = store::open_instance(&root_b).map_err(|e|e.to_string())?;
    let startup_ok = instance_b.startup_report().is_some_and(|r|r.epoch==2 && r.runs_changed==0);
    instance_b.close().map_err(|e|e.to_string())?;
    let connection = Connection::open_with_flags(db_path(&root_b),rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e|e.to_string())?;
    let epoch_events:i64=connection.query_row("SELECT count(*) FROM events WHERE kind='runtime.epoch' AND epoch=2",[],|r|r.get(0)).map_err(|e|e.to_string())?;
    drop(connection);
    let root_c = new_root(base, "pf10c")?;
    set_header_u32(&root_c, 60, 3)?;
    let result_c = store::open_instance(&root_c);
    if input_wal_empty && startup_ok && epoch_events==1
        && matches!(&result_c, Err(InstanceError::SchemaTooNew { found: 3, .. })) {
        Ok("SHM-only refused; empty input WAL accepted with required epoch2 commit/event; future3 refused".into())
    } else { Err(format!("empty_input={input_wal_empty} startup={startup_ok} epoch_events={epoch_events} future={}",describe_instance_result(&result_c))) }
}

fn case_pf11(base: &Path) -> Result<String, String> {
    let root = new_root(base, "pf11")?;
    fs::write(side_path(&root, "wal"), b"bad frame and salt bytes").map_err(|e| e.to_string())?;
    fs::write(side_path(&root, "shm"), [0xa5; 96]).map_err(|e| e.to_string())?;
    let db_before = fs::read(db_path(&root)).map_err(|e| e.to_string())?;
    let wal_before = fs::read(side_path(&root, "wal")).map_err(|e| e.to_string())?;
    let result = store::open_instance(&root);
    let outcome = format!("{}", describe_instance_result(&result));
    if let Ok(instance) = result { instance.close().map_err(|e| e.to_string())?; }
    if fs::read(db_path(&root)).map_err(|e| e.to_string())? == db_before
        && fs::read(side_path(&root, "wal")).map_err(|e| e.to_string())? == wal_before {
        Ok(format!("damaged WAL/SHM outcome={outcome}; no hang; main/WAL bytes unchanged"))
    } else { Err("damaged side-file check changed main database or WAL".into()) }
}

fn case_pf12(base: &Path) -> Result<String, String> {
    let root = new_root(base, "pf12")?;
    let other = new_root(base, "pf12-other")?;
    let db = fs::read(db_path(&other)).map_err(|e| e.to_string())?;
    fs::write(side_path(&root, "wal"), &db).map_err(|e| e.to_string())?;
    fs::write(side_path(&root, "shm"), b"foreign shared-memory bytes").map_err(|e| e.to_string())?;
    let db_before = fs::read(db_path(&root)).map_err(|e| e.to_string())?;
    let wal_before = fs::read(side_path(&root, "wal")).map_err(|e| e.to_string())?;
    let foreign_result = store::open_instance(&root);
    let foreign_outcome = format!("{}", describe_instance_result(&foreign_result));
    if let Ok(instance) = foreign_result { instance.close().map_err(|e| e.to_string())?; }
    let empty = new_root(base, "pf12-empty")?;
    fs::write(db_path(&empty), []).map_err(|e| e.to_string())?;
    fs::write(side_path(&empty, "wal"), b"foreign").map_err(|e| e.to_string())?;
    let zero = store::open_instance(&empty);
    let preserved = fs::read(db_path(&root)).map_err(|e| e.to_string())? == db_before
        && fs::read(side_path(&root, "wal")).map_err(|e| e.to_string())? == wal_before;
    if preserved && matches!(zero, Err(InstanceError::SchemaInconsistent(_))) {
        Ok(format!("foreign WAL/SHM outcome={foreign_outcome}; main/WAL preserved; zero-length DB with WAL typed refusal"))
    } else { Err(format!("preserved={preserved} zero_db={}", describe_instance_result(&zero))) }
}

#[cfg(unix)]
fn case_pf13(base: &Path) -> Result<String, String> {
    use std::os::unix::fs::PermissionsExt;
    let effective_uid = fs::read_to_string("/proc/self/status").map_err(|e| e.to_string())?
        .lines().find(|line| line.starts_with("Uid:")).and_then(|line| line.split_whitespace().nth(2))
        .ok_or("effective uid unavailable")?.parse::<u32>().map_err(|e| e.to_string())?;
    if effective_uid == 0 { return Err("PF-13 permission refusal is not exercisable as uid 0".into()); }
    let root = new_root(base, "pf13")?;
    let db = db_path(&root);
    let original = fs::metadata(&db).map_err(|e| e.to_string())?.permissions();
    let mut denied = false;
    let no_read = fs::Permissions::from_mode(0);
    fs::set_permissions(&db, no_read).map_err(|e| e.to_string())?;
    let result = store::open_instance(&root);
    denied |= matches!(&result, Err(InstanceError::PreflightUnavailable(_)));
    fs::set_permissions(&db, original).map_err(|e| e.to_string())?;
    if denied { Ok("unreadable database refused before a writable connection; mode restored".into()) }
    else { Err(format!("permission refusal not observed: {}", describe_instance_result(&result))) }
}
#[cfg(not(unix))]
fn case_pf13(_base: &Path) -> Result<String, String> {
    Ok("Unix permission-mode scenario is not applicable on this target".into())
}

fn case_pf14(base: &Path) -> Result<String, String> {
    let mut effects = Vec::new();
    for table in ["planted", "sqliteevil", "SQLITEevil"] {
        let root = legacy_root(base, "pf14")?;
        let connection = Connection::open(db_path(&root)).map_err(|e| e.to_string())?;
        connection.execute_batch(&format!("CREATE TABLE {table}(id INTEGER);")).map_err(|e| e.to_string())?;
        drop(connection);
        store::test_api::clear();
        let result = store::open_instance(&root);
        let events = store::test_api::connection_events();
        match result {
            Err(InstanceError::StageB { cause, observed_effects }) if cause.contains("schema objects")
                && events == ["stage-b-read-write"] => effects.push(format!("{table}: {observed_effects}")),
            other => return Err(format!("table={table} result={} events={events:?}", describe_instance_result(&other))),
        }
    }
    Ok(format!("raw-header accepted; Stage B refused planted/sqliteevil/SQLITEevil; measured effects: {}", effects.join("; ")))
}

fn case_pf15(base: &Path) -> Result<String, String> {
    let root = new_root(base, "pf15")?;
    let state=||->Result<(i64,String,String,i64),String>{
        let c=Connection::open_with_flags(db_path(&root),rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e|e.to_string())?;
        c.query_row("SELECT epoch, (SELECT group_concat(sql,';') FROM (SELECT sql FROM sqlite_schema ORDER BY name)), (SELECT group_concat(checksum,';') FROM (SELECT checksum FROM yokai_schema_history ORDER BY version)), (SELECT count(*) FROM events) FROM runtime_epoch WHERE id=1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).map_err(|e|e.to_string())
    };
    let prior=state()?;
    let before = root_snapshot(&root)?;
    let instance = store::open_instance(&root).map_err(|e| e.to_string())?;
    let startup_ok=instance.startup_report().is_some_and(|r|r.epoch==prior.0+1 && r.runs_examined==0 && r.runs_changed==0);
    instance.close().map_err(|e| e.to_string())?;
    let after = root_snapshot(&root)?;
    let current=state()?;
    if startup_ok && current.0==prior.0+1 && current.1==prior.1 && current.2==prior.2 && current.3==prior.3+1
        && before.get("instance.id")==after.get("instance.id") && before.get("instance.lock")==after.get("instance.lock")
        && before.get("instance.db")!=after.get("instance.db") {
        Ok(format!("successful Stage-B startup committed epoch {}->{} and one event; schema/history/identity/lock unchanged; {} before entries and {} after",prior.0,current.0,before.len(),after.len()))
    } else { Err(format!("unexpected successful-startup effects: startup={startup_ok} before_epoch={} after_epoch={}",prior.0,current.0)) }
}

fn case_pf16(base: &Path) -> Result<String, String> {
    let cases: [(&str, &str, Box<dyn Fn(&mut Vec<u8>)>); 9] = [
        ("magic", "magic", Box::new(|bytes| bytes[0] ^= 1)),
        ("page", "page size", Box::new(|bytes| bytes[16..18].copy_from_slice(&1000u16.to_be_bytes()))),
        ("page-upper", "page size", Box::new(|bytes| bytes[16..18].copy_from_slice(&u16::MAX.to_be_bytes()))),
        ("reserved", "reserved bytes", Box::new(|bytes| bytes[20] = 8)),
        ("format", "read/write format versions", Box::new(|bytes| { bytes[18] = 3; bytes[19] = 3; })),
        ("fractions", "payload fractions", Box::new(|bytes| bytes[21] = 65)),
        ("schema", "schema format", Box::new(|bytes| bytes[44..48].copy_from_slice(&5u32.to_be_bytes()))),
        ("encoding", "text encoding", Box::new(|bytes| bytes[56..60].copy_from_slice(&2u32.to_be_bytes()))),
        ("length", "database length", Box::new(|bytes| bytes.truncate(bytes.len() - 1))),
    ];
    for (case, expected_field, edit) in cases {
        let root = new_root(base, "pf16")?;
        edit_db(&root, |bytes| edit(bytes))?;
        store::test_api::clear();
        let result = store::open_instance(&root);
        let named_field = matches!(&result, Err(InstanceError::HeaderInvalid(field)) if field.contains(expected_field));
        if !named_field || !store::test_api::connection_events().is_empty() {
            return Err(format!("{case} header result={}", describe_instance_result(&result)));
        }
    }
    Ok("nine malformed raw headers named the invalid field and refused before SQLite".into())
}

fn case_pf17(base: &Path) -> Result<String, String> {
    for value in [0, 1, 2, 3, 255, 256, 65_536, 16_777_216, u32::MAX] {
        let root = if value==0 {legacy_root(base,"pf17")?}else{new_root(base,"pf17")?};
        if value==1 {let c=Connection::open(root.join("instance.db")).map_err(|e|e.to_string())?;
            c.execute_batch("DROP TABLE events; DROP TABLE jobs; DROP TABLE commands; DROP TABLE runs; DROP TABLE runtime_epoch; DELETE FROM yokai_schema_history WHERE version=2; PRAGMA user_version=1;").map_err(|e|e.to_string())?;}
        set_header_u32(&root, 60, value)?;
        let result = store::open_instance(&root);
        if value <= 2 {
            let instance = result.map_err(|e| e.to_string())?;
            instance.close().map_err(|e| e.to_string())?;
        } else if !matches!(&result, Err(InstanceError::SchemaTooNew { found, maximum: 2 }) if *found == value) {
            return Err(format!("value {value} decoded incorrectly: {}", describe_instance_result(&result)));
        }
    }
    Ok("big-endian user_version values 0, 1, 2, 255, 256, 65536, 16777216, and u32::MAX decoded; valid V0 migrated V1 upgraded and V2 accepted".into())
}

fn case_pf18(base: &Path) -> Result<String, String> {
    for (suffix, expected) in [
        ("wal", "wal"), ("shm", "shm"), ("journal", "journal"),
    ] {
        let root = new_root(base, "pf18")?;
        fs::write(db_path(&root), []).map_err(|e| e.to_string())?;
        fs::write(side_path(&root, suffix), b"").map_err(|e| e.to_string())?;
        let result = store::open_instance(&root);
        let matched = match expected {
            "wal" => matches!(&result, Err(InstanceError::SchemaInconsistent(_))),
            "shm" => matches!(&result, Err(InstanceError::SideFileStateUnverified { .. })),
            _ => matches!(&result, Err(InstanceError::UnexpectedJournal)),
        };
        if !matched { return Err(format!("zero database with {expected}: {}", describe_instance_result(&result))); }
    }
    Ok("zero-length database with WAL, SHM, and rollback journal received typed refusals".into())
}

fn case_sv2(base: &Path) -> Result<String, String> {
    let root = new_root(base, "sv2")?;
    let id_path = root.join("instance.id");
    let text = fs::read_to_string(&id_path).map_err(|e| e.to_string())?;
    let mut lines: Vec<&str> = text.lines().collect();
    lines[1] = "id 00000000000000000000000000000000";
    fs::write(&id_path, format!("{}\n{}\n{}\n", lines[0], lines[1], lines[2])).map_err(|e| e.to_string())?;
    let before = root_snapshot(&root)?;
    let result = store::open_instance(&root);
    if matches!(&result, Err(InstanceError::IdentityMismatch)) && before == root_snapshot(&root)? {
        Ok("identity/root mismatch refused; root bytes unchanged".into())
    } else { Err(format!("result={}", describe_instance_result(&result))) }
}

fn case_sv4(base: &Path) -> Result<String, String> {
    let root = new_root(base, "sv4")?;
    let id_path = root.join("instance.id");
    let text = fs::read_to_string(&id_path).map_err(|e| e.to_string())?;
    let rest = text.split_once('\n').ok_or("identity header missing")?.1;
    fs::write(&id_path, format!("yokai-instance format 77\n{rest}")).map_err(|e| e.to_string())?;
    store::test_api::clear();
    let result = store::open_instance(&root);
    let events = store::test_api::connection_events();
    if matches!(&result, Err(InstanceError::UnsupportedFormat(77))) && events.is_empty() {
        Ok("unknown identity format refused before database connection".into())
    } else { Err(format!("result={} events={events:?}", describe_instance_result(&result))) }
}

fn case_sha256_vectors() -> Result<String, String> {
    let vectors: [(&[u8], &str); 3] = [
        (b"", "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),
        (b"abc", "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
        (b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
         "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"),
    ];
    for (message, expected) in vectors {
        if store::test_api::sha256_chunks_for_test(&[message]) != expected {
            return Err(format!("known vector mismatch for {} bytes", message.len()));
        }
    }
    let split = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
    let chunked = store::test_api::sha256_chunks_for_test(&[&split[..1], &[], &split[1..17], &split[17..]]);
    if chunked != vectors[2].1 { return Err(format!("streamed chunk-boundary result={chunked}")); }
    Ok("FIPS 180-4 empty, abc, and 56-byte vectors passed in one-shot and split updates".into())
}

fn symlink_dir(target: &Path, link: &Path) -> Result<(), String> {
    #[cfg(unix)]
    { std::os::unix::fs::symlink(target, link).map_err(|e| format!("symlink {}: {e}", link.display())) }
    #[cfg(windows)]
    { std::os::windows::fs::symlink_dir(target, link).map_err(|e| format!("symlink {}: {e}", link.display())) }
}

fn symlink_file(target: &Path, link: &Path) -> Result<(), String> {
    #[cfg(unix)]
    { std::os::unix::fs::symlink(target, link).map_err(|e| format!("symlink {}: {e}", link.display())) }
    #[cfg(windows)]
    { std::os::windows::fs::symlink_file(target, link).map_err(|e| format!("symlink {}: {e}", link.display())) }
}
