//! The desktop shell's entire IPC surface (ADR-001).
//!
//! Tauri commands are reserved for what only the shell can do: discovering and
//! starting daemon hosts, opening a link in the OS browser, and reading or
//! writing OS-keychain secrets (ADR-016). Product behaviour stays behind the
//! WebSocket protocol, so a remote daemon and a local one use the same code.

use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;

use crate::daemon::{self, DaemonEndpoint};
use crate::diagnostics;

/// Maximum length of a keychain service or account name.
const MAX_KEYCHAIN_FIELD: usize = 256;

/// Maximum size of a keychain secret, in bytes.
const MAX_KEYCHAIN_SECRET: usize = 64 * 1024;

/// Lists the daemon hosts this shell can reach.
///
/// The local host is always first and its sidecar is started if nothing answers
/// on `127.0.0.1:47800`. On Windows, each WSL distribution is listed after it.
/// Passing `activate` starts the daemon in that distribution on `127.0.0.1:47801`
/// (WSL2 localhost forwarding) before the list is returned.
#[tauri::command]
pub async fn daemon_endpoints(
    app: AppHandle,
    activate: Option<String>,
) -> Result<Vec<DaemonEndpoint>, String> {
    let mut endpoints = vec![daemon::local_endpoint(&app).await];
    endpoints.extend(daemon::wsl_endpoints(activate.as_deref()).await);
    diagnostics::info(&format!("reported {} daemon endpoint(s)", endpoints.len()));
    Ok(endpoints)
}

/// Opens an http(s) or mailto link in the user's default application.
///
/// The allowlist is deliberate: the view must not be able to launch arbitrary
/// schemes (`file:`, `h4rn:`) through the shell.
#[tauri::command]
pub fn open_external(app: AppHandle, url: String) -> Result<(), String> {
    let parsed = tauri::Url::parse(&url).map_err(|_| "the link is not a valid URL".to_owned())?;
    if !matches!(parsed.scheme(), "http" | "https" | "mailto") {
        return Err("only http, https and mailto links can be opened from h4rn".to_owned());
    }
    app.opener()
        .open_url(parsed.as_str().to_owned(), None::<String>)
        .map_err(|error| format!("could not open the link: {error}"))
}

/// Reads a secret from the OS keychain.
///
/// A missing entry is `Ok(None)`, not an error: callers use it to decide
/// whether onboarding is needed.
#[tauri::command]
pub async fn keychain_get(service: String, account: String) -> Result<Option<String>, String> {
    check_field("service", &service)?;
    check_field("account", &account)?;

    blocking(move || match keyring::Entry::new(&service, &account) {
        Ok(entry) => match entry.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(format!(
                "the OS keychain could not read the secret: {error}"
            )),
        },
        Err(error) => Err(format!("the OS keychain is not available: {error}")),
    })
    .await
}

/// Stores a secret in the OS keychain.
#[tauri::command]
pub async fn keychain_set(service: String, account: String, secret: String) -> Result<(), String> {
    check_field("service", &service)?;
    check_field("account", &account)?;
    if secret.len() > MAX_KEYCHAIN_SECRET {
        return Err("the secret is too large for the OS keychain".to_owned());
    }

    blocking(move || {
        keyring::Entry::new(&service, &account)
            .and_then(|entry| entry.set_password(&secret))
            .map_err(|error| format!("the OS keychain could not store the secret: {error}"))
    })
    .await
}

/// Removes a secret from the OS keychain. Removing a missing entry is fine.
#[tauri::command]
pub async fn keychain_delete(service: String, account: String) -> Result<(), String> {
    check_field("service", &service)?;
    check_field("account", &account)?;

    blocking(move || match keyring::Entry::new(&service, &account) {
        Ok(entry) => match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(format!(
                "the OS keychain could not delete the secret: {error}"
            )),
        },
        Err(error) => Err(format!("the OS keychain is not available: {error}")),
    })
    .await
}

fn check_field(name: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("the keychain {name} is required"));
    }
    if value.len() > MAX_KEYCHAIN_FIELD {
        return Err(format!("the keychain {name} is too long"));
    }
    Ok(())
}

async fn blocking<T, F>(task: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(task)
        .await
        .map_err(|error| format!("the keychain operation was interrupted: {error}"))?
}
