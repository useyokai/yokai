//! SQLite runtime checks for `yokai-store`, run against a file-backed database.
//!
//! Custom harness (harness = false). Linux clears the dumpable flag; Windows reports that
//! the check is unsupported and requires the isolated-run opt-in documented by yokai-testsupport.
//! Without the required status or opt-in, no check runs and the exit status is 2. Output is one
//! CHECK line per check followed by a RESULT line.

#[path = "support/mod.rs"]
mod support;

use std::fmt::Display;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Barrier;

use rusqlite::{Connection, OpenFlags};
use yokai_store as store;

/// Extended result code SQLITE_CONSTRAINT_FOREIGNKEY.
const SQLITE_CONSTRAINT_FOREIGNKEY: i32 = 787;
/// SQLite release bundled with the pinned rusqlite version, and its source identifier.
const BUNDLED_VERSION: &str = "3.54.1";
const BUNDLED_SOURCE_ID: &str =
    "2026-10-16 14:45:20 555e07ef33f69a0957d24d8e555b702464a131d6f1607f4c3c1e4551e8e6ca76";
/// R11: fresh paths raced, and concurrent `create_file` callers per path.
const RACE_ROUNDS: usize = 64;
const RACE_CREATORS: usize = 16;

struct Checks {
    total: usize,
    failed: usize,
}

impl Checks {
    fn record(&mut self, id: &str, ok: bool, detail: impl Display) {
        self.total += 1;
        if !ok {
            self.failed += 1;
        }
        println!("CHECK {id} {} {detail}", if ok { "PASS" } else { "FAIL" });
    }
}

fn main() -> ExitCode {
    let dumpable = yokai_testsupport::make_non_dumpable();
    println!("{}", dumpable.line());
    if !dumpable.permits_checks() {
        println!("RESULT FAIL dumpability status unavailable or not opted in; no check was run");
        return ExitCode::from(2);
    }
    let mut checks = Checks { total: 0, failed: 0 };
    if let Err(e) = run(&mut checks) {
        checks.record("S0_setup", false, e);
    }
    if checks.failed == 0 && checks.total > 0 {
        println!("RESULT PASS {}/{}", checks.total, checks.total);
        ExitCode::SUCCESS
    } else {
        println!("RESULT FAIL {}/{}", checks.failed, checks.total);
        ExitCode::FAILURE
    }
}

fn count(conn: &Connection, table: &str) -> Result<i64, String> {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .map_err(|e| format!("count {table}: {e}"))
}

/// True if `result` failed with SQLITE_CONSTRAINT_FOREIGNKEY.
fn is_fk_violation(result: &rusqlite::Result<usize>) -> bool {
    matches!(result, Err(e) if store::ext_code(e) == Some(SQLITE_CONSTRAINT_FOREIGNKEY))
}

fn settings_ok(s: &store::ConnectionSettings) -> bool {
    s.journal_mode.eq_ignore_ascii_case("wal")
        && s.synchronous == 2
        && s.foreign_keys == 1
        && s.busy_timeout == store::BUSY_TIMEOUT_MS as i64
}

fn file_header(path: &Path) -> Result<[u8; 16], String> {
    let mut header = [0u8; 16];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut header))
        .map_err(|e| format!("read header of {}: {e}", path.display()))?;
    Ok(header)
}

