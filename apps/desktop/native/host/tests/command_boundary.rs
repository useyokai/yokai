use yokai_desktop::{commands::{caller_allowed, parse}, projection::BridgeState};
use serde_json::json;

#[test]
fn only_exact_local_main_document_is_admitted() {
    assert!(caller_allowed("main", "https://tauri.localhost/"));
    for url in ["http://tauri.localhost/", "https://tauri.localhost/other", "https://tauri.localhost/?x=1", "https://example.com/"] {
        assert!(!caller_allowed("main", url));
    }
    assert!(!caller_allowed("other", "https://tauri.localhost/"));
    for name in ["shutdown", "validation", "execute", "read_file"] {
        assert!(parse(name, &json!({})).is_err());
    }
}

#[test]
fn close_and_native_failure_disable_all_mutations() {
    let mut state = BridgeState::new(true, String::new(), None);
    state.close();
    let view = state.snapshot().unwrap();
    assert!(!view.availability.connect && !view.availability.submit && !view.availability.reconcile && !view.availability.save_draft);
    assert!(state.queued.is_none());
}

#[cfg(windows)]
#[test]
fn raw_ipc_and_unrequested_window_destruction_are_refused() {
    assert!(yokai_desktop::commands::parse_ipc("submit",&tauri::ipc::InvokeBody::Raw(b"{}".to_vec())).is_err());
    for (closing,requested) in [(false,false),(true,false),(false,true)] {
        assert!(yokai_desktop::lifecycle::controller_destroyed(closing,requested).is_err());
    }
    assert!(yokai_desktop::lifecycle::controller_destroyed(true,true).is_ok());
}
