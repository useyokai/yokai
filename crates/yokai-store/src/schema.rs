//! Embedded SQL migrations. History is drift detection, not tamper resistance.
use std::path::Path;
use rusqlite::{Connection, TransactionBehavior, params};
use yokai_types::InstanceId;
use crate::InstanceError;

const V1: &str = include_str!("../migrations/V1__instance_meta.sql");
const V2: &str = include_str!("../migrations/V2__durable_records.sql");
const HISTORY: &str = "CREATE TABLE yokai_schema_history (version INTEGER PRIMARY KEY, name TEXT NOT NULL, checksum TEXT NOT NULL CHECK (length(checksum) = 64), applied_at TEXT NOT NULL) STRICT";
#[derive(Clone,Copy)]
struct Migration { version:u32,name:&'static str,sql:&'static str }
const FIRST:Migration=Migration{version:1,name:"instance_meta",sql:V1};
const SECOND:Migration=Migration{version:2,name:"durable_records",sql:V2};
#[derive(Clone,Copy)]
pub(crate) struct Plan(&'static [Migration]);
impl Plan {
    pub(crate) fn latest(self)->u32 {self.0.last().map_or(0,|m|m.version)}
    pub(crate) fn has_durable(self)->bool{self.0.iter().any(|m|m.version==2 && m.name=="durable_records")}
    fn validate(self)->Result<(),InstanceError>{
        for (i,m) in self.0.iter().enumerate(){
            if m.version as usize!=i+1{return Err(migration("non-contiguous embedded plan"));}
            lint(m.sql,m.version).map_err(migration)?;
        }
        Ok(())
    }
}
pub(crate) fn production()->Plan{Plan(&[FIRST,SECOND])}
#[cfg(feature="test-api")]
pub(crate) fn test_plan(latest:u32,fail:bool)->Result<Plan,InstanceError>{
    const PROBE:Migration=Migration{version:3,name:"migration_probe",sql:"CREATE TABLE migration_probe (id INTEGER PRIMARY KEY, value TEXT NOT NULL) STRICT; PRAGMA user_version = 3;"};
    const FAIL:Migration=Migration{version:3,name:"migration_probe",sql:"CREATE TABLE migration_probe (id INTEGER PRIMARY KEY, value TEXT NOT NULL) STRICT; INSERT INTO missing_table VALUES(1); PRAGMA user_version = 3;"};
    const DATA:Migration=Migration{version:4,name:"probe_data",sql:"INSERT INTO migration_probe VALUES(1,'retained'); PRAGMA user_version = 4;"};
    match (latest,fail){(1,false)=>Ok(Plan(&[FIRST])),(2,false)=>Ok(production()),(3,false)=>Ok(Plan(&[FIRST,SECOND,PROBE])),(3,true)=>Ok(Plan(&[FIRST,SECOND,FAIL])),(4,false)=>Ok(Plan(&[FIRST,SECOND,PROBE,DATA])),_=>Err(migration("invalid test-only plan"))}
}

fn inconsistent(e: impl ToString) -> InstanceError { InstanceError::SchemaInconsistent(e.to_string()) }
fn migration(e: impl ToString) -> InstanceError { InstanceError::Migration(e.to_string()) }
fn checksum(sql: &str) -> String { crate::preflight::sha256_bytes(sql.as_bytes()) }
fn normalized(sql: &str) -> String { sql.trim().trim_end_matches(';').split_whitespace().collect::<Vec<_>>().join(" ") }

/// Read-only validation, shared by both compatibility stages. Caller owns the Instance lock.
pub(crate) fn inspect(conn: &Connection, id: InstanceId, created: &str, plan:Plan) -> Result<u32, InstanceError> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(inconsistent)?;
    if version < 0 { return Err(inconsistent("negative user_version")); }
    if version > i64::from(plan.latest()) { return Err(InstanceError::SchemaTooNew { found: version as u32, maximum: plan.latest() }); }
    let mut stmt = conn.prepare("PRAGMA integrity_check").map_err(inconsistent)?;
    let results: Vec<String> = stmt.query_map([], |r| r.get(0)).map_err(inconsistent)?
        .collect::<Result<_, _>>().map_err(inconsistent)?;
    if results != ["ok"] { return Err(InstanceError::Integrity(format!("{results:?}"))); }
    let fk = crate::foreign_key_violations(conn).map_err(inconsistent)?;
    if !fk.is_empty() { return Err(InstanceError::ForeignKeyViolations(fk)); }
    let mut stmt = conn.prepare("SELECT type, name, sql FROM sqlite_schema WHERE lower(substr(name, 1, 7)) <> 'sqlite_' ORDER BY name").map_err(inconsistent)?;
    let objects: Vec<(String,String,String)> = stmt.query_map([], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).map_err(inconsistent)?
        .collect::<Result<_, _>>().map_err(inconsistent)?;
    if version == 0 {
        if !objects.is_empty() { return Err(inconsistent(format!("{} schema objects at version 0", objects.len()))); }
        return Ok(0);
    }
    // SQLite itself determines statement/trigger boundaries in the reference schema.
    // The reference is memory-only; the inspected database connection remains read-only.
    let reference=Connection::open_in_memory().map_err(inconsistent)?;
    reference.execute_batch(HISTORY).map_err(inconsistent)?;
    for m in plan.0.iter().filter(|m|i64::from(m.version)<=version) {
        reference.execute_batch(m.sql).map_err(inconsistent)?;
    }
    let mut expected_stmt=reference.prepare("SELECT type,name,sql FROM sqlite_schema WHERE lower(substr(name,1,7)) <> 'sqlite_' ORDER BY name").map_err(inconsistent)?;
    let expected:Vec<(String,String,String)>=expected_stmt.query_map([],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))
        .map_err(inconsistent)?.collect::<Result<_,_>>().map_err(inconsistent)?;
    if objects.len() != expected.len() || !objects.iter().zip(expected).all(|((kind,name,sql),(want_kind,want_name,ddl))|
        kind == &want_kind && name == &want_name && normalized(sql) == normalized(&ddl)) {
        return Err(inconsistent("schema objects differ from the embedded version"));
    }
    let mut stmt = conn.prepare("SELECT version,name,checksum,applied_at FROM yokai_schema_history ORDER BY version").map_err(inconsistent)?;
    let history: Vec<(i64,String,String,String)> = stmt.query_map([], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))
        .map_err(inconsistent)?.collect::<Result<_,_>>().map_err(inconsistent)?;
    if history.len()!=version as usize || !history.iter().zip(plan.0).all(|(row,m)|row.0==i64::from(m.version)) {
        return Err(inconsistent("user_version and contiguous migration history disagree"));
    }
    if history.iter().any(|row|!crate::instance::valid_timestamp(&row.3)) {
        return Err(inconsistent("invalid migration history timestamp"));
    }
    if !history.iter().zip(plan.0).all(|(row,m)|row.1==m.name && row.2==checksum(m.sql)) {
        return Err(InstanceError::MigrationDiverged("migration name/checksum mismatch".into()));
    }
    let mut stmt = conn.prepare("SELECT id,instance_id,format,created_at,last_health_check FROM instance_meta").map_err(inconsistent)?;
    let meta: Vec<(i64,Vec<u8>,i64,String,Option<String>)> = stmt.query_map([], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))
        .map_err(inconsistent)?.collect::<Result<_,_>>().map_err(inconsistent)?;
    if meta.len() != 1 || meta[0].0 != 1 || meta[0].1.as_slice() != id.as_bytes() || meta[0].2 != 1 || meta[0].3 != created {
        return Err(InstanceError::IdentityMismatch);
    }
    if meta[0].4.as_deref().is_some_and(|s| !crate::instance::valid_timestamp(s)) {
        return Err(inconsistent("invalid last_health_check"));
    }
    if version>=2 && plan.has_durable() {
        let epochs:Vec<(i64,i64)>=conn.prepare("SELECT id,epoch FROM runtime_epoch").map_err(inconsistent)?
            .query_map([],|r|Ok((r.get(0)?,r.get(1)?))).map_err(inconsistent)?.collect::<Result<_,_>>().map_err(inconsistent)?;
        if epochs.len()!=1 || epochs[0].0!=1 || epochs[0].1<1{return Err(inconsistent("runtime epoch singleton missing or invalid"));}
    }
    Ok(version as u32)
}