fn run(c: &mut Checks) -> Result<(), String> {
    let base = std::env::var_os("YOKAI_TEST_DATA_DIR")
        .map(PathBuf::from)
        .ok_or("YOKAI_TEST_DATA_DIR is not set")?;
    if !base.is_absolute() || !base.is_dir() {
        return Err(format!("YOKAI_TEST_DATA_DIR={} is not an existing absolute directory", base.display()));
    }
    let dir = base.join(format!("sqlite_runtime-{}", std::process::id()));
    std::fs::create_dir(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let path = dir.join("store.db");
    println!("INFO db_path={}", path.display());

    // R1: the store refuses a relative path and an existing path, and creates a new file.
    let relative = store::test_api::create_file(Path::new("relative.db"));
    let existing = store::test_api::create_file(&dir);
    c.record(
        "R1_path_rules",
        matches!(relative, Err(store::StoreError::NotAbsolutePath(_)))
            && matches!(existing, Err(store::StoreError::AlreadyExists(_))),
        format!("relative={:?} existing={:?}", relative.err(), existing.err()),
    );
    let conn = store::test_api::create_file(&path).map_err(|e| format!("create_file: {e}"))?;

    // R2: the linked SQLite is the bundled release, at or above the minimum.
    let rt = store::test_api::sqlite_runtime(&conn).map_err(|e| format!("sqlite_runtime: {e}"))?;
    let at_least_min = store::parse_version(&rt.version).is_some_and(|v| v >= store::MIN_SQLITE_VERSION);
    c.record(
        "R2_sqlite_version",
        at_least_min && rt.version == BUNDLED_VERSION && rt.source_id == BUNDLED_SOURCE_ID,
        format!("version={} source_id={}", rt.version, rt.source_id),
    );

    // R3: settings read back after initialization.
    let s = store::test_api::read_settings(&conn).map_err(|e| format!("read_settings: {e}"))?;
    c.record("R3_settings", settings_ok(&s), format!("{s:?}"));

    // R4: the main database is the file at `path`.
    let file = store::test_api::main_database_file(&conn).map_err(|e| format!("main_database_file: {e}"))?;
    let canonical = std::fs::canonicalize(&path).map_err(|e| format!("canonicalize: {e}"))?;
    let same_file = !file.is_empty()
        && std::fs::canonicalize(Path::new(&file))
            .map(|reported| reported.as_path() == canonical.as_path())
            .unwrap_or(false);
    c.record(
        "R4_file_backed",
        same_file && path.is_file(),
        format!("database_list.file={file} canonical={}", canonical.display()),
    );

    // R5: extension loading is refused from SQL.
    let ext = store::test_api::load_extension_refused(&conn);
    c.record(
        "R5_load_extension_refused",
        matches!(&ext, Ok(msg) if msg.contains("not authorized")),
        format!("{ext:?}"),
    );

    conn.execute_batch(
        "CREATE TABLE parent(id INTEGER PRIMARY KEY);
         CREATE TABLE child(id INTEGER PRIMARY KEY, parent_id INTEGER NOT NULL REFERENCES parent(id));
         INSERT INTO parent(id) VALUES (1);
         INSERT INTO child(id, parent_id) VALUES (1, 1);",
    )
    .map_err(|e| format!("schema: {e}"))?;

    // R6: inserting a child that references a missing parent is rejected.
    let orphan = conn.execute("INSERT INTO child(id, parent_id) VALUES (2, 99)", []);
    let children = count(&conn, "child")?;
    c.record(
        "R6_fk_insert_rejected",
        is_fk_violation(&orphan) && children == 1,
        format!("result={orphan:?} child_rows={children}"),
    );

    // R7: deleting a parent that is still referenced is rejected.
    let delete = conn.execute("DELETE FROM parent WHERE id = 1", []);
    let parents = count(&conn, "parent")?;
    c.record(
        "R7_fk_delete_rejected",
        is_fk_violation(&delete) && parents == 1,
        format!("result={delete:?} parent_rows={parents}"),
    );

    // R8: control. Enforcement comes from the connection setting: a second connection with
    // `foreign_keys = OFF` accepts the orphan, and `foreign_key_check` then reports exactly it.
    let raw = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|e| format!("control connection: {e}"))?;
    raw.execute_batch("PRAGMA foreign_keys = OFF;").map_err(|e| format!("control pragma: {e}"))?;
    let fk_off: i64 = raw
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .map_err(|e| format!("control readback: {e}"))?;
    let inserted = raw.execute("INSERT INTO child(id, parent_id) VALUES (3, 77)", []);
    drop(raw);
    let found = store::test_api::foreign_key_violations(&conn).map_err(|e| format!("foreign_key_check: {e}"))?;
    let removed = conn.execute("DELETE FROM child WHERE id = 3", []);
    let after = store::test_api::foreign_key_violations(&conn).map_err(|e| format!("foreign_key_check: {e}"))?;
    c.record(
        "R8_fk_control",
        fk_off == 0
            && matches!(inserted, Ok(1))
            && found.len() == 1
            && found[0] == "child rowid=Some(3) parent=parent fkid=0"
            && matches!(removed, Ok(1))
            && after.is_empty(),
        format!("fk_off={fk_off} inserted={inserted:?} found={found:?} removed={removed:?} after={after:?}"),
    );
    drop(conn);

    // R9: reopened from disk, the settings are applied again and the committed rows remain.
    let conn = store::test_api::open_file(&path).map_err(|e| format!("open_file: {e}"))?;
    let s = store::test_api::read_settings(&conn).map_err(|e| format!("read_settings: {e}"))?;
    let (parents, children) = (count(&conn, "parent")?, count(&conn, "child")?);
    c.record(
        "R9_reopen",
        settings_ok(&s) && parents == 1 && children == 1,
        format!("{s:?} parent_rows={parents} child_rows={children}"),
    );
    drop(conn);

    // R10: after the last connection closed, the file on disk is a SQLite database.
    let header = file_header(&path)?;
    let size = std::fs::metadata(&path).map_err(|e| format!("metadata: {e}"))?.len();
    c.record(
        "R10_on_disk_format",
        &header == b"SQLite format 3\0" && size > 0,
        format!("header={:?} size={size}", String::from_utf8_lossy(&header)),
    );

    // R11: concurrent creators of one path. Every round, RACE_CREATORS threads released by one
    // barrier call `create_file` on the same new path: exactly one gets a connection to that
    // file with the required settings, and every other caller gets `AlreadyExists`.
    let canonical_dir = std::fs::canonicalize(&dir).map_err(|e| format!("canonicalize: {e}"))?;
    let mut bad_rounds = Vec::new();
    for round in 0..RACE_ROUNDS {
        let race_path = dir.join(format!("race-{round}.db"));
        let barrier = Barrier::new(RACE_CREATORS);
        let outcomes: Vec<String> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..RACE_CREATORS)
                .map(|_| scope.spawn(|| creator_outcome(&barrier, &race_path, &canonical_dir)))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap_or_else(|_| "panicked".into())).collect()
        });
        let created = outcomes.iter().filter(|o| *o == "created").count();
        let refused = outcomes.iter().filter(|o| *o == "already_exists").count();
        if created != 1 || refused != RACE_CREATORS - 1 {
            bad_rounds.push(format!("round {round}: {outcomes:?}"));
        }
    }
    c.record(
        "R11_concurrent_create_exclusive",
        bad_rounds.is_empty(),
        format!("rounds={RACE_ROUNDS} creators={RACE_CREATORS} bad_rounds={bad_rounds:?}"),
    );

    support::verify_absence_oracle(&dir)?;

    // R12: an existing database, created outside the store with the default rollback journal,
    // is refused and left byte-for-byte unchanged, without WAL or shared-memory files. On Unix,
    // a dangling symbolic link is refused and its target is not created.
    let existing_path = dir.join("existing.db");
    let other = Connection::open(&existing_path).map_err(|e| format!("existing db: {e}"))?;
    other
        .execute_batch("CREATE TABLE marker(id INTEGER PRIMARY KEY); INSERT INTO marker(id) VALUES (7);")
        .map_err(|e| format!("existing db schema: {e}"))?;
    drop(other);
    let before = std::fs::read(&existing_path).map_err(|e| format!("read existing: {e}"))?;
    let refused = store::test_api::create_file(&existing_path);
    let after = std::fs::read(&existing_path).map_err(|e| format!("read existing: {e}"))?;
    let mut side_files = Vec::new();
    for name in ["existing.db-wal", "existing.db-shm", "existing.db-journal"] {
        if support::entry_exists(&dir.join(name))? { side_files.push(name); }
    }
    let (link_refused, link_detail) = dangling_link_refused(&dir)?;
    c.record(
        "R12_existing_untouched",
        matches!(refused, Err(store::StoreError::AlreadyExists(_)))
            && before == after
            && side_files.is_empty()
            && link_refused,
        format!(
            "result={:?} unchanged={} side_files={side_files:?} {link_detail}",
            refused.err(),
            before == after
        ),
    );
    Ok(())
}

