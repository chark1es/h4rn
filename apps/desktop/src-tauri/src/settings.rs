//! Shell settings that must be known before the UI loads.
//!
//! "Keep running in tray" changes what the window's close button does, so the
//! Rust shell reads it during setup and owns the file. The view reads and
//! writes the same value through `shell_settings` and
//! `shell_set_keep_in_tray`. The other startup switch, "Open at login", is the
//! autostart plugin's own registration and needs no file here.
//!
//! A tray icon can fail to be created (no appindicator, a locked-down desktop,
//! an unsupported session). Hiding the window when there is no tray would
//! leave the app with no window and no way back, so the shell records whether
//! the tray exists and only honours the choice when it does.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use crate::diagnostics;

/// File name under the app's configuration directory.
const FILE_NAME: &str = "shell.json";

/// The shell's persisted choices.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShellSettings {
    /// Closing the main window hides it instead of quitting.
    pub keep_in_tray: bool,
}

/// What the view reads: the persisted choice plus live shell capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ShellSettingsView {
    pub keep_in_tray: bool,
    /// Whether this run actually created a tray icon.
    pub tray_available: bool,
}

/// The copy the window handler reads on every close request.
#[derive(Default)]
pub struct ShellSettingsState(pub Mutex<ShellSettings>);

/// Whether the tray icon was created during setup.
#[derive(Default)]
pub struct TrayAvailability(pub AtomicBool);

fn lock(state: &ShellSettingsState) -> MutexGuard<'_, ShellSettings> {
    state.0.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether the close button should hide the window instead of quitting.
///
/// Both the choice and a working tray are required; otherwise the window
/// would disappear with no way to bring it back.
pub fn hides_on_close(keep_in_tray: bool, tray_available: bool) -> bool {
    keep_in_tray && tray_available
}

/// Whether the close button should hide the window in this run.
pub fn keep_in_tray(state: &ShellSettingsState, tray: &TrayAvailability) -> bool {
    hides_on_close(lock(state).keep_in_tray, tray.0.load(Ordering::Relaxed))
}

fn file_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_config_dir()
        .map(|directory| directory.join(FILE_NAME))
        .map_err(|error| format!("the app configuration directory is unavailable: {error}"))
}

fn read_from(path: &Path) -> ShellSettings {
    let Ok(raw) = fs::read_to_string(path) else {
        return ShellSettings::default();
    };
    match serde_json::from_str::<ShellSettings>(&raw) {
        Ok(settings) => settings,
        Err(error) => {
            diagnostics::warn(&format!(
                "ignoring unreadable shell settings at {}: {error}",
                path.display()
            ));
            ShellSettings::default()
        }
    }
}

fn write_to(path: &Path, settings: ShellSettings) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(&settings)
        .map_err(|error| format!("could not encode the shell settings: {error}"))?;
    fs::write(path, format!("{body}\n"))
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

/// Reads the persisted settings, falling back to "quit on close".
pub fn load(app: &AppHandle) -> ShellSettings {
    file_path(app)
        .map(|path| read_from(&path))
        .unwrap_or_default()
}

fn view(settings: ShellSettings, tray: &TrayAvailability) -> ShellSettingsView {
    ShellSettingsView {
        keep_in_tray: settings.keep_in_tray,
        tray_available: tray.0.load(Ordering::Relaxed),
    }
}

/// What the view reads on mount.
#[tauri::command]
pub fn shell_settings(
    state: State<'_, ShellSettingsState>,
    tray: State<'_, TrayAvailability>,
) -> ShellSettingsView {
    view(*lock(&state), &tray)
}

/// Persists the tray choice and returns what is now in force.
#[tauri::command]
pub fn shell_set_keep_in_tray(
    app: AppHandle,
    state: State<'_, ShellSettingsState>,
    tray: State<'_, TrayAvailability>,
    enabled: bool,
) -> Result<ShellSettingsView, String> {
    if enabled && !tray.0.load(Ordering::Relaxed) {
        // Enabling this without a tray would make the close button hide the
        // only window. Refuse instead of persisting a trap.
        return Err(
            "No tray icon is available in this session, so closing the window would leave no way back."
                .into(),
        );
    }
    let next = ShellSettings {
        keep_in_tray: enabled,
    };
    write_to(&file_path(&app)?, next)?;
    *lock(&state) = next;
    diagnostics::info(if enabled {
        "closing the window now keeps h4rn in the tray"
    } else {
        "closing the window now quits h4rn"
    });
    Ok(view(next, &tray))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_path() -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!(
            "h4rn-shell-settings-{}-{stamp}/shell.json",
            std::process::id()
        ))
    }

    #[test]
    fn round_trips_the_tray_choice() {
        let path = scratch_path();
        write_to(&path, ShellSettings { keep_in_tray: true }).expect("write");
        assert_eq!(read_from(&path), ShellSettings { keep_in_tray: true });
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn a_missing_or_broken_file_means_quit_on_close() {
        let path = scratch_path();
        assert_eq!(read_from(&path), ShellSettings::default());

        fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        fs::write(&path, "{not json").expect("write");
        assert_eq!(read_from(&path), ShellSettings::default());
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn a_file_without_the_field_keeps_the_default() {
        let path = scratch_path();
        fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        fs::write(&path, "{}\n").expect("write");
        assert!(!read_from(&path).keep_in_tray);
        let _ = fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn a_missing_tray_never_hides_the_only_window() {
        assert!(!hides_on_close(true, false));
        assert!(hides_on_close(true, true));
        assert!(!hides_on_close(false, true));
    }

    #[test]
    fn the_view_reports_what_the_shell_can_do() {
        let tray = TrayAvailability(AtomicBool::new(false));
        assert!(!view(ShellSettings { keep_in_tray: true }, &tray).tray_available);
        tray.0.store(true, Ordering::Relaxed);
        assert!(view(ShellSettings { keep_in_tray: true }, &tray).tray_available);
    }
}
