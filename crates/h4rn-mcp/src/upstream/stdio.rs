//! A child process speaking MCP over stdin/stdout.
//!
//! MCP's stdio transport is newline-delimited JSON-RPC: one complete message
//! per line, no framing header. The child gets the resolved environment and
//! working directory, its stderr is drained (a full stderr pipe would deadlock
//! a chatty server), and every outstanding request fails when the child exits
//! so a caller never waits for a response that can no longer arrive.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, broadcast, oneshot};
use tokio::task::JoinHandle;

use super::{
    ConnectedUpstream, Upstream, UpstreamError, UpstreamEvent, UpstreamLimits, discover,
    initialize_params,
};
use crate::jsonrpc::{self, Incoming};
use crate::types::ServerCapabilities;

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, UpstreamError>>>>>;

/// Spawns a stdio server, performs the MCP handshake and discovers its tools.
pub async fn connect(
    name: &str,
    command: &str,
    args: &[String],
    cwd: Option<&Path>,
    env: &BTreeMap<String, String>,
    limits: &UpstreamLimits,
    events: broadcast::Sender<UpstreamEvent>,
) -> Result<ConnectedUpstream, UpstreamError> {
    let mut process = Command::new(command);
    process
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = cwd {
        process.current_dir(cwd);
    }
    for (key, value) in env {
        process.env(key, value);
    }
    let mut child = process.spawn().map_err(|error| {
        UpstreamError::Transport(format!("could not start `{command}`: {error}"))
    })?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| UpstreamError::Transport("the child process has no stdin pipe".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| UpstreamError::Transport("the child process has no stdout pipe".into()))?;
    let stderr = child.stderr.take();

    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let stdin = Arc::new(Mutex::new(stdin));
    let reader = tokio::spawn(read_stdout(
        stdout,
        Arc::clone(&pending),
        Arc::clone(&stdin),
        name.to_owned(),
        events,
    ));
    if let Some(stderr) = stderr {
        tokio::spawn(read_stderr(stderr, name.to_owned()));
    }

    let upstream = Arc::new(StdioUpstream {
        name: name.to_owned(),
        child: Mutex::new(Some(child)),
        stdin,
        pending,
        next_id: AtomicI64::new(1),
        closed: AtomicBool::new(false),
        reader: Mutex::new(Some(reader)),
        server_info: std::sync::OnceLock::new(),
        capabilities: std::sync::OnceLock::new(),
        instructions: std::sync::OnceLock::new(),
    });

    let initialize = upstream
        .request("initialize", initialize_params(), limits.request_timeout)
        .await?;
    let capabilities =
        ServerCapabilities::from_json(initialize.get("capabilities").unwrap_or(&Value::Null));
    let _ = upstream.capabilities.set(capabilities);
    let _ = upstream.server_info.set(
        initialize
            .get("serverInfo")
            .and_then(|info| info.get("name"))
            .and_then(Value::as_str)
            .map(str::to_owned),
    );
    let _ = upstream.instructions.set(
        initialize
            .get("instructions")
            .and_then(Value::as_str)
            .map(str::to_owned),
    );
    upstream
        .notify("notifications/initialized", Value::Null)
        .await?;

    discover(upstream, limits).await
}

/// A live child process.
struct StdioUpstream {
    name: String,
    child: Mutex<Option<Child>>,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Pending,
    next_id: AtomicI64,
    closed: AtomicBool,
    reader: Mutex<Option<JoinHandle<()>>>,
    server_info: std::sync::OnceLock<Option<String>>,
    capabilities: std::sync::OnceLock<ServerCapabilities>,
    instructions: std::sync::OnceLock<Option<String>>,
}

fn empty_capabilities() -> &'static ServerCapabilities {
    static EMPTY: std::sync::OnceLock<ServerCapabilities> = std::sync::OnceLock::new();
    EMPTY.get_or_init(ServerCapabilities::default)
}

impl StdioUpstream {
    async fn write_value(&self, value: &Value) -> Result<(), UpstreamError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(UpstreamError::Transport(format!(
                "\"{}\" is not running",
                self.name
            )));
        }
        let mut line = serde_json::to_string(value)
            .map_err(|error| UpstreamError::Protocol(error.to_string()))?;
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await.map_err(|error| {
            UpstreamError::Transport(format!("could not write to the child: {error}"))
        })?;
        stdin.flush().await.map_err(|error| {
            UpstreamError::Transport(format!("could not flush the child's stdin: {error}"))
        })
    }
}