/// One R11 creator: waits at the barrier, calls `create_file`, and on success checks that the
/// connection is backed by the raced file and has the required settings.
fn creator_outcome(barrier: &Barrier, path: &Path, canonical_dir: &Path) -> String {
    barrier.wait();
    match store::test_api::create_file(path) {
        Ok(conn) => {
            let file = store::test_api::main_database_file(&conn);
            let settings = store::test_api::read_settings(&conn);
            let expected = canonical_dir.join(path.file_name().unwrap_or_default());
            let same_file = file.as_ref().ok()
                .and_then(|reported| std::fs::canonicalize(Path::new(reported)).ok())
                .is_some_and(|resolved| resolved == expected);
            match (file, settings) {
                (Ok(_), Ok(s)) if same_file && settings_ok(&s) => "created".into(),
                (f, s) => format!("created_wrong file={f:?} settings={s:?}"),
            }
        }
        Err(store::StoreError::AlreadyExists(_)) => "already_exists".into(),
        Err(e) => format!("error {e}"),
    }
}

#[cfg(unix)]
fn dangling_link_refused(dir: &Path) -> Result<(bool, String), String> {
    let link = dir.join("dangling.db");
    let target = dir.join("dangling-target.db");
    std::os::unix::fs::symlink(&target, &link).map_err(|e| format!("symlink: {e}"))?;
    let result = store::test_api::create_file(&link);
    let target_created = support::entry_exists(&target)?;
    let ok = matches!(result, Err(store::StoreError::AlreadyExists(_))) && !target_created;
    Ok((ok, format!("dangling_link={:?} target_created={target_created}", result.err())))
}

