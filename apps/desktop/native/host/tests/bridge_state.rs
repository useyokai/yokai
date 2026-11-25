use yokai_desktop::{commands::Action, projection::{BridgeState, Replay}};
use yokai_ipc::Event;
use yokai_types::{ConversationId, InstanceId, MessageId};

#[test]
fn reservation_disables_duplicate_activation_before_worker_pickup() {
    let mut state = BridgeState::new(true, String::new(), None);
    state.native_ready = true;
    assert!(state.reserve(Action::Connect).is_ok());
    assert!(!state.snapshot().unwrap().availability.connect);
    assert!(state.reserve(Action::Connect).is_err());
}

#[test]
fn late_ticket_cannot_replace_close_state() {
    let mut state = BridgeState::new(true, String::new(), None);
    state.native_ready = true;
    let ticket = state.reserve(Action::Connect).unwrap();
    state.close();
    assert!(!state.matches(ticket));
}

#[test]
fn replay_pairs_original_user_message_to_assistant_by_run() {
    let user = MessageId::from_bytes([0x11; 16]).to_string();
    let assistant = MessageId::from_bytes([0x22; 16]).to_string();
    let mut replay = Replay::new(ConversationId::from_bytes([0x33; 16]).to_string());
    replay.page(vec![
        Event { sequence: 2, run: 1, kind: "created".into(), message: None, text: None },
        Event { sequence: 4, run: 2, kind: "submitted".into(), message: Some(user.clone()), text: Some("hello".into()) },
        Event { sequence: 5, run: 2, kind: "completed".into(), message: Some(assistant), text: Some("reply".into()) },
    ], 5, false).unwrap();
    assert_eq!(replay.view.committed.len(), 1);
    assert_eq!(replay.view.committed[0].message_id, user);
    assert!(replay.page(vec![Event { sequence: 5, run: 2, kind: "completed".into(), message: None, text: None }], 5, false).is_err());
}

#[test]
fn replay_rejects_unknown_events_and_unbounded_pages() {
    let mut replay = Replay::new(ConversationId::from_bytes([0x33; 16]).to_string());
    assert!(replay.page(vec![Event { sequence: 1, run: 1, kind: "surprise".into(), message: None, text: None }], 1, false).is_err());
    let mut replay = Replay::new(ConversationId::from_bytes([0x33; 16]).to_string());
    assert!(replay.page(Vec::new(), 0, true).is_err());

    let canonical = MessageId::from_bytes([0xab; 16]).to_string();
    for malformed in [
        "ab".repeat(16), canonical.to_uppercase(), canonical.replacen('-', "", 1),
        canonical.replacen('-', "_", 1), format!("{canonical}0"),
    ] {
        let mut replay = Replay::new(ConversationId::from_bytes([0x33; 16]).to_string());
        let error = replay.page(vec![
            Event { sequence: 1, run: 1, kind: "created".into(), message: None, text: None },
            Event { sequence: 2, run: 2, kind: "submitted".into(), message: Some(malformed.clone()), text: Some("hello".into()) },
        ], 2, false).unwrap_err();
        assert_eq!(error, "Invalid identity.");

        let mut replay = Replay::new(ConversationId::from_bytes([0x33; 16]).to_string());
        let error = replay.page(vec![
            Event { sequence: 1, run: 1, kind: "created".into(), message: None, text: None },
            Event { sequence: 2, run: 2, kind: "submitted".into(), message: Some(canonical.clone()), text: Some("hello".into()) },
            Event { sequence: 3, run: 2, kind: "completed".into(), message: Some(malformed), text: Some("reply".into()) },
        ], 3, false).unwrap_err();
        assert_eq!(error, "Invalid identity.");
    }
}

#[test]
fn close_between_write_and_receive_refuses_the_next_io() {
    let mut state=BridgeState::new(true,String::new(),None);
    state.claim_io().unwrap();
    state.close();
    state.finish_io(false);
    assert!(state.claim_io().is_err());
    assert!(!state.io_inflight);
}

#[test]
fn retained_io_never_reopens_admission() {
    let mut state=BridgeState::new(true,String::new(),None);
    state.claim_io().unwrap(); state.finish_io(true);
    assert!(state.io_inflight && state.fatal);
    assert!(state.claim_io().is_err());
    assert!(state.reserve(Action::Connect).is_err());
}

