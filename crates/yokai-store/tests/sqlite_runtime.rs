//! SQLite runtime checks for `yokai-store`, run against a file-backed database.
//!
//! Custom harness (`harness = false`). The first action clears the dumpable flag; if that fails,
//! no check runs and the exit status is 2. Output: one `CHECK <id> PASS|FAIL <detail>` line per
//! check, then `RESULT PASS <n>/<n>` or `RESULT FAIL <failed>/<n>`. Exit status 0 only if every
//! check passed.
//!
//! `YOKAI_TEST_DATA_DIR` must name an existing absolute directory. The test creates a fresh
//! subdirectory in it and leaves the database there for inspection.
use std::fmt::Display;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rusqlite::{Connection, OpenFlags};
use yokai_store as store;

/// Extended result code SQLITE_CONSTRAINT_FOREIGNKEY.
const SQLITE_CONSTRAINT_FOREIGNKEY: i32 = 787;
/// SQLite release bundled with the pinned rusqlite version, and its source identifier.
const BUNDLED_VERSION: &str = "3.53.2";
const BUNDLED_SOURCE_ID: &str =
    "2026-06-03 19:12:13 d6e03d8c777cfa2d35e3b60d8ec3e0187f3e9f99d8e2ee9cac695fd6fcdf1a24";

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
    if !dumpable.ok() {
        println!("RESULT FAIL non-dumpable gate not met; no check was run");
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
    let relative = store::create_file(Path::new("relative.db"));
    let existing = store::create_file(&dir);
    c.record(
        "R1_path_rules",
        matches!(relative, Err(store::StoreError::NotAbsolutePath(_)))
            && matches!(existing, Err(store::StoreError::AlreadyExists(_))),
        format!("relative={:?} existing={:?}", relative.err(), existing.err()),
    );
    let conn = store::create_file(&path).map_err(|e| format!("create_file: {e}"))?;

    // R2: the linked SQLite is the bundled release, at or above the minimum.
    let rt = store::sqlite_runtime(&conn).map_err(|e| format!("sqlite_runtime: {e}"))?;
    let at_least_min = store::parse_version(&rt.version).is_some_and(|v| v >= store::MIN_SQLITE_VERSION);
    c.record(
        "R2_sqlite_version",
        at_least_min && rt.version == BUNDLED_VERSION && rt.source_id == BUNDLED_SOURCE_ID,
        format!("version={} source_id={}", rt.version, rt.source_id),
    );

    // R3: settings read back after initialization.
    let s = store::read_settings(&conn).map_err(|e| format!("read_settings: {e}"))?;
    c.record("R3_settings", settings_ok(&s), format!("{s:?}"));

    // R4: the main database is the file at `path`.
    let file = store::main_database_file(&conn).map_err(|e| format!("main_database_file: {e}"))?;
    let canonical = std::fs::canonicalize(&path).map_err(|e| format!("canonicalize: {e}"))?;
    c.record(
        "R4_file_backed",
        !file.is_empty() && Path::new(&file) == canonical.as_path() && path.is_file(),
        format!("database_list.file={file} canonical={}", canonical.display()),
    );

    // R5: extension loading is refused from SQL.
    let ext = store::load_extension_refused(&conn);
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
    let found = store::foreign_key_violations(&conn).map_err(|e| format!("foreign_key_check: {e}"))?;
    let removed = conn.execute("DELETE FROM child WHERE id = 3", []);
    let after = store::foreign_key_violations(&conn).map_err(|e| format!("foreign_key_check: {e}"))?;
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
    let conn = store::open_file(&path).map_err(|e| format!("open_file: {e}"))?;
    let s = store::read_settings(&conn).map_err(|e| format!("read_settings: {e}"))?;
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
    Ok(())
}