#[async_trait]
impl Upstream for StdioUpstream {
    fn server_info(&self) -> Option<&str> {
        self.server_info.get().and_then(|info| info.as_deref())
    }

    fn capabilities(&self) -> &ServerCapabilities {
        self.capabilities
            .get()
            .unwrap_or_else(|| empty_capabilities())
    }

    fn instructions(&self) -> Option<&str> {
        self.instructions.get().and_then(|text| text.as_deref())
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, UpstreamError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id, sender);
        if let Err(error) = self
            .write_value(&jsonrpc::request(id, method, params))
            .await
        {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }
        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(UpstreamError::Transport(format!(
                "\"{}\" closed before answering",
                self.name
            ))),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                let _ = self
                    .write_value(&jsonrpc::notification(
                        "notifications/cancelled",
                        json!({"requestId": id, "reason": "timeout"}),
                    ))
                    .await;
                Err(UpstreamError::Timeout)
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), UpstreamError> {
        self.write_value(&jsonrpc::notification(method, params))
            .await
    }

    async fn shutdown(&self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(reader) = self.reader.lock().await.take() {
            reader.abort();
        }
        if let Some(mut child) = self.child.lock().await.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        fail_pending(&self.pending, "the server was stopped").await;
    }
}

async fn fail_pending(pending: &Pending, reason: &str) {
    let mut pending = pending.lock().await;
    for (_, sender) in pending.drain() {
        let _ = sender.send(Err(UpstreamError::Transport(reason.to_owned())));
    }
}

async fn read_stdout(
    stdout: tokio::process::ChildStdout,
    pending: Pending,
    stdin: Arc<Mutex<ChildStdin>>,
    name: String,
    events: broadcast::Sender<UpstreamEvent>,
) {
    let mut lines = BufReader::new(stdout).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    tracing::debug!(server = %name, "ignoring a non-JSON line from the child");
                    continue;
                };
                match jsonrpc::parse_message(value) {
                    Ok(Incoming::Response { id, result, error }) => {
                        let Some(numeric) = jsonrpc::numeric_id(&id) else {
                            continue;
                        };
                        let Some(sender) = pending.lock().await.remove(&numeric) else {
                            continue;
                        };
                        let outcome = match (result, error) {
                            (_, Some(error)) => Err(UpstreamError::Rpc(error)),
                            (Some(result), None) => Ok(result),
                            (None, None) => Err(UpstreamError::Protocol(
                                "a response carried neither result nor error".into(),
                            )),
                        };
                        let _ = sender.send(outcome);
                    }
                    Ok(Incoming::Notification { method, .. }) => {
                        emit_list_changed(&events, &name, &method);
                    }
                    Ok(Incoming::Request { id, .. }) => {
                        // The hub does not advertise sampling/roots, so answer
                        // with a clear method-not-found instead of hanging.
                        let error = jsonrpc::RpcErrorObject::new(
                            jsonrpc::METHOD_NOT_FOUND,
                            "this client does not serve server-initiated requests",
                        );
                        let mut line = error.response(id).to_string();
                        line.push('\n');
                        let _ = stdin.lock().await.write_all(line.as_bytes()).await;
                    }
                    Err(error) => {
                        tracing::debug!(server = %name, %error, "ignoring malformed child message");
                    }
                }
            }
            Ok(None) => break,
            Err(error) => {
                tracing::debug!(server = %name, %error, "child stdout read failed");
                break;
            }
        }
    }
    fail_pending(&pending, &format!("\"{name}\" exited")).await;
}

fn emit_list_changed(events: &broadcast::Sender<UpstreamEvent>, name: &str, method: &str) {
    let event = match method {
        "notifications/tools/list_changed" => Some(UpstreamEvent::ToolsChanged {
            server: name.to_owned(),
        }),
        "notifications/resources/list_changed" => Some(UpstreamEvent::ResourcesChanged {
            server: name.to_owned(),
        }),
        "notifications/prompts/list_changed" => Some(UpstreamEvent::PromptsChanged {
            server: name.to_owned(),
        }),
        _ => None,
    };
    if let Some(event) = event {
        let _ = events.send(event);
    }
}

async fn read_stderr(stderr: tokio::process::ChildStderr, name: String) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::debug!(server = %name, "mcp stderr: {line}");
    }
}
