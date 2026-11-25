//! One bounded native owner for the existing desktop contract.
pub mod auth_diagnostics;
pub mod commands;
pub mod intent;
pub mod projection;
#[cfg(windows)]
pub mod connection;
#[cfg(windows)]
pub mod lifecycle;

#[cfg(windows)]
pub fn run() -> Result<i32, String> {
    use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
    use tauri::menu::{Menu, MenuItem, Submenu};

    if std::env::args_os().nth(1).is_some() {
        return Err("Launch Yokai using its saved desktop enrollment.".into());
    }
    if std::env::vars_os().any(|(key, _)| key.to_string_lossy().to_ascii_uppercase().starts_with("WEBVIEW2_")) {
        return Err("WebView2 environment overrides refused.".into());
    }
    let app = tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![commands::view_snapshot, commands::connect, commands::submit, commands::reconcile, commands::save_draft])
        .menu(|app| {
            let close = MenuItem::with_id(app, "close-window", "Close", true, Some("Ctrl+W"))?;
            let shutdown = MenuItem::with_id(app, "shutdown-runtime", "Shut down runtime", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit-yokai", "Quit Yokai", true, Some("Ctrl+Q"))?;
            let file = Submenu::with_items(app, "File", true, &[&close, &shutdown, &quit])?;
            Menu::with_items(app, &[&file])
        })
        .on_menu_event(|app, event| match event.id().as_ref() {
            "close-window" => {
                if let Some(window) = app.get_webview_window("main")
                    && let Err(error) = window.close()
                {
                    eprintln!("Cannot request desktop close: {error}");
                }
            }
            "shutdown-runtime" => {
                if let Some(bridge) = app.try_state::<connection::Bridge>() {
                    let _ = bridge.admit(commands::Action::Shutdown);
                }
            }
            "quit-yokai" => lifecycle::request_quit(),
            _ => (),
        })
        .setup(|app| {
            let launch = lifecycle::Launch::from_app_data(&app.path().app_local_data_dir()?)
                .map_err(std::io::Error::other)?;
            let bridge = connection::Bridge::start(Some(launch.runtime.clone()), &launch.client)
                .map_err(std::io::Error::other)?;
            app.manage(bridge);
            let built = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("Yokai").inner_size(1100.0, 760.0)
                .data_directory(launch.profile.clone())
                .use_https_scheme(true).devtools(false)
                .on_navigation(|url| url.as_str() == commands::DOCUMENT)
                .on_new_window(|_, _| tauri::webview::NewWindowResponse::Deny)
                .build();
            match built {
                Ok(window) => {
                    if let Err(reason)=lifecycle::install(window, launch) {
                        app.state::<connection::Bridge>().native_failure();
                        eprintln!("STOP | {reason} Retain case and window; native departure is unverified.");
                    }
                }
                Err(error) => {
                    let bridge = app.state::<connection::Bridge>(); bridge.request_close();
                    let end = std::time::Instant::now() + std::time::Duration::from_secs(2);
                    while !bridge.join_finished().map_err(std::io::Error::other)? {
                        if std::time::Instant::now() >= end { return Err(std::io::Error::other("Worker departure unresolved after window failure.").into()); }
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    return Err(error.into());
                }
            }
            Ok(())
        })
        .build(tauri::generate_context!()).map_err(|e| format!("Native host setup failed: {e}"))?;
    let result = app.run_return(|_, event| lifecycle::event(event));
    lifecycle::returned()?;
    Ok(result)
}

#[cfg(not(windows))]
pub fn run() -> Result<i32, String> { Err("This native host slice requires Windows.".into()) }
