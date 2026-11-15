//! The Antigravity `stream-json` fallback.
//!
//! ACP is the primary transport (ADR-026). When `agy acp` cannot be started or
//! its handshake fails, this module drives the CLI's turn-shaped NDJSON mode
//! instead:
//!
//! ```text
//! agy --input-format stream-json --output-format stream-json --sandbox \
//!     [--mode <mode>] [--model <m>] [--conversation <id>] -p ""
//! ```
//!
//! All flags come first and `-p ""` is last: `-p` takes an optional value and
//! otherwise swallows the next flag (a verified 1.5.7 trap). Resume is
//! `--conversation <uuid>`, never the per-profile and racy `--continue`.
//!
//! The fallback cannot answer `ask_permission`, so the adapter chooses an
//! explicit mode (plan, accept-edits or auto) instead of pretending approvals
//! work.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use h4rn_protocol::{
    AgentState, ApprovalDecision, HarnessKind, ToolCall, ToolCallStatus, TurnStatus, Usage,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::acp::{GroupSignal, StderrRing, signal_group, spawn_agent, spawn_stderr_reader};
use crate::{
    AttachmentRef, HarnessError, HarnessEvent, HarnessInstall, HarnessSession, SessionSpec,
    TurnReport, UserInput, detect_limit,
};

const EVENT_CHANNEL_CAPACITY: usize = 512;
const STDIN_CHANNEL_CAPACITY: usize = 64;
const TOUCHED_PATH_LIMIT: usize = 512;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
const KILL_GRACE: Duration = Duration::from_secs(2);

/// Environment variables a nested Antigravity must not inherit.
const SCRUBBED_ENV: &[&str] = &[
    "AGY_SESSION_ID",
    "ANTIGRAVITY_SESSION_ID",
    "GEMINI_CLI_SESSION_ID",
];

/// The `stream-json` normaliser.
///
/// The first frame is `{"event":"init","conversation_id":...,"init":{...}}`;
/// turns then arrive as `step_update` frames and finish with a `result` frame.
/// Unknown events are ignored; an unparseable line becomes a diagnostic event
/// rather than a panic (ADR-023).
#[derive(Debug)]
pub struct AgyStreamParser {
    session_id: Option<String>,
    tools: Vec<String>,
    state: AgentState,
    text_seen: bool,
    limit_seen: bool,
    error_seen: bool,
    last_turn: Option<TurnReport>,
    touched: Vec<String>,
}

impl Default for AgyStreamParser {
    fn default() -> Self {
        Self::new()
    }
}

impl AgyStreamParser {
    /// Creates a parser with no session state.
    pub fn new() -> Self {
        Self {
            session_id: None,
            tools: Vec::new(),
            state: AgentState::Idle,
            text_seen: false,
            limit_seen: false,
            error_seen: false,
            last_turn: None,
            touched: Vec::new(),
        }
    }

    /// Feeds one stdout line and returns its normalized events.
    pub fn push_line(&mut self, line: &str) -> Vec<HarnessEvent> {
        let line = line.trim();
        if line.is_empty() {
            return Vec::new();
        }
        let frame: Value = match serde_json::from_str(line) {
            Ok(frame) => frame,
            Err(error) => {
                return vec![HarnessEvent::Error {
                    message: format!(
                        "unparseable agy stream-json output ({error}): {}",
                        truncate(line, 320)
                    ),
                    retryable: false,
                }];
            }
        };

        let event = frame
            .get("event")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match event {
            "init" => self.on_init(&frame),
            "step_update" => self.on_step_update(&frame),
            "result" => self.on_result(&frame),
            "error" => self.on_error(&frame),
            _ => Vec::new(),
        }
    }

    /// The conversation id, once a frame reported one.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// The tool inventory from the init frame.
    pub fn tools(&self) -> &[String] {
        &self.tools
    }

    /// The current agent-state hint.
    pub fn agent_state(&self) -> AgentState {
        self.state
    }

    /// The most recent completed fallback turn.
    pub fn last_turn(&self) -> Option<&TurnReport> {
        self.last_turn.as_ref()
    }

