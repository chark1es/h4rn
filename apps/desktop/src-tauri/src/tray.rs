//! The tray icon and the window's close behaviour.
//!
//! With "Keep running in tray" on, the close button hides the window and the
//! daemon (and every running turn) stays alive. With it off, closing the window
//! quits the app as before. The tray menu always offers Open h4rn and Quit.
//! If this icon could not be created, closing always quits: hiding the only
//! window would leave no way back.

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WindowEvent};

use crate::settings::{self, ShellSettingsState, TrayAvailability};

/// What a dock click or second launch asks the shell to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub enum Reveal {
    /// Show the main window, restore it and focus it.
    ShowAndFocus,
}

/// The action for a reopen request.
///
/// macOS can report a visible window that is merely behind another app, and a
/// tray-only app has no other way back, so both answers reveal the window.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn reopen_action(_has_visible_windows: bool) -> Reveal {
    Reveal::ShowAndFocus
}

/// Brings the main window back from the tray.
pub fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Adds the Open h4rn / Quit tray icon.
pub fn create(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open h4rn", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &quit])?;

    let mut builder = TrayIconBuilder::with_id("h4rn")
        .tooltip("h4rn")
        .menu(&menu)
        // A left click is "show me the app"; the menu is on the right click.
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_main(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main(tray.app_handle());
            }
        });

    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

/// Makes the close button respect the persisted tray choice.
pub fn watch_window(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let handle = app.clone();
    let target = window.clone();
    window.on_window_event(move |event| {
        if let WindowEvent::CloseRequested { api, .. } = event {
            let state = handle.state::<ShellSettingsState>();
            let tray = handle.state::<TrayAvailability>();
            if settings::keep_in_tray(&state, &tray) {
                api.prevent_close();
                let _ = target.hide();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reopen_always_shows_and_focuses_the_window() {
        // macOS can send Reopen with `has_visible_windows: true` when the
        // window is behind other apps; both cases must reveal it, because a
        // tray-only app has no other way back.
        assert_eq!(reopen_action(false), Reveal::ShowAndFocus);
        assert_eq!(reopen_action(true), Reveal::ShowAndFocus);
    }
}
