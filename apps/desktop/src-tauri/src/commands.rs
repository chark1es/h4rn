//! The desktop shell's entire IPC surface (ADR-001).
//!
//! Tauri commands are reserved for what only the shell can do: discovering and
//! starting daemon hosts, opening a link in the OS browser, and reading or
//! writing OS-keychain secrets (ADR-016). Product behaviour stays behind the
//! WebSocket protocol, so a remote daemon and a local one use the same code.

use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

/// Opens an OIDC authorize URL and returns the loopback callback's query.
///
/// The sign-in provider redirects to `http://127.0.0.1:<port><path>?...`; this
/// command binds that one-shot listener, opens the system browser, and resolves
/// with the raw query string (without the leading `?`). The view validates the
/// OIDC `state`, so the shell never inspects or stores tokens. The listener is
/// loopback-only and accepts a single request.
#[tauri::command]
pub async fn auth_loopback(
    app: AppHandle,
    url: String,
    path: String,
    port: u16,
) -> Result<String, String> {
    let parsed = tauri::Url::parse(&url).map_err(|_| "the sign-in link is not a URL".to_owned())?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("only http and https sign-in links can be opened".to_owned());
    }
    if !path.starts_with('/') || path.len() > 256 {
        return Err("the callback path must start with /".to_owned());
    }
    if port == 0 {
        return Err("the callback port is required".to_owned());
    }

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|error| format!("could not listen on 127.0.0.1:{port}: {error}"))?;

    app.opener()
        .open_url(parsed.as_str().to_owned(), None::<String>)
        .map_err(|error| format!("could not open the sign-in page: {error}"))?;

    let accept = tokio::time::timeout(std::time::Duration::from_secs(300), listener.accept())
        .await
        .map_err(|_| "the sign-in window timed out".to_owned())?
        .map_err(|error| format!("the callback listener failed: {error}"))?;
    let (mut socket, _) = accept;

    let mut buffer = vec![0_u8; 8 * 1024];
    let read = tokio::time::timeout(std::time::Duration::from_secs(10), socket.read(&mut buffer))
        .await
        .map_err(|_| "the sign-in callback did not send a request".to_owned())?
        .map_err(|error| format!("could not read the sign-in callback: {error}"))?;
    if read == 0 {
        return Err("the sign-in callback was empty".to_owned());
    }
    let request = String::from_utf8_lossy(&buffer[..read]);
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| "the sign-in callback was not HTTP".to_owned())?;
    let (request_path, query) = target.split_once('?').unwrap_or((target, ""));
    if request_path != path {
        let response = "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
        let _ = socket.write_all(response.as_bytes()).await;
        return Err("the sign-in callback used an unexpected path".to_owned());
    }

    let body = "<!doctype html><meta charset=\"utf-8\"><title>h4rn</title><body style=\"font:14px system-ui;padding:40px\">Signed in. You can close this tab and return to h4rn.</body>";
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;
    diagnostics::info("completed an OIDC loopback sign-in");
    Ok(query.to_owned())
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
