//! Shell settings that must be known before the UI loads.
//!
//! "Keep running in tray" changes what the window's close button does, so the
//! Rust shell reads it during setup and owns the file. The view reads and
//! writes the same value through `shell_settings` and
//! `shell_set_keep_in_tray`. The other startup switch, "Open at login", is the
//! autostart plugin's own registration and needs no file here.

use std::fs;
use std::path::{Path, PathBuf};
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

/// The copy the window handler reads on every close request.
#[derive(Default)]
pub struct ShellSettingsState(pub Mutex<ShellSettings>);

fn lock(state: &ShellSettingsState) -> MutexGuard<'_, ShellSettings> {
    state.0.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether the close button should hide the window instead of quitting.
pub fn keep_in_tray(state: &ShellSettingsState) -> bool {
    lock(state).keep_in_tray
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

/// What the view reads on mount.
#[tauri::command]
pub fn shell_settings(state: State<'_, ShellSettingsState>) -> ShellSettings {
    *lock(&state)
}

/// Persists the tray choice and returns what is now in force.
#[tauri::command]
pub fn shell_set_keep_in_tray(
    app: AppHandle,
    state: State<'_, ShellSettingsState>,
    enabled: bool,
) -> Result<ShellSettings, String> {
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
    Ok(next)
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
}
