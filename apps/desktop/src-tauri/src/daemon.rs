//! Local daemon lifecycle (ADR-001, ADR-019).
//!
//! The shell owns exactly one process decision: if a local daemon is not
//! answering on the loopback port, start the bundled `h4rnd` sidecar. It reads
//! the access token from that same binary (`h4rnd token`) and never logs it.
//! WSL distributions are listed as additional hosts; a daemon inside one is
//! started on demand by `daemon_endpoints(activate: Some(distro))`.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Manager, WebviewWindow};
use tauri_plugin_shell::ShellExt;
use tauri_plugin_shell::process::{CommandChild, CommandEvent};

use crate::diagnostics;
use crate::platform;

/// The local daemon's loopback port (`h4rnd serve` default).
pub const LOCAL_PORT: u16 = 47800;

/// The loopback port requested from a daemon running inside WSL.
///
/// WSL2 forwards loopback listeners to Windows, so this is reachable as
/// `127.0.0.1:47801` without a proxy.
#[cfg(windows)]
pub const WSL_PORT: u16 = 47801;

const HEALTH_TIMEOUT: Duration = Duration::from_millis(800);
const START_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(150);

/// One daemon host the desktop shell can connect to.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonEndpoint {
    /// `local` or `wsl`.
    pub kind: EndpointKind,
    /// Human label for host pickers.
    pub host_label: String,
    /// The WebSocket URL the UI connects to.
    pub ws_url: String,
    /// The host's local access token; empty when it could not be read.
    pub token: String,
    /// Whether `/healthz` answered while this list was assembled.
    pub online: bool,
    /// The WSL distribution, when this is a WSL host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distro: Option<String>,
}

/// Where a daemon runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EndpointKind {
    /// This Windows/macOS/Linux machine, via the bundled sidecar.
    Local,
    /// A WSL2 distribution on this Windows machine.
    #[cfg(windows)]
    Wsl,
}

/// Process and token state owned by the shell.
#[derive(Default)]
pub struct DaemonState {
    sidecar: Mutex<Option<CommandChild>>,
    spawn_attempted: AtomicBool,
    local_token: Mutex<Option<String>>,
}

impl DaemonState {
    /// Stops the daemon this shell started, if any.
    ///
    /// A daemon that was already answering at startup was not started by us and
    /// is intentionally left running.
    pub fn shutdown(&self) {
        let child = lock(&self.sidecar).take();
        let Some(child) = child else {
            return;
        };
        match child.kill() {
            Ok(()) => diagnostics::info("stopped the bundled h4rnd sidecar"),
            Err(error) => diagnostics::warn(&format!(
                "could not stop the bundled h4rnd sidecar: {error}"
            )),
        }
    }
}

/// Builds the local endpoint, starting the sidecar when nothing answers.
pub async fn local_endpoint(app: &AppHandle) -> DaemonEndpoint {
    let online = ensure_local(app).await;
    let token = local_token(app).await.unwrap_or_default();
    DaemonEndpoint {
        kind: EndpointKind::Local,
        host_label: platform::hostname(),
        ws_url: ws_url(LOCAL_PORT),
        token,
        online,
        distro: None,
    }
}

/// Lists WSL hosts and, when `activate` names one, starts its daemon.
#[cfg(windows)]
pub async fn wsl_endpoints(activate: Option<&str>) -> Vec<DaemonEndpoint> {
    let mut endpoints = Vec::new();

    for distro in platform::wsl_distros() {
        let probe = distro.clone();
        let token = tauri::async_runtime::spawn_blocking(move || {
            platform::wsl_output(&probe, &["h4rnd", "token"])
        })
        .await
        .ok()
        .flatten();

        let mut online = is_healthy(WSL_PORT);
        if !online
            && activate == Some(distro.as_str())
            && platform::start_wsl_daemon(&distro, WSL_PORT)
        {
            diagnostics::info(&format!("starting h4rnd inside WSL distribution {distro}"));
            online = wait_for_health(WSL_PORT, START_TIMEOUT).await;
        }

        endpoints.push(DaemonEndpoint {
            kind: EndpointKind::Wsl,
            host_label: distro.clone(),
            ws_url: ws_url(WSL_PORT),
            token: token.unwrap_or_default(),
            online,
            distro: Some(distro),
        });
    }

    endpoints
}