    /// Paths named by tool steps seen so far, de-duplicated and bounded.
    pub fn touched_paths(&self) -> &[String] {
        &self.touched
    }

    fn on_init(&mut self, frame: &Value) -> Vec<HarnessEvent> {
        if let Some(session_id) = frame
            .get("conversation_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            self.session_id = Some(session_id.to_string());
        }
        if let Some(init) = frame.get("init") {
            if let Some(tools) = init.get("tools").and_then(Value::as_array) {
                self.tools = tools
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect();
            }
        }
        match &self.session_id {
            Some(session_id) => vec![HarnessEvent::SessionStarted {
                session_id: session_id.clone(),
            }],
            None => Vec::new(),
        }
    }

    fn on_step_update(&mut self, frame: &Value) -> Vec<HarnessEvent> {
        let Some(step) = frame.get("step_update") else {
            return Vec::new();
        };
        if let Some(session_id) = step
            .get("conversation_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            self.session_id = Some(session_id.to_string());
        }

        let state = step
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let step_type = step
            .get("step_type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let text = step.get("text_delta").and_then(Value::as_str);

        let mut events = Vec::new();
        if state.eq_ignore_ascii_case("ACTIVE") {
            self.state = AgentState::Thinking;
            if let Some(text) = text
                && !text.is_empty()
            {
                self.text_seen = true;
                events.push(HarnessEvent::TextDelta {
                    text: text.to_string(),
                });
            }
        } else if state.eq_ignore_ascii_case("DONE")
            && step_type == "agent_response"
            && let Some(text) = text
            && !text.is_empty()
        {
            self.text_seen = true;
            events.push(HarnessEvent::TextDelta {
                text: text.to_string(),
            });
        }

        // Some builds attach a partially normalized tool step. Preserve it
        // when the shape is recognizable instead of guessing at old ones.
        // `stream-json` never sends a completion update for these, so the
        // call is reported as running and its paths feed `touched_paths`.
        if let Some(tool_call) = step.get("tool_call").and_then(parse_stream_tool) {
            let id = tool_call
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let name = tool_call
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string();
            let summary = tool_call
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let input = tool_call.get("rawInput").cloned();
            for path in step_paths(&tool_call) {
                if self.touched.len() < TOUCHED_PATH_LIMIT && !self.touched.contains(&path) {
                    self.touched.push(path);
                }
            }
            let started_at = chrono::Utc::now();
            events.push(HarnessEvent::ToolCall {
                tool_call: ToolCall {
                    tool_call_id: id.to_string(),
                    name,
                    status: ToolCallStatus::Running,
                    summary,
                    input,
                    output: None,
                    started_at,
                    completed_at: None,
                    stats: None,
                },
            });
        }

        events
    }

    fn on_result(&mut self, frame: &Value) -> Vec<HarnessEvent> {
        let Some(result) = frame.get("result") else {
            return Vec::new();
        };
        if let Some(session_id) = result
            .get("conversation_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            self.session_id = Some(session_id.to_string());
        }

        let status = result
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("SUCCESS");
        let success = status.eq_ignore_ascii_case("SUCCESS");
        let response = result
            .get("response")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let duration_ms = result
            .get("duration_seconds")
            .and_then(Value::as_f64)
            .map(|seconds| (seconds * 1000.0).round() as u64)
            .unwrap_or(0);
        let num_turns = result.get("num_turns").and_then(Value::as_u64).unwrap_or(1);
        let usage = result
            .get("usage")
            .map(|usage| usage_from_stream(usage, duration_ms))
            .unwrap_or_else(|| Usage {
                duration_ms,
                ..Usage::default()
            });

        let mut events = Vec::new();
        if !success {
            let detail = if response.trim().is_empty() {
                status.to_string()
            } else {
                response.to_string()
            };
            if !self.limit_seen
                && let Some(signal) = detect_limit(&detail, chrono::Utc::now())
            {
                self.limit_seen = true;
                self.state = AgentState::RateLimited;
                events.push(HarnessEvent::LimitHit {
                    reset_at: signal.reset_at,
                    auto_resume: signal.reset_at.is_some(),
                });
            } else if !self.error_seen {
                self.error_seen = true;
                self.state = AgentState::Error;
                events.push(HarnessEvent::Error {
                    message: truncate(&detail, 2_000),
                    retryable: true,
                });
            }
        } else if !self.text_seen && !response.trim().is_empty() {
            self.text_seen = true;
            self.state = AgentState::Thinking;
            events.push(HarnessEvent::TextDelta {
                text: response.to_string(),
            });
        }

        self.last_turn = Some(TurnReport {
            num_turns,
            duration_ms,
            duration_api_ms: 0,
            cost_micros: 0,
            is_error: !success,
            stop_reason: Some(status.to_string()),
            subtype: None,
        });
        self.state = if success {
            AgentState::Done
        } else if self.limit_seen {
            AgentState::RateLimited
        } else {
            AgentState::Error
        };

        if usage.input_tokens > 0 || usage.output_tokens > 0 || usage.cached_input_tokens > 0 {
            events.push(HarnessEvent::Usage {
                usage: usage.clone(),
            });
        }
        events.push(HarnessEvent::TurnCompleted {
            status: if success {
                TurnStatus::Completed
            } else {
                TurnStatus::Failed
            },
            usage,
        });

        self.text_seen = false;
        self.limit_seen = false;
        self.error_seen = false;
        events
    }

    fn on_error(&mut self, frame: &Value) -> Vec<HarnessEvent> {
        let detail = frame
            .get("error")
            .and_then(|error| {
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .or_else(|| error.as_str())
            })
            .or_else(|| frame.get("message").and_then(Value::as_str))
            .unwrap_or("agy stream-json reported an error")
            .to_string();
        if !self.error_seen
            && let Some(signal) = detect_limit(&detail, chrono::Utc::now())
        {
            self.error_seen = true;
            self.limit_seen = true;
            self.state = AgentState::RateLimited;
            return vec![HarnessEvent::LimitHit {
                reset_at: signal.reset_at,
                auto_resume: signal.reset_at.is_some(),
            }];
        }
        if self.error_seen {
            return Vec::new();
        }
        self.error_seen = true;
        self.state = AgentState::Error;
        vec![HarnessEvent::Error {
            message: truncate(&detail, 2_000),
            retryable: true,
        }]
    }
}

fn parse_stream_tool(value: &Value) -> Option<Value> {
    if value.is_object() {
        Some(value.clone())
    } else {
        None
    }
}

fn usage_from_stream(raw: &Value, duration_ms: u64) -> Usage {
    let input = u64_field(raw, "input_tokens");
    let cache = u64_field(raw, "cache_read_tokens");
    Usage {
        input_tokens: input.saturating_add(cache),
        output_tokens: u64_field(raw, "output_tokens"),
        cached_input_tokens: cache,
        cost_micros: 0,
        duration_ms,
        ..Usage::default()
    }
}

fn u64_field(value: &Value, key: &str) -> u64 {
    value
        .get(key)
        .and_then(|field| {
            field
                .as_u64()
                .or_else(|| field.as_i64().map(|number| number.max(0) as u64))
                .or_else(|| {
                    field
                        .as_f64()
                        .filter(|number| *number >= 0.0)
                        .map(|number| number as u64)
                })
        })
        .unwrap_or(0)
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

/// State shared with the stdout parser and the supervisor.
#[derive(Debug)]
struct StreamShared {
    session_id: Option<String>,
    state: AgentState,
    touched: Vec<String>,
    last_turn: Option<TurnReport>,
}

/// A `stream-json` session.
pub(crate) struct StreamJsonSession {
    kind: HarnessKind,
    stdin_tx: Option<mpsc::Sender<Vec<u8>>>,
    events_rx: Option<mpsc::Receiver<HarnessEvent>>,
    supervisor: Option<tokio::task::JoinHandle<()>>,
    writer: Option<tokio::task::JoinHandle<()>>,
    pid: u32,
    shutting_down: Arc<AtomicBool>,
    stderr: StderrRing,
    shared: Arc<Mutex<StreamShared>>,
    first_attachments: Vec<AttachmentRef>,
    first_send: bool,
    closed: bool,
}

impl StreamJsonSession {
    fn session_id(&self) -> Option<String> {
        lock(&self.shared).session_id.clone()
    }

    fn agent_state(&self) -> AgentState {
        lock(&self.shared).state
    }

    fn last_turn(&self) -> Option<TurnReport> {
        lock(&self.shared).last_turn.clone()
    }

    fn touched_paths(&self) -> Vec<String> {
        lock(&self.shared).touched.clone()
    }

    fn stderr_tail(&self) -> String {
        self.stderr.snapshot()
    }

    async fn write_frame(&mut self, frame: Vec<u8>) -> Result<(), HarnessError> {
        let Some(sender) = &self.stdin_tx else {
            return Err(HarnessError::Transport {
                harness: self.kind,
                message: "agy stream-json stdin is closed".into(),
            });
        };
        sender
            .send(frame)
            .await
            .map_err(|_| HarnessError::Transport {
                harness: self.kind,
                message: "agy stream-json writer task has exited".into(),
            })
    }
}

#[async_trait]
impl HarnessSession for StreamJsonSession {
    async fn send(&mut self, input: UserInput) -> Result<(), HarnessError> {
        if self.closed || self.shutting_down.load(Ordering::SeqCst) {
            return Err(HarnessError::Transport {
                harness: self.kind,
                message: "session is shut down".into(),
            });
        }
        let mut attachments = input.attachments;
        if self.first_send {
            self.first_send = false;
            attachments.extend(self.first_attachments.iter().cloned());
        }
        let frame = build_stream_frame(&input.text, &attachments)?;
        self.write_frame(frame).await?;
        lock(&self.shared).state = AgentState::Thinking;
        Ok(())
    }

    async fn interrupt(&mut self) -> Result<(), HarnessError> {
        if self.closed {
            return Err(HarnessError::Transport {
                harness: self.kind,
                message: "session is shut down".into(),
            });
        }
        // Print mode has no control channel; SIGINT ends the current turn the
        // same way the TUI's interrupt does.
        signal_group(self.pid, GroupSignal::Interrupt);
        lock(&self.shared).state = AgentState::Thinking;
        Ok(())
    }

    async fn respond_approval(
        &mut self,
        _approval_id: &str,
        _decision: ApprovalDecision,
        _option_id: Option<&str>,
    ) -> Result<(), HarnessError> {
        Err(HarnessError::Unsupported {
            harness: self.kind,
            feature: "answering approvals over stream-json; start the ACP transport for client-decided permissions".into(),
        })
    }

    fn events(&mut self) -> BoxStream<'static, HarnessEvent> {
        match self.events_rx.take() {
            Some(receiver) => ReceiverStream::new(receiver).boxed(),
            None => futures::stream::empty().boxed(),
        }
    }

    fn session_id(&self) -> Option<String> {
        StreamJsonSession::session_id(self)
    }

    fn agent_state(&self) -> AgentState {
        StreamJsonSession::agent_state(self)
    }

    fn last_turn(&self) -> Option<TurnReport> {
        StreamJsonSession::last_turn(self)
    }

    fn touched_paths(&self) -> Vec<String> {
        StreamJsonSession::touched_paths(self)
    }

    fn stderr_tail(&self) -> String {
        StreamJsonSession::stderr_tail(self)
    }

    async fn shutdown(&mut self) -> Result<(), HarnessError> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.shutting_down.store(true, Ordering::SeqCst);
        self.stdin_tx.take();
        signal_group(self.pid, GroupSignal::Terminate);
        if let Some(mut supervisor) = self.supervisor.take() {
            match tokio::time::timeout(SHUTDOWN_GRACE, &mut supervisor).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(target: "h4rn_harness::antigravity", %error, "agy supervisor task failed");
                }
                Err(_) => {
                    signal_group(self.pid, GroupSignal::Kill);
                    match tokio::time::timeout(KILL_GRACE, &mut supervisor).await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            tracing::warn!(target: "h4rn_harness::antigravity", %error, "agy supervisor task failed after kill");
                        }
                        Err(_) => supervisor.abort(),
                    }
                }
            }
        }
        if let Some(writer) = self.writer.take() {
            writer.abort();
        }
        Ok(())
    }
}

