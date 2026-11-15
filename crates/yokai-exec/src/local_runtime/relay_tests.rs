use super::{IoError, RelaySite, Relays};
use std::{io, thread, time::{Duration, Instant}};

fn finished(worker: thread::JoinHandle<()>) -> thread::JoinHandle<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !worker.is_finished() {
        assert!(Instant::now() < deadline, "relay test worker did not finish");
        thread::yield_now();
    }
    worker
}

#[test]
fn failed_join_stays_latched_after_removal_and_empty_reaps() {
    let mut relays = Relays::new();
    relays.workers.push(finished(thread::spawn(|| panic!("synthetic relay panic"))));
    assert_eq!(relays.reap(RelaySite::ActiveLoop), Err(RelaySite::ActiveLoop));
    assert!(relays.workers.is_empty());
    assert_eq!(relays.reap(RelaySite::Idle), Err(RelaySite::ActiveLoop));
    assert_eq!(relays.reap(RelaySite::Drain), Err(RelaySite::ActiveLoop));
}

#[test]
fn active_admission_fault_survives_reader_result() {
    let mut relays = Relays::new();
    relays.workers.push(finished(thread::spawn(|| panic!("synthetic admission relay panic"))));
    assert_eq!(relays.reap(RelaySite::ActiveAdmission), Err(RelaySite::ActiveAdmission));
    let cleanup_error: Result<(), _> = Err(io::Error::other("reader cleanup error").into());
    assert_eq!(relays.session_retention_reason(&cleanup_error),
               Some("progress relay failed during active session"));
    assert_eq!(relays.session_retention_reason(&Ok(())),
               Some("progress relay failed during active session"));
    assert_eq!(relays.session_retention_reason(&Err::<(), _>(IoError::PendingRetained)),
               Some("session I/O unconfirmed"));
}

#[test]
fn idle_and_drain_faults_keep_their_terminal_reason() {
    for (site, expected) in [
        (RelaySite::Idle, "progress relay failed"),
        (RelaySite::Drain, "progress relay failed during drain"),
    ] {
        let mut relays = Relays::new();
        relays.workers.push(finished(thread::spawn(|| panic!("synthetic relay panic"))));
        assert_eq!(relays.reap(site).unwrap_err().reason(), expected);
        assert_eq!(relays.reap(RelaySite::ActiveLoop), Err(site));
    }
}

#[test]
fn clean_reap_and_ordinary_disconnect_remain_recoverable() {
    let mut relays = Relays::new();
    relays.workers.push(finished(thread::spawn(|| {})));
    assert_eq!(relays.reap(RelaySite::ActiveLoop), Ok(()));
    assert!(relays.workers.is_empty());
    let disconnect: Result<(), _> = Err(io::Error::other("ordinary transport error").into());
    assert_eq!(relays.session_retention_reason(&disconnect), None);
    assert_eq!(relays.session_retention_reason(&Ok(())), None);
}
