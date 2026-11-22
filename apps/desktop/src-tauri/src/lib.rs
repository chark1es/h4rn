//! h4rn desktop shell.
//!
//! The Tauri layer is deliberately thin (ADR-001): it owns window creation, the
//! local daemon's lifecycle, and the OS side-channels — deep links, opening
//! URLs, keychain access — while every piece of product behaviour stays in the
//! Rust daemon behind the WebSocket protocol.
//!
//! Startup sequence:
//!
//! 1. window-state, deep-link, notification and autostart registration run
//!    during setup, and the tray choice is read from disk before the window
//!    exists;
//! 2. `daemon::bootstrap` starts the bundled `h4rnd` sidecar when nothing
//!    answers on `127.0.0.1:47800`, reads its access token, and loads the UI
//!    (index.html at the origin root) with `?daemon=<ws_url>&token=<token>`;
//! 3. on exit, only the daemon this shell spawned is stopped.

mod commands;
mod daemon;
mod diagnostics;
mod platform;
mod settings;
mod tray;
mod updates;

use tauri::{Manager, RunEvent};

pub use daemon::DaemonState;
pub use settings::{ShellSettings, ShellSettingsState, TrayAvailability};
pub use updates::UpdateState;

/// Build and run the desktop shell.
pub fn run() {
    let app = tauri::Builder::default()
        // A second launch focuses the running app instead of starting another
        // daemon and window. On Linux the AppIndicator can exist without a
        // tray host ever showing it, so this is the guaranteed way back to a
        // hidden window; macOS dock clicks arrive as `RunEvent::Reopen` below.
        // The plugin's `deep-link` feature hands the second launch's argv to
        // the deep-link plugin first, so `h4rn://` links opened while h4rn
        // runs reach the window that already exists.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            tray::show_main(app);
        }))
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_deep_link::init())
        // The OS notification channel. Clicks come back through
        // `h4rn://notification-click` below.
        .plugin(tauri_plugin_notification::init())
        // Registers this binary as a login item; the view owns the switch.
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        // Signed updates are opt-in: the endpoint and public key live in
        // `plugins.updater`, and release builds replace them from CI secrets.
        // A dev build with the placeholder key simply fails `check()`.
        .plugin(tauri_plugin_updater::Builder::new().build())
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
        .manage(UpdateState::default())
        .invoke_handler(tauri::generate_handler![
            commands::daemon_endpoints,
            commands::auth_loopback,
            commands::open_external,
            commands::keychain_get,
            commands::keychain_set,
            commands::keychain_delete,
            settings::shell_settings,
            settings::shell_set_keep_in_tray,
            updates::app_update_status,
            updates::app_update_check,
            updates::app_update_set_auto,
            updates::app_update_install,
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            diagnostics::init(&handle);

            // The close button's behaviour has to be known before the UI
            // loads, so the shell reads it during setup and owns the file.
            let shell_settings = settings::load(&handle);
            diagnostics::info(if shell_settings.keep_in_tray {
                "keep running in tray is on"
            } else {
                "keep running in tray is off"
            });
            app.manage(ShellSettingsState(std::sync::Mutex::new(shell_settings)));
            // Only a created tray may hide the window: without one, closing
            // would leave the app running with no way to bring it back.
            let tray_available = match tray::create(&handle) {
                Ok(()) => true,
                Err(error) => {
                    diagnostics::warn(&format!("could not create the tray icon: {error}"));
                    false
                }
            };
            app.manage(TrayAvailability(std::sync::atomic::AtomicBool::new(
                tray_available,
            )));
            tray::watch_window(&handle);

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

                // The notification plugin reports a tap here; the view opens
                // the thread named in the notification's `extra`.
                use tauri_plugin_notification::NotificationExt;
                let notification_handle = app.handle().clone();
                if let Err(error) = app.notification().on_action(move |action| {
                    if action.action_id() != "tap" {
                        return;
                    }
                    if let Some(window) = notification_handle.get_webview_window("main") {
                        let _ = window.show();
                        let _ = window.unminimize();
                        let _ = window.set_focus();
                        let _ = window.emit("h4rn://notification-click", action.clone());
                    }
                }) {
                    diagnostics::warn(&format!(
                        "could not listen for notification clicks: {error}"
                    ));
                }
            }

            updates::spawn_background_checks(handle.clone());

            tauri::async_runtime::spawn(async move {
                daemon::bootstrap(handle).await;
            });

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building the h4rn desktop shell");

    app.run(|handle, event| match event {
        RunEvent::Exit => handle.state::<DaemonState>().shutdown(),
        // A dock click on macOS with no visible window (or with one behind
        // other apps) is the same request as a second launch: show and focus
        // the window that "Keep running in tray" hid.
        #[cfg(target_os = "macos")]
        RunEvent::Reopen {
            has_visible_windows,
            ..
        } => {
            if tray::reopen_action(has_visible_windows) == tray::Reveal::ShowAndFocus {
                tray::show_main(handle);
            }
        }
        _ => {}
    });
}