impl Drop for StreamJsonSession {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        self.shutting_down.store(true, Ordering::SeqCst);
        signal_group(self.pid, GroupSignal::Kill);
    }
}

/// Spawns one `agy` `stream-json` process.
pub(crate) fn spawn(
    install: &HarnessInstall,
    spec: &SessionSpec,
    mode: &str,
) -> Result<StreamJsonSession, HarnessError> {
    if !spec.workspace.is_dir() {
        return Err(HarnessError::Spawn {
            harness: HarnessKind::Antigravity,
            message: format!(
                "workspace {} is not a directory on this host",
                spec.workspace.display()
            ),
        });
    }

    let mut args: Vec<String> = vec![
        "--input-format".into(),
        "stream-json".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--sandbox".into(),
    ];
    args.push("--mode".into());
    args.push(mode.to_string());
    if !spec.model.trim().is_empty()
        && spec.model != "default"
        && spec.model != "antigravity-default"
    {
        args.push("--model".into());
        args.push(spec.model.trim().to_string());
    }
    if let Some(resume_id) = spec.resume_id.as_deref().filter(|id| !id.is_empty()) {
        args.push("--conversation".into());
        args.push(resume_id.to_string());
    }
    // Must be last: `-p` takes an optional value and otherwise swallows the
    // next token, which is a verified 1.5.7 flag-parsing trap.
    args.push("-p".into());
    args.push(String::new());

    let mut env = BTreeMap::new();
    for (key, value) in &spec.env_overrides {
        if !crate::antigravity::is_h4rn_only_env(key) {
            env.insert(key.clone(), value.clone());
        }
    }

    let mut child = spawn_agent(&install.path, &args, &spec.workspace, &env, SCRUBBED_ENV)
        .map_err(|error| HarnessError::Spawn {
            harness: HarnessKind::Antigravity,
            message: format!("failed to spawn {}: {error}", install.path.display()),
        })?;
    let pid = child.id().ok_or_else(|| HarnessError::Spawn {
        harness: HarnessKind::Antigravity,
        message: "agy exited before it reported a process id".into(),
    })?;
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        signal_group(pid, GroupSignal::Kill);
        return Err(HarnessError::Spawn {
            harness: HarnessKind::Antigravity,
            message: "agy was spawned without piped stdio".into(),
        });
    };

    let (events_tx, events_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    let (stdin_tx, stdin_rx) = mpsc::channel(STDIN_CHANNEL_CAPACITY);
    let stderr_ring = StderrRing::new();
    let shutting_down = Arc::new(AtomicBool::new(false));
    let shared = Arc::new(Mutex::new(StreamShared {
        session_id: spec.resume_id.clone(),
        state: AgentState::Idle,
        touched: Vec::new(),
        last_turn: None,
    }));

    spawn_stderr_reader(stderr, stderr_ring.clone());
    spawn_stdout_reader(
        stdout,
        shared.clone(),
        events_tx.clone(),
        shutting_down.clone(),
    );
    let writer = spawn_stdin_writer(stdin, stdin_rx, events_tx.clone());
    let supervisor = spawn_supervisor(child, stderr_ring.clone(), shutting_down.clone(), events_tx);

    Ok(StreamJsonSession {
        kind: HarnessKind::Antigravity,
        stdin_tx: Some(stdin_tx),
        events_rx: Some(events_rx),
        supervisor: Some(supervisor),
        writer: Some(writer),
        pid,
        shutting_down,
        stderr: stderr_ring,
        shared,
        first_attachments: spec.attachments.clone(),
        first_send: true,
        closed: false,
    })
}

