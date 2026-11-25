use serde::Deserialize;
use serde_json::Value;
use yokai_desktop_contract::{SaveDraftRequest, SubmitRequest, MAX_TEXT_BYTES};

pub const COMMANDS: &[&str] = &["view_snapshot", "connect", "submit", "reconcile", "save_draft"];
pub const DOCUMENT: &str = "https://tauri.localhost/";

#[derive(Clone, Debug)]
pub enum Action { Snapshot, Connect, Submit(String), Reconcile, SaveDraft(String), Shutdown }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubmitEnvelope { request: SubmitRequest }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DraftEnvelope { request: SaveDraftRequest }

pub fn caller_allowed(label: &str, document: &str) -> bool { label == "main" && document == DOCUMENT }

/// Validate structure and byte length before cloning the already-decoded message.
/// Tauri's initial IPC allocation is outside this wrapper's allocation bound.
pub fn parse(command: &str, body: &Value) -> Result<Action, String> {
    let object = body.as_object().ok_or("Expected an object envelope.")?;
    match command {
        "view_snapshot" | "connect" | "reconcile" => {
            if !object.is_empty() { return Err("Unexpected command arguments.".into()); }
            let _: Empty = serde_json::from_value(body.clone()).map_err(|_| "Invalid empty envelope.")?;
            Ok(match command { "view_snapshot" => Action::Snapshot, "connect" => Action::Connect, _ => Action::Reconcile })
        }
        "submit" | "save_draft" => {
            let inner = object.get("request").and_then(Value::as_object).ok_or("Missing typed request.")?;
            let text = inner.get("text").and_then(Value::as_str).ok_or("Expected text.")?;
            if object.len() != 1 || inner.len() != 1 || text.len() > MAX_TEXT_BYTES { return Err("Request shape or length refused.".into()); }
            if command == "submit" {
                let value: SubmitEnvelope = serde_json::from_value(body.clone()).map_err(|_| "Invalid submit request.")?;
                Ok(Action::Submit(value.request.text().into()))
            } else {
                let value: DraftEnvelope = serde_json::from_value(body.clone()).map_err(|_| "Invalid draft request.")?;
                Ok(Action::SaveDraft(value.request.text().into()))
            }
        }
        _ => Err("Unknown desktop command.".into()),
    }
}

#[cfg(windows)]
async fn invoke(command: &str, request: tauri::ipc::Request<'_>, window: tauri::WebviewWindow, bridge: tauri::State<'_, crate::connection::Bridge>) -> Result<yokai_desktop_contract::ViewSnapshot, String> {
    let url = window.url().map_err(|_| "Caller document unavailable.")?;
    if !caller_allowed(window.label(), url.as_str()) { return Err("Caller refused.".into()); }
    bridge.call(parse_ipc(command,request.body())?).await
}

#[cfg(windows)]
pub fn parse_ipc(command:&str,body:&tauri::ipc::InvokeBody) -> Result<Action,String> {
    let tauri::ipc::InvokeBody::Json(body)=body else { return Err("Raw IPC refused.".into()); };
    parse(command,body)
}

#[cfg(windows)]
#[tauri::command]
pub async fn view_snapshot(request: tauri::ipc::Request<'_>, window: tauri::WebviewWindow, bridge: tauri::State<'_, crate::connection::Bridge>) -> Result<yokai_desktop_contract::ViewSnapshot, String> { invoke("view_snapshot", request, window, bridge).await }
#[cfg(windows)]
#[tauri::command]
pub async fn connect(request: tauri::ipc::Request<'_>, window: tauri::WebviewWindow, bridge: tauri::State<'_, crate::connection::Bridge>) -> Result<yokai_desktop_contract::ViewSnapshot, String> { invoke("connect", request, window, bridge).await }
#[cfg(windows)]
#[tauri::command]
pub async fn submit(request: tauri::ipc::Request<'_>, window: tauri::WebviewWindow, bridge: tauri::State<'_, crate::connection::Bridge>) -> Result<yokai_desktop_contract::ViewSnapshot, String> { invoke("submit", request, window, bridge).await }
#[cfg(windows)]
#[tauri::command]
pub async fn reconcile(request: tauri::ipc::Request<'_>, window: tauri::WebviewWindow, bridge: tauri::State<'_, crate::connection::Bridge>) -> Result<yokai_desktop_contract::ViewSnapshot, String> { invoke("reconcile", request, window, bridge).await }
#[cfg(windows)]
#[tauri::command]
pub async fn save_draft(request: tauri::ipc::Request<'_>, window: tauri::WebviewWindow, bridge: tauri::State<'_, crate::connection::Bridge>) -> Result<yokai_desktop_contract::ViewSnapshot, String> { invoke("save_draft", request, window, bridge).await }
