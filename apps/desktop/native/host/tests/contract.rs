use serde_json::json;
use yokai_desktop::commands::{parse, Action};

#[test]
fn exact_five_envelopes() {
    for command in ["view_snapshot", "connect", "reconcile"] {
        assert!(parse(command, &json!({})).is_ok());
        assert!(parse(command, &json!({"extra": true})).is_err());
    }
    assert!(matches!(parse("submit", &json!({"request":{"text":"hello"}})).unwrap(), Action::Submit(_)));
    assert!(parse("save_draft", &json!({"request":{"text":""}})).is_ok());
    assert!(parse("shutdown", &json!({})).is_err());
}

#[test]
fn rejects_outer_inner_unknown_fields_and_utf8_overflow() {
    assert!(parse("submit", &json!({"request":{"text":"x"},"path":"x"})).is_err());
    assert!(parse("submit", &json!({"request":{"text":"x","id":"x"}})).is_err());
    assert!(parse("submit", &json!({"request":{"text":"  "}})).is_err());
    assert!(parse("save_draft", &json!({"request":{"text":"é".repeat(8193)}})).is_err());
    assert!(parse("submit", &json!(["x"])).is_err());
}

#[test]
fn native_snapshot_uses_the_existing_contract_names_and_bounds() {
    use yokai_desktop::{commands::COMMANDS,projection::BridgeState};
    use yokai_desktop_contract::{ViewSnapshot,FailureKind,WriteCertainty};
    assert_eq!(COMMANDS,&["view_snapshot","connect","submit","reconcile","save_draft"]);
    let mut state=BridgeState::new(true,String::new(),None);
    state.fail(FailureKind::Transport,WriteCertainty::Unknown,false);
    let wire=serde_json::to_value(state.snapshot().unwrap()).unwrap();
    assert_eq!(wire["connection"],"blocked");
    assert_eq!(wire["failure"]["write_certainty"],"unknown");
    assert!(serde_json::from_value::<ViewSnapshot>(wire.clone()).is_ok());
    let mut invalid=wire; invalid["connection"]="invented".into();
    assert!(serde_json::from_value::<ViewSnapshot>(invalid).is_err());
    state.view.draft="\u{0001}".repeat(16384);
    assert!(state.snapshot().is_err());
}