#[test]
fn send_entry_is_unknown_and_native_acceptance_survives_unsaved_record() {
    use yokai_desktop::{intent::{Record,Phase},projection::OperationFacts};
    use yokai_desktop_contract::WriteCertainty;
    let mut record=Record::empty(InstanceId::from_bytes([0x11; 16]).to_string()); record.begin("original".into()).unwrap();
    let mut facts=OperationFacts::default();
    assert!(matches!(facts.certainty(record.intent.as_ref()),WriteCertainty::DefinitelyNotWritten));
    facts.send_entered=true;
    assert!(matches!(facts.certainty(record.intent.as_ref()),WriteCertainty::Unknown));
    record.intent.as_mut().unwrap().phase=Phase::Submitting;
    facts.accepted_run=Some(7);
    // Simulate persistence refusal: the prior record still has no accepted run.
    assert!(record.intent.as_ref().unwrap().accepted_run.is_none());
    assert!(matches!(facts.certainty(record.intent.as_ref()),WriteCertainty::DurablyAccepted));
    facts.committed=true;
    assert!(matches!(facts.certainty(None),WriteCertainty::DurablyAccepted));
}

#[test]
fn saved_action_reply_is_not_replaced_by_fast_completion() {
    use yokai_desktop::{intent::Record,projection::{pending,Response}};
    let mut record=Record::empty(InstanceId::from_bytes([0x11; 16]).to_string()); record.begin("original".into()).unwrap();
    let mut state=BridgeState::new(true,String::new(),Some(pending(&record.instance,record.intent.as_ref().unwrap())));
    state.response=Some(Response { ready:None,waker:None });
    let saved=state.snapshot(); state.response_ready(saved);
    state.view.pending=None;
    let completed=state.snapshot(); state.response_ready(completed);
    let response=state.response.take().unwrap().ready.unwrap().unwrap();
    assert_eq!(response.pending.unwrap().prompt,"original");
    assert!(response.draft.is_empty());
}

#[test]
fn closing_cancels_waiting_persistence_reply() {
    use yokai_desktop::projection::Response;
    let mut state=BridgeState::new(true,String::new(),None);
    state.response=Some(Response { ready:None,waker:None }); state.close();
    assert!(state.response.unwrap().ready.unwrap().is_err());
}

#[test]
fn accepted_submission_is_not_a_committed_turn() {
    let mut replay=Replay::new(ConversationId::from_bytes([0x33; 16]).to_string());
    replay.page(vec![
        Event { sequence:1,run:1,kind:"created".into(),message:None,text:None },
        Event { sequence:2,run:2,kind:"submitted".into(),message:Some(MessageId::from_bytes([0x11; 16]).to_string()),text:Some("hello".into()) },
    ],2,false).unwrap();
    assert!(replay.runs.contains_key(&2));
    assert!(replay.view.committed.is_empty());
    assert!(!replay.completed_runs.contains(&2));
}

#[test]
fn replay_requires_completion_within_the_page_bound() {
    let mut replay=Replay::new(ConversationId::from_bytes([0x33; 16]).to_string());
    replay.page(vec![Event {sequence:1,run:1,kind:"created".into(),message:None,text:None}],1,true).unwrap();
    for page in 2..=7 {
        replay.page(vec![Event {sequence:page,run:page,kind:"submitted".into(),message:Some(MessageId::from_bytes((page as u128).to_be_bytes()).to_string()),text:Some("x".into())}],page,true).unwrap();
    }
    assert!(!replay.view.at_history_end);
    assert_eq!(replay.page(vec![Event {sequence:8,run:8,kind:"submitted".into(),message:Some(MessageId::from_bytes(8u128.to_be_bytes()).to_string()),text:Some("x".into())}],8,true).unwrap_err(), "Replay pagination unfinished at bound.");
}

#[test]
fn combined_view_is_validated_before_retiring_original_intent() {
    use yokai_desktop::{intent::Record,projection::{reconciled,VIEW_MAX}};
    use yokai_desktop_contract::{ConversationView,CommittedTurn};
    let mut record=Record::empty(InstanceId::from_bytes([0x11; 16]).to_string()); record.begin("original".into()).unwrap();
    let original=record.bytes().unwrap();
    let mut next=record.clone(); next.intent=None; next.draft="d".repeat(16384);
    let history=ConversationView {conversation_id:record.conversation.clone().unwrap(),committed:vec![CommittedTurn {message_id:MessageId::from_bytes([0x22; 16]).to_string(),prompt:"p".repeat(16000),reply:"r".repeat(48000)}],history_cursor:Some("3".into()),at_history_end:true};
    assert!(yokai_ipc::encode(&history,VIEW_MAX).is_ok());
    assert!(reconciled(&BridgeState::new(true,String::new(),None).view,&next,history).is_err());
    assert_eq!(record.bytes().unwrap(),original);
}

