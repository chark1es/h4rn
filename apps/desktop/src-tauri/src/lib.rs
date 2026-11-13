//! h4rn desktop shell.
//!
//! The Tauri layer is deliberately thin (ADR-001): it owns window creation, the
//! local daemon's lifecycle, and the OS side-channels — deep links, opening
//! URLs, keychain access — while every piece of product behaviour stays in the
//! Rust daemon behind the WebSocket protocol.
//!
//! Startup sequence:
//!
//! 1. window-state and deep-link registration run during setup;
//! 2. `daemon::bootstrap` starts the bundled `h4rnd` sidecar when nothing
//!    answers on `127.0.0.1:47800`, reads its access token, and loads the UI
//!    (index.html at the origin root) with `?daemon=<ws_url>&token=<token>`;
//! 3. on exit, only the daemon this shell spawned is stopped.

mod commands;
mod daemon;
mod diagnostics;
mod platform;

use tauri::{Manager, RunEvent};

pub use daemon::DaemonState;

/// Build and run the desktop shell.
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_deep_link::init())
        .plugin(
            tauri_plugin_window_state::Builder::default()
                // Size, position and maximised state remember the user's
                // layout; visibility stays owned by the shell so the window
                // never flashes before the daemon endpoint is known.
                .with_state_flags(
                    tauri_plugin_window_state::StateFlags::all()
                        & !tauri_plugin_window_state::StateFlags::VISIBLE,
                )
                .build(),
        )
        .manage(DaemonState::default())
        .invoke_handler(tauri::generate_handler![
            commands::daemon_endpoints,
            commands::open_external,
            commands::keychain_get,
            commands::keychain_set,
            commands::keychain_delete,
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            diagnostics::init(&handle);

            #[cfg(desktop)]
            {
                use tauri::Emitter;
                use tauri_plugin_deep_link::DeepLinkExt;

                if let Err(error) = app.deep_link().register_all() {
                    diagnostics::warn(&format!(
                        "could not register the h4rn:// deep link: {error}"
                    ));
                }

                let deep_link_handle = app.handle().clone();
                app.deep_link().on_open_url(move |event| {
                    if let Some(window) = deep_link_handle.get_webview_window("main") {
                        let urls: Vec<String> = event
                            .urls()
                            .into_iter()
                            .map(|url| url.to_string())
                            .collect();
                        let _ = window.emit("h4rn://deep-link", urls);
                    }
                });
            }

            tauri::async_runtime::spawn(async move {
                daemon::bootstrap(handle).await;
            });

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building the h4rn desktop shell");

    app.run(|handle, event| {
        if matches!(event, RunEvent::Exit) {
            handle.state::<DaemonState>().shutdown();
        }
    });
}