fn spawn_stdout_reader(
    stdout: tokio::process::ChildStdout,
    shared: Arc<Mutex<StreamShared>>,
    events_tx: mpsc::Sender<HarnessEvent>,
    shutting_down: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut parser = AgyStreamParser::new();
        let mut lines = BufReader::new(stdout).lines();
        loop {
            let line = match lines.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(error) => {
                    let _ = events_tx
                        .send(HarnessEvent::Error {
                            message: format!("failed reading agy stdout: {error}"),
                            retryable: false,
                        })
                        .await;
                    break;
                }
            };
            let events = parser.push_line(&line);
            {
                let mut state = lock(&shared);
                state.session_id = parser.session_id().map(str::to_string);
                state.state = parser.agent_state();
                state.last_turn = parser.last_turn().cloned();
                state.touched = parser.touched_paths().to_vec();
            }
            for event in events {
                if events_tx.send(event).await.is_err() {
                    return;
                }
            }
        }
        if !shutting_down.load(Ordering::SeqCst) {
            let _ = events_tx
                .send(HarnessEvent::Error {
                    message: "agy stream-json stdout closed".into(),
                    retryable: false,
                })
                .await;
        }
    })
}

fn spawn_stdin_writer(
    mut stdin: tokio::process::ChildStdin,
    mut frames: mpsc::Receiver<Vec<u8>>,
    events_tx: mpsc::Sender<HarnessEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(frame) = frames.recv().await {
            if let Err(error) = stdin.write_all(&frame).await {
                let _ = events_tx
                    .send(HarnessEvent::Error {
                        message: format!("failed writing to agy stdin: {error}"),
                        retryable: false,
                    })
                    .await;
                break;
            }
            if let Err(error) = stdin.flush().await {
                let _ = events_tx
                    .send(HarnessEvent::Error {
                        message: format!("failed flushing agy stdin: {error}"),
                        retryable: false,
                    })
                    .await;
                break;
            }
        }
    })
}