/// Deliberately conservative SQL lexer: quoted identifiers are not needed by embedded SQL.
/// Comments and string literals do not contribute statement keywords.
pub(crate) fn lint(sql: &str, version: u32) -> Result<(), String> {
    let bytes=sql.as_bytes(); let mut i=0; let mut text=String::new();
    while i<bytes.len() {
        match bytes[i] {
            b'-' if bytes.get(i+1)==Some(&b'-') => { i+=2; while i<bytes.len() && bytes[i]!=b'\n' { i+=1; } text.push(' '); }
            b'/' if bytes.get(i+1)==Some(&b'*') => {
                i+=2; while i+1<bytes.len() && &bytes[i..i+2]!=b"*/" { i+=1; }
                if i+1>=bytes.len() { return Err("unterminated comment".into()); } i+=2; text.push(' ');
            }
            b'\'' => {
                i+=1; let mut closed=false;
                while i<bytes.len() { if bytes[i]==b'\'' { i+=1; if bytes.get(i)==Some(&b'\'') { i+=1; } else {closed=true;break;} } else {i+=1;} }
                if !closed { return Err("unterminated string".into()); } text.push_str(" literal ");
            }
            b'"'|b'`'|b'['|b']' => return Err("quoted identifiers are outside the migration SQL subset".into()),
            b if b.is_ascii() => { text.push((b as char).to_ascii_lowercase()); i+=1; }
            _ => return Err("non-ASCII SQL outside a string/comment".into()),
        }
    }
    let statements: Vec<&str> = text.split(';').map(str::trim).filter(|s| !s.is_empty()).collect();
    if !text.trim_end().ends_with(';') || statements.last().copied()!=Some(format!("pragma user_version = {version}").as_str()) {
        return Err("migration must end with its matching PRAGMA user_version".into());
    }
    let mut trigger_end=false;
    for (index,statement) in statements.iter().enumerate() {
        let tokens: Vec<&str> = statement.split(|c:char| !c.is_ascii_alphanumeric() && c!='_').filter(|s| !s.is_empty()).collect();
        if trigger_end {
            if tokens!=["end"] {return Err("immutable trigger must end immediately".into());}
            trigger_end=false;continue;
        }
        if tokens.starts_with(&["create","trigger"]) {
            // Only the reviewed command refusal triggers need BEGIN/END in a migration.
            // A trigger cannot smuggle a transaction/connection statement through lint.
            let direct=tokens.len()==12 && ["commands_immutable","commands_append_only"].contains(&tokens[2])
                && tokens[3]=="before" && ["update","delete"].contains(&tokens[4])
                && tokens[5..]==["on","commands","begin","select","raise","abort","literal"];
            let replacement=tokens==["create","trigger","commands_no_replace","before","insert","on","commands",
                "when","exists","select","1","from","commands","where","command_id","new","command_id",
                "or","run_id","new","run_id","begin","select","raise","abort","literal"];
            if !direct && !replacement {
                return Err("only immutable command refusal triggers are allowed".into());
            }
            trigger_end=true;continue;
        }
        if tokens.iter().any(|s| ["begin","commit","end","rollback","savepoint","release","vacuum","attach","detach"].contains(s)) {
            return Err("forbidden transaction or connection statement".into());
        }
        if tokens.contains(&"pragma") && index+1!=statements.len() { return Err("only terminal user_version PRAGMA is allowed".into()); }
    }
    if trigger_end {return Err("unterminated immutable trigger".into());}
    Ok(())
}