#[test]
fn restored_conversation_queues_one_connect_at_native_readiness() {
    use yokai_desktop_contract::ConversationView;

    let mut state = BridgeState::new(true, String::new(), None);
    state.view.conversation = Some(ConversationView {
        conversation_id: ConversationId::from_bytes([0x33; 16]).to_string(),
        committed: Vec::new(),
        history_cursor: None,
        at_history_end: false,
    });

    assert!(state.queued.is_none());
    assert!(state.mark_native_ready().unwrap());
    assert!(matches!(state.queued, Some(Action::Connect)));
    assert!(!state.snapshot().unwrap().availability.connect);
    assert!(!state.snapshot().unwrap().availability.submit);

    state.queued.take();
    state.busy = false;
    assert!(!state.mark_native_ready().unwrap());
    assert!(state.queued.is_none());

    let mut fresh = BridgeState::new(true, String::new(), None);
    assert!(!fresh.mark_native_ready().unwrap());
    assert!(fresh.snapshot().unwrap().availability.connect);
    assert!(fresh.queued.is_none());

    let mut closing = BridgeState::new(true, String::new(), None);
    closing.close();
    assert!(closing.mark_native_ready().is_err());
    assert!(closing.queued.is_none());
}

#[test]
fn shutdown_reserves_a_fresh_authentication_generation_and_reports_confirmed_stop() {
    use yokai_desktop_contract::{ConnectionState, FailureKind, WriteCertainty};

    let mut state = BridgeState::new(true, String::new(), None);
    state.native_ready = true;
    state.connected = true;
    state.view.connection = ConnectionState::Connected;
    state.generation = 1;
    state.bump().unwrap();

    let shutdown = state.reserve(Action::Shutdown).unwrap();
    assert_eq!(shutdown.generation, 2);
    assert!(state.reserve(Action::Shutdown).is_err());
    assert!(matches!(state.snapshot().unwrap().connection, ConnectionState::Connected));

    state.confirm_runtime_stopped();
    let stopped = serde_json::to_value(state.snapshot().unwrap()).unwrap();
    assert_eq!(stopped["connection"], "runtime_stopped");
    assert!(!state.connected);
    assert!(!state.snapshot().unwrap().availability.submit);

    state.fail(FailureKind::ShutdownUnconfirmed, WriteCertainty::Unknown, false);
    let uncertain = serde_json::to_value(state.snapshot().unwrap()).unwrap();
    assert_eq!(uncertain["connection"], "blocked");
    assert_eq!(uncertain["failure"]["kind"], "shutdown_unconfirmed");
}

fn connected_state() -> BridgeState {
    use yokai_desktop_contract::ConnectionState;

    let mut state = BridgeState::new(true, String::new(), None);
    state.mark_native_ready().unwrap();
    state.connected = true;
    state.view.connection = ConnectionState::Connected;
    state
}

#[test]
fn quit_waits_for_settled_shutdown_before_closing() {
    let mut state = connected_state();
    assert!(state.request_quit().unwrap());
    assert!(!state.request_quit().unwrap());
    assert!(!state.begin_window_close(true));
    assert!(!state.begin_window_close(false));
    assert!(matches!(state.queued.take(), Some(Action::Shutdown)));

    state.confirm_runtime_stopped();
    assert!(!state.begin_window_close(true));
    state.busy = false;
    assert!(!state.snapshot().unwrap().availability.connect);
    assert!(!state.snapshot().unwrap().availability.save_draft);
    assert!(state.begin_window_close(true));
    assert!(state.closing);
    assert!(!state.begin_window_close(true));
}

#[test]
fn unconfirmed_quit_keeps_the_window_open() {
    use yokai_desktop_contract::{FailureKind, WriteCertainty};

    let mut state = connected_state();
    assert!(state.request_quit().unwrap());
    state.queued.take();
    state.fail(FailureKind::ShutdownUnconfirmed, WriteCertainty::Unknown, false);
    assert!(!state.begin_window_close(true));
    assert!(!state.closing);
    assert!(state.begin_window_close(false));
}

#[test]
fn refused_quit_preserves_the_existing_action() {
    let mut state = connected_state();
    state.reserve(Action::SaveDraft("draft".into())).unwrap();
    assert!(state.request_quit().is_err());
    assert!(!state.begin_window_close(true));
    assert!(!state.closing);
    assert!(matches!(state.queued.take(), Some(Action::SaveDraft(_))));
}

#[test]
fn explicit_shutdown_stays_open_until_quit_or_close() {
    let mut state = connected_state();
    state.reserve(Action::Shutdown).unwrap();
    state.queued.take();
    state.confirm_runtime_stopped();
    state.busy = false;
    assert!(!state.begin_window_close(true));
    assert!(!state.request_quit().unwrap());
    assert!(state.queued.is_none());
    assert!(state.begin_window_close(true));
}
