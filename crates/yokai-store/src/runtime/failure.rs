//! Definite outcomes require never-begun or confirmed-rollback evidence.
use super::*;

pub(super) fn classify(conn: &Connection, shared: &Shared, request: &WriteRequest, error: &OperationError) -> WriteFailure {
    let mut evidence = lock(&request.progress).clone();
    evidence.sqlite_code = match error {
        OperationError::Sqlite(error) => crate::ext_code(error),
        #[cfg(feature = "test-api")]
        OperationError::Refused => None,
    };
    let domain = {
        #[cfg(feature = "test-api")]
        { matches!(error, OperationError::Refused) }
        #[cfg(not(feature = "test-api"))]
        { false }
    };
    let auto = conn.is_autocommit();
    evidence.autocommit_after_error = Some(auto);
    let code = evidence.sqlite_code.map(|code| code & 0xff);
    let reason = if domain { WriteReason::Refused } else { match code {
        Some(5) => WriteReason::Busy, Some(19) => WriteReason::Constraint,
        Some(13) => WriteReason::StorageFull, Some(11 | 26) => WriteReason::Corrupt,
        _ => WriteReason::StorageError,
    } };
    if evidence.phase == Phase::Queued {
        if !auto { shared.change(StoreState::Failed); }
        return WriteFailure { operation_id: Some(request.id),
            certainty: if auto { Certainty::NotApplied } else { Certainty::OutcomeUnknown },
            reason: if reason == WriteReason::Busy { reason } else { WriteReason::BeginFailed }, evidence };
    }
    let confirmed = if auto && evidence.phase == Phase::Begun && !domain {
        true // R-a: statement failed before COMMIT, autocommit observed.
    } else if !auto {
        let fail_rollback = {
            #[cfg(feature = "test-api")]
            { matches!(request.work.fault(),Some(
                crate::test_api::WriteFault::RollbackFailure(_) | crate::test_api::WriteFault::Refuse { rollback_fails: true })) }
            #[cfg(not(feature = "test-api"))]
            { false }
        };
        let success = !fail_rollback && conn.execute_batch("ROLLBACK").is_ok();
        evidence.rollback_succeeded = Some(success);
        let after = conn.is_autocommit(); evidence.autocommit_after_rollback = Some(after);
        success && after // R-b: actual successful ROLLBACK and autocommit observed.
    } else { false };
    let (definite, transition) = if evidence.phase == Phase::CommitIssued {
        match reason {
            WriteReason::Busy if !auto && confirmed => (true, None),
            WriteReason::Constraint if !auto && confirmed => (true, Some(StoreState::Degraded)),
            WriteReason::StorageFull => (false, Some(StoreState::Degraded)),
            _ => (false, Some(StoreState::Failed)),
        }
    } else if !confirmed { (false, Some(StoreState::Failed)) }
    else {
        match reason {
            WriteReason::Corrupt => (true, Some(StoreState::Failed)),
            WriteReason::StorageFull | WriteReason::StorageError => (true, Some(StoreState::Degraded)),
            _ => (true, None),
        }
    };
    if let Some(status) = transition { shared.change(status); }
    WriteFailure { operation_id: Some(request.id),
        certainty: if definite { Certainty::NotApplied } else { Certainty::OutcomeUnknown }, reason, evidence }
}