pub(crate) fn migrate_new(conn: &mut Connection, id: InstanceId, created: &str) -> Result<(), InstanceError> {
    let plan=production();plan.validate()?;
    for m in plan.0 {apply(conn,id,created,*m,plan)?;}
    Ok(())
}
fn apply(conn:&mut Connection,id:InstanceId,created:&str,m:Migration,plan:Plan)->Result<(),InstanceError>{
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(migration)?;
    let result = (|| -> Result<(), InstanceError> {
        if m.version==1 {tx.execute_batch(HISTORY).map_err(migration)?;}
        tx.execute_batch(m.sql).map_err(migration)?;
        if m.version==1 {tx.execute("INSERT INTO instance_meta(id,instance_id,format,created_at,last_health_check) VALUES(1,?1,1,?2,NULL)",params![id.as_bytes().as_slice(),created]).map_err(migration)?;}
        let stamp=crate::instance::utc_timestamp().map_err(migration)?;
        tx.execute("INSERT INTO yokai_schema_history(version,name,checksum,applied_at) VALUES(?1,?2,?3,?4)",params![m.version,m.name,checksum(m.sql),stamp]).map_err(migration)?;
        crate::instance::hit_failpoint(&format!("migration-{}-before-commit",m.version))?;
        inspect(&tx,id,created,plan)?;
        Ok(())
    })();
    if let Err(error)=result {
        return match tx.rollback() { Ok(())=>Err(error), Err(rollback)=>Err(migration(format!("{error}; rollback unconfirmed: {rollback}"))) };
    }
    tx.commit().map_err(|e| migration(format!("commit outcome unconfirmed; inspect durable state before retry: {e}")))
}

pub(crate) fn upgrade(conn: &mut Connection, root: &Path, id: InstanceId, created: &str,plan:Plan) -> Result<(), InstanceError> {
    plan.validate()?;
    let current=inspect(conn,id,created,plan)?;
    for m in plan.0.iter().filter(|m|m.version>current){
        crate::snapshot::usage(root)?;
        crate::snapshot::create(conn,root,m.version-1,plan.latest())?;
        apply(conn,id,created,*m,plan)?;
    }
    Ok(())
}
