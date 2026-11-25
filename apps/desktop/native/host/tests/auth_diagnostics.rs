use yokai_desktop::auth_diagnostics::{AuthJournal, AuthOutcome, AuthTicket, MAX_AUTH_ATTEMPTS};
use yokai_desktop_contract::FailureKind;

#[test]
fn records_success_only_after_remote_and_running_status() {
    let mut log = AuthJournal::default();
    let key = log.begin(1).unwrap();
    log.remote_entered(key).unwrap();
    log.remote_succeeded(key).unwrap();
    log.status_confirmed(key).unwrap();
    log.finish(key, None).unwrap();
    let row = &log.records()[0];
    assert_eq!(row.sequence, 1);
    assert_eq!(row.generation, 1);
    assert_eq!(row.outcome, AuthOutcome::Connected);
    assert!(row.remote_succeeded && row.running_status_confirmed);
    assert!(row.failure.is_none() && !row.retained_io);
    assert!(log.require_complete().is_ok());
}

#[test]
fn remote_failure_is_distinct_from_status_failure() {
    let mut log = AuthJournal::default();
    let first = log.begin(1).unwrap();
    log.remote_entered(first).unwrap();
    log.finish(first, Some((FailureKind::Authentication, false))).unwrap();
    let second = log.begin(2).unwrap();
    log.remote_entered(second).unwrap();
    log.remote_succeeded(second).unwrap();
    log.finish(second, Some((FailureKind::Timeout, false))).unwrap();
    assert!(!log.records()[0].remote_succeeded);
    assert!(log.records()[1].remote_succeeded);
    assert!(log.records().iter().all(|r| !r.running_status_confirmed && r.outcome == AuthOutcome::Failed));
}

#[test]
fn false_success_faults_the_capture_without_rewriting_history() {
    for remote in [false, true] {
        let mut log = AuthJournal::default();
        let key = log.begin(1).unwrap();
        if remote { log.remote_entered(key).unwrap(); log.remote_succeeded(key).unwrap(); }
        assert!(log.finish(key, None).is_err());
        assert!(log.faulted());
        assert_eq!(log.records()[0].outcome, AuthOutcome::Started);
        assert!(log.require_complete().is_err());
    }
}

#[test]
fn stale_generation_or_sequence_never_updates_current_attempt() {
    for wrong in [AuthTicket { sequence: 1, generation: 2 }, AuthTicket { sequence: 2, generation: 1 }] {
        let mut log = AuthJournal::default();
        let current = log.begin(1).unwrap();
        log.remote_entered(current).unwrap();
        assert!(log.remote_succeeded(wrong).is_err());
        assert!(!log.records()[0].remote_succeeded);
        assert!(log.faulted());
    }
}

#[test]
fn terminal_outcome_can_be_assigned_only_once() {
    let mut log = AuthJournal::default();
    let key = log.begin(1).unwrap();
    log.finish(key, Some((FailureKind::Authentication, false))).unwrap();
    assert!(log.finish(key, Some((FailureKind::Transport, true))).is_err());
    assert!(matches!(log.records()[0].failure.as_ref(), Some(FailureKind::Authentication)));
    assert!(!log.records()[0].retained_io);
    assert!(log.faulted());
}

#[test]
fn retained_io_is_recorded_without_asserting_closure() {
    let mut log = AuthJournal::default();
    let key = log.begin(1).unwrap();
    log.finish(key, Some((FailureKind::Authentication, true))).unwrap();
    assert!(log.records()[0].retained_io);
    assert!(log.require_complete().is_err());
}

#[test]
fn pending_attempt_cannot_be_overwritten_or_finalized() {
    let mut log = AuthJournal::default();
    log.begin(1).unwrap();
    assert!(log.require_complete().is_err());
    assert!(log.begin(2).is_err());
    assert_eq!(log.records().len(), 1);
    assert!(log.faulted());
}

#[test]
fn repeated_generation_and_zero_generation_are_refused() {
    let mut zero = AuthJournal::default();
    assert!(zero.begin(0).is_err());
    assert!(zero.records().is_empty());
    let mut log = AuthJournal::default();
    let key = log.begin(7).unwrap();
    log.finish(key, Some((FailureKind::Transport, false))).unwrap();
    assert!(log.begin(7).is_err());
    assert_eq!(log.records().len(), 1);
}

#[test]
fn status_before_remote_and_duplicate_remote_are_refused() {
    let mut log = AuthJournal::default();
    let key = log.begin(1).unwrap();
    assert!(log.status_confirmed(key).is_err());
    let mut log = AuthJournal::default();
    let key = log.begin(1).unwrap();
    log.remote_entered(key).unwrap();
    log.remote_succeeded(key).unwrap();
    assert!(log.remote_succeeded(key).is_err());
}

#[test]
fn history_has_fixed_count_and_encoded_size_bounds() {
    let mut log = AuthJournal::default();
    for generation in 1..=MAX_AUTH_ATTEMPTS as u64 {
        let key = log.begin(generation).unwrap();
        log.finish(key, Some((FailureKind::Authentication, false))).unwrap();
    }
    assert!(log.require_complete().is_ok());
    assert!(serde_json::to_vec(&log).unwrap().len() < 8192);
    assert!(log.begin(MAX_AUTH_ATTEMPTS as u64 + 1).is_err());
    assert_eq!(log.records().len(), MAX_AUTH_ATTEMPTS);
    assert!(log.faulted());
}

#[test]
fn complete_empty_history_is_valid_for_ownership_only_host() {
    assert!(AuthJournal::default().require_complete().is_ok());
}

#[test]
fn failure_after_status_keeps_confirmed_facts_without_claiming_connect_completion() {
    let mut log = AuthJournal::default();
    let key = log.begin(1).unwrap();
    log.remote_entered(key).unwrap();
    log.remote_succeeded(key).unwrap();
    log.status_confirmed(key).unwrap();
    log.finish(key, Some((FailureKind::ProjectionLimit, false))).unwrap();
    assert!(log.records()[0].running_status_confirmed);
    assert_eq!(log.records()[0].outcome, AuthOutcome::Failed);
}

#[test]
fn preflight_failure_is_not_reported_as_a_remote_attempt() {
    let mut log = AuthJournal::default();
    let key = log.begin(1).unwrap();
    log.finish(key, Some((FailureKind::Transport, false))).unwrap();
    assert!(!log.records()[0].remote_entered);
    assert!(!log.records()[0].remote_succeeded);
    assert_eq!(log.records()[0].outcome, AuthOutcome::Failed);
}
