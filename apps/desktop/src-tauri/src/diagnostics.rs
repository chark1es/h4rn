//! Token-free diagnostics for the desktop shell.
//!
//! The shell is deliberately quiet: it has no console in release builds, so
//! startup failures would otherwise be invisible. Messages are mirrored to
//! stderr and appended to the platform log directory. Callers must never pass
//! access tokens, endpoint query strings or other secrets to these functions.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tauri::Manager;

/// Start a fresh file once the log grows past this size.
const MAX_LOG_BYTES: u64 = 512 * 1024;

static LOG_FILE: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Points diagnostics at the platform log directory.
///
/// Call once during setup, before anything worth reporting happens.
pub fn init(app: &tauri::AppHandle) {
    let file = app
        .path()
        .app_log_dir()
        .ok()
        .and_then(|directory| prepare(&directory));
    let _ = LOG_FILE.set(file);
}

/// Records an informational message.
pub fn info(message: &str) {
    record("info", message);
}

/// Records a warning.
pub fn warn(message: &str) {
    record("warn", message);
}

fn prepare(directory: &Path) -> Option<PathBuf> {
    std::fs::create_dir_all(directory).ok()?;
    let path = directory.join("shell.log");
    if std::fs::metadata(&path)
        .map(|metadata| metadata.len() > MAX_LOG_BYTES)
        .unwrap_or(false)
    {
        let _ = std::fs::remove_file(&path);
    }
    Some(path)
}

fn record(level: &str, message: &str) {
    eprintln!("[h4rn] {level}: {message}");

    let Some(Some(path)) = LOG_FILE.get() else {
        return;
    };
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let _ = writeln!(file, "{level}\t{message}");
}
