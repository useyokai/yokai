//! Startup is synchronous under Instance ownership; no public handle exists yet.
use rusqlite::{Connection,OptionalExtension,Transaction,TransactionBehavior,params};
use crate::InstanceError;

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub struct StartupReport {pub epoch:i64,pub runs_examined:u64,pub runs_changed:u64}
impl StartupReport {pub(crate) fn created()->Self{Self{epoch:1,runs_examined:0,runs_changed:0}}}
fn error(e:impl ToString)->InstanceError{InstanceError::Store(format!("startup: {}",e.to_string()))}
fn transaction<T>(conn:&mut Connection,run:impl FnOnce(&Transaction<'_>)->Result<T,InstanceError>)->Result<T,InstanceError>{
    let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(error)?;
    match run(&tx){
        Ok(value)=>{tx.commit().map_err(|e|error(format!("commit outcome unconfirmed; reopen to resolve: {e}")))?;Ok(value)},
        Err(cause)=>match tx.rollback(){Ok(())=>Err(cause),Err(e)=>Err(error(format!("{cause}; rollback unconfirmed: {e}")))},
    }
}
pub(crate) fn recover(conn:&mut Connection)->Result<StartupReport,InstanceError>{
    let epoch=transaction(conn,|tx|{
        let old:i64=tx.query_row("SELECT epoch FROM runtime_epoch WHERE id=1",[],|r|r.get(0)).map_err(error)?;
        let next=old.checked_add(1).ok_or_else(||error("runtime epoch overflow"))?;
        if tx.execute("UPDATE runtime_epoch SET epoch=?1,clean_shutdown=0 WHERE id=1 AND epoch=?2",params![next,old]).map_err(error)?!=1{return Err(error("runtime epoch singleton changed"));}
        tx.execute("INSERT INTO events(kind,detail,epoch) VALUES('runtime.epoch',?1,?2)",params![format!("epoch={next}"),next]).map_err(error)?;
        Ok(next)
    })?;
    crate::instance::hit_failpoint("startup-after-epoch-commit")?;
    let mut report=StartupReport{epoch,runs_examined:0,runs_changed:0};let mut previous:Option<i64>=None;
    loop {
        // No run_id arithmetic, unbounded list, or transaction across multiple runs.
        let row:Option<(i64,String,i64)>=conn.query_row("SELECT run_id,admission,recovery_required FROM runs WHERE status='Open' AND admission<>'Held' AND (?1 IS NULL OR run_id>?1) ORDER BY run_id LIMIT 1",
            [previous],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(error)?;
        let Some((id,admission,recovery))=row else{break};previous=Some(id);report.runs_examined+=1;
        let reason=if recovery==1{"RecoveryRequired"}else if admission=="WaitingApproval"{"ApprovalInvalidated"}else{"StartupInterrupted"};
        transaction(conn,|tx|{
            if tx.execute("UPDATE runs SET admission='Held',hold_reason=?2 WHERE run_id=?1 AND status='Open' AND admission<>'Held'",params![id,reason]).map_err(error)?!=1{return Err(error("startup run changed outside ownership"));}
            tx.execute("INSERT INTO events(run_id,kind,detail,epoch) VALUES(?1,'run.held',?2,?3)",params![id,reason,epoch]).map_err(error)?;
            Ok(())
        })?;
        report.runs_changed+=1;
        #[cfg(feature="test-api")]
        crate::instance::hit_failpoint(&format!("startup-after-hold-{}",report.runs_changed))?;
    }
    Ok(report)
}
