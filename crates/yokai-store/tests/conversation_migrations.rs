//! Fresh V3, historical V2 upgrade, and migration failure evidence.
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};
use yokai_store as store;
use yokai_types::CommandId;

fn fixture(name: &str) -> PathBuf {
    let root = PathBuf::from(std::env::var_os("YOKAI_TEST_DATA_DIR").expect("fresh fixture root"));
    assert!(root.is_absolute() && root.is_dir());
    let root = root.join(name);
    std::fs::create_dir(&root).unwrap();
    root
}

fn readonly(path: &Path) -> Connection {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

fn history(connection: &Connection) -> Vec<(i64, String, String, String)> {
    connection.prepare("SELECT version,name,checksum,applied_at FROM yokai_schema_history ORDER BY version")
        .unwrap().query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
        .unwrap().collect::<rusqlite::Result<_>>().unwrap()
}

fn version(connection: &Connection) -> i64 {
    connection.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap()
}

fn fresh_and_future() {
    let instance = store::create_instance(&fixture("fresh")).unwrap();
    let root = instance.root().to_owned();
    instance.close().unwrap();
    let path = root.join("instance.db");
    let conn = Connection::open(&path).unwrap();
    assert_eq!(version(&conn), 3);
    let rows = history(&conn);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].2, "434298f48d27b0725e69b4520a9fbfe89b8c7769c7d69cb4ad74a43966771ed7");
    assert_eq!(rows[1].2, "cc01ccc6ec50fd703818709ed322fb874153619b4a6961a8568cdac390fc723d");
    assert_eq!(rows[2].1, "conversation_core");
    let v3_bytes = include_bytes!("../migrations/V3__conversation_core.sql");
    let v3_checksum = store::test_api::sha256_chunks_for_test(&[v3_bytes]);
    assert_eq!(rows[2].2, v3_checksum);
    println!("INFO V3 migration checksum={v3_checksum}");
    assert!(!root.join("backups").exists());
    conn.execute_batch("PRAGMA user_version=4").unwrap();
    drop(conn);
    let before = std::fs::read(&path).unwrap();
    assert!(store::open_instance(&root).err().unwrap().to_string().contains("schema 4 is newer than supported maximum 3"));
    assert_eq!(std::fs::read(path).unwrap(), before);
}

type Rows = Vec<Vec<rusqlite::types::Value>>;

fn rows(connection: &Connection, sql: &str) -> Rows {
    let mut statement = connection.prepare(sql).unwrap();
    let columns = statement.column_count();
    statement.query_map([], |row| (0..columns).map(|i| row.get(i)).collect()).unwrap()
        .collect::<rusqlite::Result<_>>().unwrap()
}

fn durable_rows(connection: &Connection) -> Vec<Rows> {
    [
        "SELECT * FROM instance_meta",
        "SELECT * FROM runtime_epoch",
        "SELECT * FROM commands ORDER BY command_id",
        "SELECT * FROM runs ORDER BY run_id",
        "SELECT * FROM jobs ORDER BY job_id",
        "SELECT * FROM events ORDER BY seq",
    ].map(|sql| rows(connection, sql)).into()
}

fn v2_upgrade() {
    let instance = store::test_api::create_test_v2(&fixture("v2")).unwrap();
    let request = store::CommandRequest::new(CommandId::new("v2-command").unwrap(), "v2", 1, b"\0v2\xff").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let store::AcceptOutcome::Accepted { run_id } = instance.store().accept_command(request.clone(), deadline)
        .unwrap().wait(deadline).unwrap() else { panic!("V2 fixture did not accept command") };
    let root = instance.root().to_owned();
    instance.close().unwrap();
    let conn = readonly(&root.join("instance.db"));
    assert_eq!(version(&conn), 2);
    let prior_history = history(&conn);
    let prior_rows = durable_rows(&conn);
    drop(conn);

    let upgraded = store::open_instance(&root).unwrap();
    assert_eq!(store::test_api::user_version(&upgraded).unwrap(), 3);
    let result = upgraded.store().accept_command(request, deadline).unwrap().wait(deadline).unwrap();
    assert_eq!(result, store::AcceptOutcome::Duplicate { run_id });
    upgraded.close().unwrap();
    let backups = std::fs::read_dir(root.join("backups")).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(backups.len(), 1);
    let snapshot = readonly(&backups[0].path());
    assert_eq!(version(&snapshot), 2);
    assert_eq!(history(&snapshot), prior_history);
    assert_eq!(durable_rows(&snapshot), prior_rows);
    let current = readonly(&root.join("instance.db"));
    assert_eq!(&history(&current)[..2], prior_history.as_slice());
    assert_eq!(rows(&current, "SELECT * FROM commands ORDER BY command_id"), prior_rows[2]);
}

fn rollback() {
    let old = store::test_api::create_test_v2(&fixture("rollback")).unwrap();
    let root = old.root().to_owned();
    old.close().unwrap();
    let before = readonly(&root.join("instance.db"));
    let prior = history(&before);
    drop(before);
    let child = Command::new(std::env::current_exe().unwrap())
        .arg("--expect-v3-error").arg(&root)
        .env("YOKAI_TEST_FAILPOINT", "migration-3-before-commit:error").output().unwrap();
    assert!(child.status.success(), "{}", String::from_utf8_lossy(&child.stderr));
    let conn = readonly(&root.join("instance.db"));
    assert_eq!(version(&conn), 2);
    assert_eq!(history(&conn), prior);
    assert_eq!(conn.query_row("SELECT count(*) FROM sqlite_schema WHERE name='conversations'", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
    assert_eq!(std::fs::read_dir(root.join("backups")).unwrap().count(), 1);
    drop(conn);
    let upgraded = store::open_instance(&root).unwrap();
    assert_eq!(store::test_api::user_version(&upgraded).unwrap(), 3);
    upgraded.close().unwrap();
}

fn main() -> ExitCode {
    let protection = yokai_testsupport::make_non_dumpable();
    println!("{}", protection.line());
    if !protection.permits_checks() { return ExitCode::FAILURE; }
    let args: Vec<_> = std::env::args_os().collect();
    if let [_, flag, path] = args.as_slice() {
        if flag == "--expect-v3-error" {
            let error = store::open_instance(Path::new(path)).err().expect("V3 must fail");
            assert!(error.to_string().contains("migration-3-before-commit"), "{error}");
            return ExitCode::SUCCESS;
        }
    }
    assert_eq!(args.len(), 1);
    for (id, check) in [
        ("M1-V3-FRESH", fresh_and_future as fn()),
        ("M1-V2-UPGRADE", v2_upgrade),
        ("M1-V3-ROLLBACK", rollback),
    ] {
        if std::panic::catch_unwind(check).is_err() {
            println!("CHECK {id} FAIL");
            return ExitCode::FAILURE;
        }
        println!("CHECK {id} PASS");
    }
    ExitCode::SUCCESS
}