/// No WSL hosts outside Windows.
#[cfg(not(windows))]
pub async fn wsl_endpoints(_activate: Option<&str>) -> Vec<DaemonEndpoint> {
    Vec::new()
}

/// Loads the UI with the chosen endpoint.
///
/// The UI is `index.html` served from the shell's origin, loaded with
/// `?daemon=<ws_url>&token=<token>`. The token travels only in the page's query
/// string (the UI's boot contract), never through logs. If the daemon could not
/// be reached or has no token yet, the UI still loads so it can show its own
/// connection state.
pub async fn bootstrap(app: AppHandle) {
    let endpoint = local_endpoint(&app).await;
    diagnostics::info(&format!(
        "local daemon {} at {}",
        if endpoint.online {
            "is online"
        } else {
            "is unreachable"
        },
        endpoint.ws_url
    ));

    let Some(window) = app.get_webview_window("main") else {
        diagnostics::warn("the main window is missing; not loading the UI");
        return;
    };

    let url = ui_url(&app, &window, &endpoint);
    if let Err(error) = window.navigate(url) {
        diagnostics::warn(&format!(
            "could not load the UI at the daemon endpoint: {error}"
        ));
    }
    if let Err(error) = window.show() {
        diagnostics::warn(&format!("could not show the main window: {error}"));
    }
    let _ = window.set_focus();
}

/// Ensures something answers on the local daemon port.
async fn ensure_local(app: &AppHandle) -> bool {
    if is_healthy(LOCAL_PORT) {
        return true;
    }

    let state = app.state::<DaemonState>();
    if !state.spawn_attempted.swap(true, Ordering::SeqCst) {
        match spawn_local(app).await {
            Ok(()) => diagnostics::info("started the bundled h4rnd sidecar"),
            Err(error) => diagnostics::warn(&format!("could not start h4rnd: {error}")),
        }
    }

    wait_for_health(LOCAL_PORT, START_TIMEOUT).await
}

/// Spawns `h4rnd serve`, preferring the bundled sidecar and falling back to a
/// daemon on `PATH` (a developer or a user who installed `h4rnd` themselves).
async fn spawn_local(app: &AppHandle) -> Result<(), String> {
    let shell = app.shell();
    let sidecar = shell
        .sidecar("h4rnd")
        .map_err(|error| format!("the bundled daemon is unavailable: {error}"))?;

    let (receiver, child) = match sidecar.args(["serve"]).set_process_group(true).spawn() {
        Ok(spawned) => spawned,
        Err(sidecar_error) => {
            diagnostics::warn(&format!(
                "the bundled sidecar did not start ({sidecar_error}); trying h4rnd on PATH"
            ));
            shell
                .command("h4rnd")
                .args(["serve"])
                .set_process_group(true)
                .spawn()
                .map_err(|path_error| {
                    format!("sidecar failed: {sidecar_error}; h4rnd on PATH failed: {path_error}")
                })?
        }
    };

    lock(&app.state::<DaemonState>().sidecar).replace(child);
    tauri::async_runtime::spawn(drain(receiver));

    Ok(())
}

/// Drains sidecar output so its pipes never fill, and forwards it to the shell
/// log. The daemon never prints access tokens.
async fn drain(mut receiver: tokio::sync::mpsc::Receiver<CommandEvent>) {
    while let Some(event) = receiver.recv().await {
        match event {
            CommandEvent::Stdout(line) => {
                diagnostics::info(&format!("h4rnd: {}", String::from_utf8_lossy(&line)));
            }
            CommandEvent::Stderr(line) => {
                diagnostics::info(&format!("h4rnd: {}", String::from_utf8_lossy(&line)));
            }
            CommandEvent::Error(error) => {
                diagnostics::warn(&format!("h4rnd output error: {error}"));
            }
            CommandEvent::Terminated(payload) => {
                diagnostics::info(&format!("bundled h4rnd exited (code {:?})", payload.code));
                return;
            }
            _ => {}
        }
    }
}

