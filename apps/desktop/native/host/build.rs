fn main() {
    const COMMANDS: &[&str] = &["view_snapshot", "connect", "submit", "reconcile", "save_draft"];
    let attributes = tauri_build::Attributes::new()
        .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS));
    tauri_build::try_build(attributes).expect("desktop configuration or ACL generation failed");
}
