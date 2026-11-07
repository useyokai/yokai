//! Bounded opaque command requests and the single atomic acceptance transaction body.
use std::fmt;
use std::time::Instant;
use rusqlite::{Connection,OptionalExtension,params};
use sha2::{Digest,Sha256};
use yokai_types::CommandId;

/// Clients persist the ID before sending and retry every non-definite result with
/// the same ID and canonical bytes. Canonicalization is the caller's responsibility;
/// a new encoding requires a new operation version. No command dispatch is performed.
#[derive(Clone,Debug)]
pub struct CommandRequest {
    id:CommandId,
    kind:String,
    version:u32,
    payload:Vec<u8>,
    digest:[u8;32],
}
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub struct InvalidCommand(&'static str);
impl fmt::Display for InvalidCommand {fn fmt(&self,f:&mut fmt::Formatter<'_>)->fmt::Result{write!(f,"invalid command {}",self.0)}}
impl std::error::Error for InvalidCommand {}
impl CommandRequest {
    pub fn new(id:CommandId,kind:&str,version:u32,payload:&[u8])->Result<Self,InvalidCommand>{
        if kind.is_empty() || kind.len()>64{return Err(InvalidCommand("kind length"));}
        if !(1..=65535).contains(&version){return Err(InvalidCommand("operation version"));}
        if payload.len()>65536{return Err(InvalidCommand("payload length"));}
        // All lengths are checked before conversion or allocation.
        let mut hash=Sha256::new();hash.update(b"yokai.command.v1\0");
        hash.update((kind.len() as u32).to_be_bytes());hash.update(kind.as_bytes());
        hash.update(version.to_be_bytes());hash.update((payload.len() as u64).to_be_bytes());hash.update(payload);
        Ok(Self{id,kind:kind.to_owned(),version,payload:payload.to_vec(),digest:hash.finalize().into()})
    }
    pub fn command_id(&self)->&CommandId{&self.id}
    pub fn digest(&self)->[u8;32]{self.digest}
}
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum AcceptOutcome { Accepted{run_id:i64}, Duplicate{run_id:i64}, Conflict{run_id:i64} }

/// A wait deadline is independent of the operation deadline. A timeout never
/// cancels handed-off work and is resolved with the original command identity.
pub struct PendingCommand {pub(crate) inner:crate::PendingWrite}
impl PendingCommand {
    pub fn operation_id(&self)->u64{self.inner.operation_id()}
    pub fn wait(self,deadline:Instant)->Result<AcceptOutcome,crate::WriteFailure>{
        let id=self.inner.operation_id();
        self.inner.wait(deadline)?.command.ok_or_else(|| crate::WriteFailure::missing_reply(id))
    }
}

/// Caller owns BEGIN IMMEDIATE and settles COMMIT/ROLLBACK through the existing
/// failure engine. The boolean requests COMMIT for insertion, ROLLBACK for replay.
pub(crate) fn accept(conn:&Connection,request:&CommandRequest)->rusqlite::Result<(AcceptOutcome,bool)>{
    point("accept-after-begin")?;
    if let Some(outcome)=lookup(conn,request)? {return Ok((outcome,false));}
    let (run_id,_)=insert(conn,request)?;
    Ok((AcceptOutcome::Accepted{run_id},true))
}
pub(crate) fn lookup(conn:&Connection,request:&CommandRequest)->rusqlite::Result<Option<AcceptOutcome>>{
    let existing:Option<(Vec<u8>,i64)>=conn.query_row("SELECT request_digest,run_id FROM commands WHERE command_id=?1",
        [request.id.as_str()],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    Ok(existing.map(|(digest,run_id)| if digest.as_slice()==request.digest.as_slice(){
        AcceptOutcome::Duplicate{run_id}
    }else{
        AcceptOutcome::Conflict{run_id}
    }))
}
pub(crate) fn insert(conn:&Connection,request:&CommandRequest)->rusqlite::Result<(i64,i64)>{
    let epoch:i64=conn.query_row("SELECT epoch FROM runtime_epoch WHERE id=1",[],|r|r.get(0))?;
    conn.execute("INSERT INTO runs(admission,status,recovery_required,created_epoch) VALUES('Queued','Open',0,?1)",[epoch])?;
    let run_id=conn.last_insert_rowid();
    point("accept-after-run")?;
    conn.execute("INSERT INTO commands(command_id,kind,op_version,payload,request_digest,run_id,accepted_epoch) VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![request.id.as_str(),request.kind,request.version,request.payload,request.digest.as_slice(),run_id,epoch])?;
    point("accept-after-command")?;
    conn.execute("INSERT INTO jobs(run_id,job_no,state,created_epoch) VALUES(?1,1,'Pending',?2)",params![run_id,epoch])?;
    point("accept-after-job")?;
    conn.execute("INSERT INTO events(run_id,kind,detail,epoch) VALUES(?1,'run.accepted',?2,?3)",params![run_id,request.id.as_str(),epoch])?;
    point("accept-after-event")?;
    Ok((run_id,epoch))
}
pub(crate) fn point(boundary:&str)->rusqlite::Result<()>{
    #[cfg(feature="test-api")]
    if let Err(message)=crate::failpoint::hit(boundary){return Err(rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(10),Some(message)));}
    let _=boundary;Ok(())
}