/// Reads the local access token through `h4rnd token`, caching it for the
/// lifetime of the shell.
async fn local_token(app: &AppHandle) -> Option<String> {
    let state = app.state::<DaemonState>();
    if let Some(token) = lock(&state.local_token).clone() {
        return Some(token);
    }

    let output = run_token(app).await?;
    let token = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if token.is_empty() {
        return None;
    }
    lock(&state.local_token).replace(token.clone());
    Some(token)
}

async fn run_token(app: &AppHandle) -> Option<tauri_plugin_shell::process::Output> {
    let shell = app.shell();
    if let Ok(sidecar) = shell.sidecar("h4rnd") {
        if let Ok(output) = sidecar.args(["token"]).output().await {
            if output.status.success() {
                return Some(output);
            }
        }
    }
    let output = shell.command("h4rnd").args(["token"]).output().await.ok()?;
    output.status.success().then_some(output)
}

/// Whether `/healthz` answers on a loopback port.
///
/// A raw HTTP/1.1 request keeps the shell free of an HTTP client dependency;
/// the daemon's health endpoint is a public, unauthenticated liveness check.
pub fn is_healthy(port: u16) -> bool {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) = TcpStream::connect_timeout(&address, HEALTH_TIMEOUT) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(HEALTH_TIMEOUT));
    let _ = stream.set_write_timeout(Some(HEALTH_TIMEOUT));

    let request =
        format!("GET /healthz HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }

    let mut buffer = [0_u8; 32];
    match stream.read(&mut buffer) {
        Ok(read) if read > 0 => {
            let head = String::from_utf8_lossy(&buffer[..read]);
            head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200")
        }
        _ => false,
    }
}

async fn wait_for_health(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if is_healthy(port) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Builds `index.html?daemon=<ws_url>&token=<token>` on top of the shell's own
/// origin, so the UI keeps loading from `frontendDist` and keeps its CSP.
fn ui_url(app: &AppHandle, window: &WebviewWindow, endpoint: &DaemonEndpoint) -> tauri::Url {
    let mut url = base_url(app, window);
    // Keep the root path: the UI's router treats `/` as the inbox and would
    // render a placeholder for `/index.html`. Both the Tauri asset protocol and
    // Vite serve `index.html` at `/`.
    if url.path().is_empty() {
        url.set_path("/");
    }
    url.set_query(None);
    url.set_fragment(None);
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("daemon", &endpoint.ws_url);
        if !endpoint.token.is_empty() {
            query.append_pair("token", &endpoint.token);
        }
    }
    url
}

fn base_url(app: &AppHandle, window: &WebviewWindow) -> tauri::Url {
    if let Ok(url) = window.url() {
        if url.scheme() != "about" {
            return url;
        }
    }
    if let Some(dev_url) = app.config().build.dev_url.clone() {
        return dev_url;
    }
    let origin = if cfg!(windows) || cfg!(target_os = "android") {
        "http://tauri.localhost/"
    } else {
        "tauri://localhost/"
    };
    tauri::Url::parse(origin).expect("the shell's own origin is a valid URL")
}

fn ws_url(port: u16) -> String {
    format!("ws://127.0.0.1:{port}/ws")
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_urls_point_at_the_daemon_route() {
        assert_eq!(ws_url(LOCAL_PORT), "ws://127.0.0.1:47800/ws");
    }

    #[cfg(windows)]
    #[test]
    fn wsl_websocket_url_uses_the_wsl_port() {
        assert_eq!(ws_url(WSL_PORT), "ws://127.0.0.1:47801/ws");
    }

    #[test]
    fn health_check_rejects_a_closed_port() {
        // Port 9 (discard) is not a daemon; connection refused, not a panic.
        assert!(!is_healthy(9));
    }
}