fn spawn_supervisor(
    mut child: tokio::process::Child,
    stderr: StderrRing,
    shutting_down: Arc<AtomicBool>,
    events_tx: mpsc::Sender<HarnessEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let status = child.wait().await;
        if !shutting_down.load(Ordering::SeqCst) {
            let event = match &status {
                Ok(status) if !status.success() => {
                    let tail = stderr.snapshot();
                    let message = if tail.is_empty() {
                        format!("agy exited with {status}")
                    } else {
                        format!("agy exited with {status}: {tail}")
                    };
                    match detect_limit(&message, chrono::Utc::now()) {
                        Some(signal) => Some(HarnessEvent::LimitHit {
                            reset_at: signal.reset_at,
                            auto_resume: signal.reset_at.is_some(),
                        }),
                        None => Some(HarnessEvent::Error {
                            message,
                            retryable: false,
                        }),
                    }
                }
                Err(error) => Some(HarnessEvent::Error {
                    message: format!("failed to wait for agy: {error}"),
                    retryable: false,
                }),
                _ => None,
            };
            if let Some(event) = event {
                let _ = events_tx.send(event).await;
            }
        }
    })
}

/// Builds one stream-json user frame.
fn build_stream_frame(text: &str, attachments: &[AttachmentRef]) -> Result<Vec<u8>, HarnessError> {
    let mut path_lines = Vec::new();

    for attachment in attachments {
        let name = attachment
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| attachment.id.to_string());
        // The turn-shaped mode has no verified image-block contract, so name
        // the file instead of guessing at a base64 envelope. The agent can read
        // it through its own tools.
        let kind = if resolved_media_type(attachment).starts_with("image/") {
            "image"
        } else {
            "file"
        };
        path_lines.push(format!(
            "[Attached {kind} \"{name}\" is saved at: {}]",
            attachment.path.display()
        ));
    }

    let mut full_text = text.to_string();
    if !path_lines.is_empty() {
        if !full_text.is_empty() {
            full_text.push('\n');
        }
        full_text.push_str(&path_lines.join("\n"));
    }
    let content = if full_text.is_empty() {
        Vec::new()
    } else {
        vec![json!({"type": "text", "text": full_text})]
    };

    let frame = json!({
        "event": "user",
        "message": {"role": "user", "content": content}
    });
    let mut bytes = serde_json::to_vec(&frame).map_err(|error| HarnessError::Protocol {
        harness: HarnessKind::Antigravity,
        message: format!("failed to encode a stream-json frame: {error}"),
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn resolved_media_type(attachment: &AttachmentRef) -> String {
    if !attachment.media_type.is_empty() {
        return attachment.media_type.clone();
    }
    let extension = attachment
        .path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("pdf") => "application/pdf",
        _ => "",
    }
    .to_string()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Tool paths named by a stream-json step, when the shape exposes them.
fn step_paths(value: &Value) -> Vec<String> {
    let mut paths = BTreeSet::new();
    for key in ["path", "file_path", "absolute_path"] {
        if let Some(path) = value
            .get("rawInput")
            .and_then(|input| input.get(key))
            .and_then(Value::as_str)
        {
            paths.insert(path.to_string());
        }
    }
    paths.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replay(text: &str) -> (AgyStreamParser, Vec<HarnessEvent>) {
        let mut parser = AgyStreamParser::new();
        let mut events = Vec::new();
        for line in text.lines() {
            events.extend(parser.push_line(line));
        }
        (parser, events)
    }

    #[test]
    fn init_step_and_result_are_normalized() {
        let (parser, events) = replay(
            r#"{"event":"init","conversation_id":"conv-1","init":{"cwd":"/w","tools":["view_file"],"permission_mode":"request-review"}}
{"event":"step_update","step_update":{"conversation_id":"conv-1","step_index":1,"state":"ACTIVE","step_type":"agent_response","text_delta":"pong"}}
{"event":"result","result":{"conversation_id":"conv-1","status":"SUCCESS","response":"pong\n","duration_seconds":1.5,"num_turns":1,"usage":{"input_tokens":10,"output_tokens":2,"cache_read_tokens":3}}}"#,
        );
        assert!(matches!(
            events.first(),
            Some(HarnessEvent::SessionStarted { session_id }) if session_id == "conv-1"
        ));
        assert_eq!(
            events
                .iter()
                .filter_map(|event| match event {
                    HarnessEvent::TextDelta { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec!["pong"]
        );
        let usage = events
            .iter()
            .find_map(|event| match event {
                HarnessEvent::Usage { usage } => Some(usage.clone()),
                _ => None,
            })
            .expect("usage");
        assert_eq!(usage.input_tokens, 13);
        assert_eq!(usage.output_tokens, 2);
        assert_eq!(usage.cached_input_tokens, 3);
        assert!(matches!(
            events.last(),
            Some(HarnessEvent::TurnCompleted {
                status: TurnStatus::Completed,
                ..
            })
        ));
        assert_eq!(parser.session_id(), Some("conv-1"));
        assert_eq!(parser.agent_state(), AgentState::Done);
        assert_eq!(parser.last_turn().map(|turn| turn.num_turns), Some(1));
    }

    #[test]
    fn a_failed_result_with_a_limit_is_not_an_error() {
        let (_, events) = replay(
            r#"{"event":"result","result":{"conversation_id":"conv-1","status":"ERROR","response":"Quota exceeded. Resets at 2026-11-13T15:00:00Z","num_turns":1}}"#,
        );
        assert!(matches!(
            events.first(),
            Some(HarnessEvent::LimitHit { .. })
        ));
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, HarnessEvent::Error { .. }))
        );
        assert!(matches!(
            events.last(),
            Some(HarnessEvent::TurnCompleted {
                status: TurnStatus::Failed,
                ..
            })
        ));
    }

    #[test]
    fn user_frames_use_the_event_envelope() {
        let frame = build_stream_frame("hello", &[]).expect("frame");
        let value: Value = serde_json::from_slice(&frame).expect("json");
        assert_eq!(value["event"], "user");
        assert_eq!(value["message"]["role"], "user");
        assert_eq!(value["message"]["content"][0]["type"], "text");
        assert_eq!(value["message"]["content"][0]["text"], "hello");
    }

    #[test]
    fn step_paths_reads_raw_input() {
        let paths = step_paths(&json!({"rawInput": {"path": "/w/a.rs"}}));
        assert_eq!(paths, vec!["/w/a.rs".to_string()]);
    }
}
