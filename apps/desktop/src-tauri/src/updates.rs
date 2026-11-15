//! Desktop app updates (ADR-043).
//!
//! The shell checks the configured updater endpoint shortly after launch and
//! then every six hours. A check that cannot run — no endpoint, the development
//! placeholder public key, no network — is logged and otherwise silent: an
//! update notice is a convenience, never an error the person has to read.
//!
//! The view only renders. It asks through `app_update_*` commands and listens
//! for `h4rn://update-checked` and `h4rn://update-progress` events.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::diagnostics;

/// Wait this long after launch so the window and daemon get the first seconds.
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(20);

/// Time between automatic checks.
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Event carrying the outcome of every check.
pub const EVENT_CHECKED: &str = "h4rn://update-checked";

/// Event carrying download and install progress.
pub const EVENT_PROGRESS: &str = "h4rn://update-progress";

/// A newer release the endpoint offers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AvailableUpdate {
    /// Version the endpoint offers, for example `0.2.0`.
    pub version: String,
    /// Release notes, when the manifest has them.
    pub notes: Option<String>,
}

/// What the view needs to draw the notice and Settings → About.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AppUpdateStatus {
    /// Version of the running app.
    pub current_version: String,
    /// A newer release, when one exists.
    pub available: Option<AvailableUpdate>,
    /// Milliseconds since the Unix epoch of the last completed check.
    pub checked_at_ms: Option<u64>,
    /// Whether the background check is enabled.
    pub auto_check: bool,
    /// A short sentence when the last manual or background check failed.
    pub error: Option<String>,
}

/// Download and install progress, emitted as `h4rn://update-progress`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum UpdateProgress {
    /// Bytes received so far; `total` is absent when the server sends no length.
    Downloading {
        /// Bytes received.
        downloaded: u64,
        /// Total bytes, when known.
        total: Option<u64>,
    },
    /// The download finished and the installer is running.
    Installing,
    /// The install failed; the app keeps running.
    Failed {
        /// A sentence for the person.
        message: String,
    },
}

/// Shell-owned updater state.
pub struct UpdateState {
    auto_check: AtomicBool,
    inner: Mutex<Inner>,
    /// Held while an install runs so a second click cannot start another.
    installing: AtomicBool,
}

#[derive(Default)]
struct Inner {
    pending: Option<Update>,
    checked_at_ms: Option<u64>,
    error: Option<String>,
}

impl Default for UpdateState {
    fn default() -> Self {
        Self {
            auto_check: AtomicBool::new(true),
            inner: Mutex::new(Inner::default()),
            installing: AtomicBool::new(false),
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

impl UpdateState {
    fn status(&self, app: &AppHandle) -> AppUpdateStatus {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        AppUpdateStatus {
            current_version: app.package_info().version.to_string(),
            available: inner.pending.as_ref().map(|update| AvailableUpdate {
                version: update.version.clone(),
                notes: update.body.clone(),
            }),
            checked_at_ms: inner.checked_at_ms,
            auto_check: self.auto_check.load(Ordering::Relaxed),
            error: inner.error.clone(),
        }
    }
}

/// One check against the configured endpoint, recording and announcing it.
async fn run_check(app: &AppHandle, state: &UpdateState) -> AppUpdateStatus {
    let outcome = match app.updater() {
        Ok(updater) => updater.check().await.map_err(|error| error.to_string()),
        Err(error) => Err(error.to_string()),
    };
    {
        let mut inner = state
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        inner.checked_at_ms = Some(now_ms());
        match outcome {
            Ok(update) => {
                inner.pending = update;
                inner.error = None;
            }
            Err(error) => {
                // An unset endpoint or the development placeholder key lands
                // here on every dev build; keep it in the log, not in the UI.
                diagnostics::info(&format!("update check did not complete: {error}"));
                inner.error = Some("Couldn't reach the update server.".to_owned());
            }
        }
    }
    let status = state.status(app);
    let _ = app.emit(EVENT_CHECKED, &status);
    status
}

/// Checks on launch and then every six hours while the toggle is on.
pub fn spawn_background_checks(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(FIRST_CHECK_DELAY).await;
        loop {
            let state = app.state::<UpdateState>();
            if state.auto_check.load(Ordering::Relaxed) {
                run_check(&app, &state).await;
            }
            tokio::time::sleep(CHECK_INTERVAL).await;
        }
    });
}

/// The current update status without contacting the server.
#[tauri::command]
pub fn app_update_status(app: AppHandle, state: State<'_, UpdateState>) -> AppUpdateStatus {
    state.status(&app)
}

/// Checks the endpoint now. A failed check is reported in `error`, not thrown.
#[tauri::command]
pub async fn app_update_check(
    app: AppHandle,
    state: State<'_, UpdateState>,
) -> Result<AppUpdateStatus, String> {
    Ok(run_check(&app, &state).await)
}

/// Turns the background check on or off.
#[tauri::command]
pub fn app_update_set_auto(
    app: AppHandle,
    state: State<'_, UpdateState>,
    enabled: bool,
) -> AppUpdateStatus {
    state.auto_check.store(enabled, Ordering::Relaxed);
    state.status(&app)
}

/// Downloads and installs the pending update, then relaunches the app.
#[tauri::command]
pub async fn app_update_install(
    app: AppHandle,
    state: State<'_, UpdateState>,
) -> Result<(), String> {
    let update = state
        .inner
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .pending
        .clone()
        .ok_or_else(|| "There is no update to install.".to_owned())?;
    if state.installing.swap(true, Ordering::SeqCst) {
        return Err("An update is already installing.".to_owned());
    }
    let downloaded = Arc::new(Mutex::new(0_u64));
    let progress_app = app.clone();
    let finished_app = app.clone();
    let result = update
        .download_and_install(
            move |chunk, total| {
                let mut so_far = downloaded
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                *so_far += chunk as u64;
                let _ = progress_app.emit(
                    EVENT_PROGRESS,
                    UpdateProgress::Downloading {
                        downloaded: *so_far,
                        total,
                    },
                );
            },
            move || {
                let _ = finished_app.emit(EVENT_PROGRESS, UpdateProgress::Installing);
            },
        )
        .await;

    match result {
        // Windows exits inside the installer; elsewhere relaunch here.
        Ok(()) => app.restart(),
        Err(error) => {
            state.installing.store(false, Ordering::SeqCst);
            diagnostics::warn(&format!("update install failed: {error}"));
            let message = "The update could not be installed. Try again later.".to_owned();
            let _ = app.emit(
                EVENT_PROGRESS,
                UpdateProgress::Failed {
                    message: message.clone(),
                },
            );
            Err(message)
        }
    }
}