#[cfg(windows)]
fn dangling_link_refused(dir: &Path) -> Result<(bool, String), String> {
    use std::os::windows::fs::MetadataExt;
    let link=dir.join("dangling.db");let target=dir.join("dangling-target.db");
    if !dir.is_absolute() || support::entry_exists(&link)? || support::entry_exists(&target)?{return Err("dangling fixture paths are not fresh/absolute".into());}
    match std::os::windows::fs::symlink_file(&target,&link){
        Ok(())=>{},
        Err(e) if e.raw_os_error()==Some(1314) || e.kind()==std::io::ErrorKind::PermissionDenied=>return Ok((true,format!("dangling_link=not_checked capability unavailable: {e}"))),
        Err(e)=>return Err(format!("Windows file symlink: {e}")),
    }
    let before=std::fs::read_link(&link).map_err(|e|e.to_string())?;
    let result=store::test_api::create_file(&link);let result_error=result.err();
    let target_created=support::entry_exists(&target)?;
    let after=std::fs::read_link(&link).map_err(|e|e.to_string())?;
    let metadata=std::fs::symlink_metadata(&link).map_err(|e|e.to_string())?;
    let mut side_files=Vec::new();
    for name in ["dangling.db-wal","dangling.db-shm","dangling.db-journal","dangling-target.db-wal","dangling-target.db-shm","dangling-target.db-journal"] {
        if support::entry_exists(&dir.join(name))? { side_files.push(name); }
    }
    let conventional=|p:&Path|{let text=p.to_string_lossy();text.strip_prefix("\\\\?\\").unwrap_or(&text).to_owned()};
    let unchanged=conventional(&before)==conventional(&target) && after==before && metadata.file_type().is_symlink() && metadata.file_attributes()&0x400!=0;
    println!("INFO R12 Windows dangling_link={} target={} before_target={} after_target={} reparse={} target_created={target_created} side_files={side_files:?}",
        link.display(),target.display(),before.display(),after.display(),metadata.file_attributes()&0x400!=0);
    // Remove only our verified link; its target stays absent and the inventory is retained above.
    if !unchanged || link.parent()!=Some(dir) || !link.is_absolute(){return Err("owned Windows dangling-link identity changed; no unlink attempted".into());}
    std::fs::remove_file(&link).map_err(|e|format!("unlink owned dangling symlink: {e}"))?;
    let target_after_unlink=support::entry_exists(&target)?;
    let ok=matches!(result_error,Some(store::StoreError::AlreadyExists(_))) && !target_created && !target_after_unlink && side_files.is_empty();
    Ok((ok,format!("dangling_link=checked_windows result={result_error:?} unchanged={unchanged} target_created={target_created} target_after_unlink={target_after_unlink}; owned link unlinked after inventory")))
}

#[cfg(not(any(unix,windows)))]
fn dangling_link_refused(_dir: &Path) -> Result<(bool, String), String> {
    Ok((true, "dangling_link=not_checked".into()))
}
