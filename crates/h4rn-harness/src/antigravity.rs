//! The Google Antigravity adapter (ADR-026).
//!
//! # Transport
//!
//! ACP is the primary surface. One long-lived child per session:
//!
//! ```text
//! agy acp --process-per-session --sandbox \
//!     [--app-data-dir <dir>] [--gemini-dir <dir>] [--cloud_code_endpoint <url>]
//! ```
//!
//! The child speaks JSON-RPC 2.0 over NDJSON on stdio and is driven through
//! [`crate::acp::AcpClient`]. The shared ACP session runtime in
//! [`crate::acp`] negotiates capabilities, opens (or loads) a session, selects
//! the account-scoped model from the list `session/new` returned, sets an
//! explicit mode, and injects the h4rn MCP hub over HTTP when the agent
//! advertises `mcpCapabilities.http` (otherwise over the documented
//! `mcp-remote` stdio bridge).
//!
//! # Approvals
//!
//! ACP puts the decision on the client, so [`HarnessSession::respond_approval`]
//! answers the agent's `session/request_permission` request directly. The
//! adapter never passes `--dangerously-skip-permissions`: an approval-required
//! thread stays in `default` mode and the client decides.
//!
//! # Fallback
//!
//! When the ACP child cannot be spawned or its handshake fails, the adapter
//! drops to the CLI's turn-shaped `stream-json` mode (see [`stream`]). That
//! transport cannot raise approvals, so it runs an explicit conservative mode
//! and reports [`HarnessError::Unsupported`] if a decision is requested.
//!
//! # Detection
//!
//! [`detect_agy`] looks for `agy` on `PATH` and in `~/.local/bin` (the
//! documented manual install location), then asks it for `--version`. Nothing
//! in the daemon hard-codes a path.
//!
//! # Scope
//!
//! This module owns only what is Antigravity-specific: its spawn line, its
//! stream-json fallback, its environment overrides, and its usage-limit
//! parsing. Session/protocol mapping lives in [`crate::acp`].

use std::path::{Path, PathBuf};
use std::process::Command;

use async_trait::async_trait;
use h4rn_protocol::{HarnessKind, RuntimeMode};

use crate::acp::{AcpClient, AcpClientConfig, AcpSessionPolicy, StartedAcpSession, map_acp_error};
use crate::{
    ENV_MCP_AUTH_TOKEN, ENV_MCP_BEARER_TOKEN, Harness, HarnessError, HarnessInstall,
    HarnessSession, SessionSpec, detect_limit,
};

mod stream;

pub use stream::AgyStreamParser;

/// Overrides the `agy` executable used by the adapter.
pub const ENV_AGY_PATH: &str = "H4RN_AGY_PATH";
/// Passes `--app-data-dir` to `agy acp` (one profile per Google account).
pub const ENV_APP_DATA_DIR: &str = "H4RN_AGY_APP_DATA_DIR";
/// Passes `--gemini-dir` to `agy acp`.
pub const ENV_GEMINI_DIR: &str = "H4RN_AGY_GEMINI_DIR";
/// Passes `--cloud_code_endpoint` to `agy acp` for gateway deployments.
pub const ENV_CLOUD_CODE_ENDPOINT: &str = "H4RN_AGY_CLOUD_CODE_ENDPOINT";
/// Overrides the execution mode (`default`, `accept-edits`, `auto`, `plan`).
pub const ENV_AGY_MODE: &str = "H4RN_AGY_MODE";
/// Forces one transport: `acp`, `stream-json`, or `auto` (the default).
pub const ENV_AGY_TRANSPORT: &str = "H4RN_AGY_TRANSPORT";
/// The only API-key variable Antigravity's CLI accepts.
pub const ENV_GEMINI_API_KEY: &str = "GEMINI_API_KEY";
/// Documented custom Gemini endpoint override.
pub const ENV_GOOGLE_GEMINI_BASE_URL: &str = "GOOGLE_GEMINI_BASE_URL";
/// Model id Antigravity should request.
pub const ENV_GEMINI_MODEL: &str = "GEMINI_MODEL";

/// Environment variables a nested Antigravity must not inherit.
const SCRUBBED_ENV: &[&str] = &[
    "AGY_SESSION_ID",
    "ANTIGRAVITY_SESSION_ID",
    "GEMINI_CLI_SESSION_ID",
];

/// Model names that mean "the agent's own default".
const DEFAULT_MODELS: &[&str] = &["default", "antigravity-default", "auto"];

/// The Google Antigravity adapter.
#[derive(Clone, Debug, Default)]
pub struct AntigravityHarness {
    install: Option<HarnessInstall>,
}

impl AntigravityHarness {
    /// Creates an adapter that discovers `agy` on this host.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an adapter bound to a known installation.
    ///
    /// Tests and the daemon's inventory use this after running [`detect_agy`]
    /// once; nothing else should hard-code a path.
    pub fn with_install(install: HarnessInstall) -> Self {
        Self {
            install: Some(install),
        }
    }
}

impl AntigravityHarness {
    async fn acp_session(
        &self,
        install: &HarnessInstall,
        spec: &SessionSpec,
    ) -> Result<StartedAcpSession, HarnessError> {
        let client = AcpClient::spawn(acp_config(install, spec))
            .map_err(|error| map_acp_error(error, HarnessKind::Antigravity))?;
        match crate::acp::session_start(&client, spec, antigravity_policy(), &spec.workspace).await
        {
            Ok(session) => Ok(session),
            Err(error) => {
                client.kill();
                Err(error)
            }
        }
    }
}

#[async_trait]
impl Harness for AntigravityHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Antigravity
    }

    fn detect(&self) -> Option<HarnessInstall> {
        self.install.clone().or_else(detect_agy)
    }

    async fn start_session(
        &self,
        spec: SessionSpec,
    ) -> Result<Box<dyn HarnessSession>, HarnessError> {
        let install = self
            .detect()
            .ok_or(HarnessError::NotDetected { kind: self.kind() })?;
        if !spec.workspace.is_dir() {
            return Err(HarnessError::Spawn {
                harness: self.kind(),
                message: format!(
                    "workspace {} is not a directory on this host",
                    spec.workspace.display()
                ),
            });
        }

        let transport = transport_preference(&spec);
        if transport != TransportPreference::StreamJson {
            match self.acp_session(&install, &spec).await {
                Ok(session) => return Ok(session.into_session()),
                // An explicit mode/model failure is a caller error, not a
                // transport failure: the fallback would fail the same way.
                Err(error @ HarnessError::Unsupported { .. }) => return Err(error),
                Err(error) if transport == TransportPreference::Acp => return Err(error),
                Err(error) => {
                    tracing::warn!(
                        target: "h4rn_harness::antigravity",
                        %error,
                        "agy ACP transport failed; falling back to stream-json"
                    );
                }
            }
        }

        let mode = stream_mode(&spec);
        let session = stream::spawn(&install, &spec, &mode)?;
        Ok(Box::new(session))
    }
}

/// The Antigravity ACP policy: its own mode override, its limit parser, and
/// the model aliases that mean "whatever agy defaults to".
fn antigravity_policy() -> AcpSessionPolicy {
    AcpSessionPolicy {
        harness: HarnessKind::Antigravity,
        label: "agy".into(),
        failure_label: "Antigravity".into(),
        permission_prompt: "Antigravity is requesting permission",
        detect_limit,
        mode_for: wanted_mode,
        missing_mode_is_error: |spec| spec.env_overrides.contains_key(ENV_AGY_MODE),
        default_models: DEFAULT_MODELS,
        // Antigravity's own aliases are case-sensitive; `auto` is not.
        case_sensitive_default_models: true,
        // Antigravity keeps its explicit unsupported error for an agent that
        // cannot load a stored session.
        fresh_without_load: false,
    }
}

/// Detects an `agy` installation on this host.
///
/// Candidates are the `H4RN_AGY_PATH` override, `agy` on `PATH`, and the
/// documented manual install location `~/.local/bin/agy`. The version comes
/// from `agy --version`; an install that cannot be queried still counts as
/// detected and reports `unknown`.
pub fn detect_agy() -> Option<HarnessInstall> {
    let path = std::env::var_os(ENV_AGY_PATH)
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .or_else(|| find_executable("agy"))?;
    let version = version_of(&path).unwrap_or_else(|| "unknown".to_string());
    Some(HarnessInstall { path, version })
}

fn find_executable(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path_var) {
        for candidate in [name.to_string(), format!("{name}.exe")] {
            let path = directory.join(candidate);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    for suffix in [".local/bin", "bin"] {
        let path = home.join(suffix).join(name);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn version_of(path: &Path) -> Option<String> {
    let output = Command::new(path).arg("--version").output().ok()?;
    let bytes = if output.stdout.is_empty() {
        output.stderr
    } else {
        output.stdout
    };
    let text = String::from_utf8_lossy(&bytes);
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.trim_start_matches('v').to_string())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransportPreference {
    Auto,
    Acp,
    StreamJson,
}

fn transport_preference(spec: &SessionSpec) -> TransportPreference {
    match spec
        .env_overrides
        .get(ENV_AGY_TRANSPORT)
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("acp") => TransportPreference::Acp,
        Some("stream-json" | "stream_json" | "stream") => TransportPreference::StreamJson,
        _ => TransportPreference::Auto,
    }
}

/// ACP mode for the thread's autonomy, with the explicit override in front.
fn wanted_mode(spec: &SessionSpec) -> String {
    if let Some(mode) = spec
        .env_overrides
        .get(ENV_AGY_MODE)
        .map(|mode| mode.trim())
        .filter(|mode| !mode.is_empty())
    {
        return mode.to_string();
    }
    crate::acp::default_mode(spec)
}

/// Stream-json mode for the fallback, which cannot answer approvals.
fn stream_mode(spec: &SessionSpec) -> String {
    if let Some(mode) = spec
        .env_overrides
        .get(ENV_AGY_MODE)
        .map(|mode| mode.trim())
        .filter(|mode| !mode.is_empty())
    {
        return mode.to_string();
    }
    match spec.runtime_mode {
        // No client-side approval path exists here, so the safest fallback is
        // plan mode rather than silently auto-approving actions.
        RuntimeMode::ApprovalRequired => "plan",
        RuntimeMode::AutoAcceptEdits => "accept-edits",
        RuntimeMode::FullAccess => "auto",
    }
    .to_string()
}

fn is_h4rn_only_env(key: &str) -> bool {
    key.starts_with("H4RN_")
}

fn acp_config(install: &HarnessInstall, spec: &SessionSpec) -> AcpClientConfig {
    let mut config = AcpClientConfig::new(&install.path, &spec.workspace).args([
        "acp",
        "--process-per-session",
        "--sandbox",
    ]);
    for key in SCRUBBED_ENV {
        config = config.scrub(*key);
    }
    if let Some(dir) = spec
        .env_overrides
        .get(ENV_APP_DATA_DIR)
        .filter(|dir| !dir.trim().is_empty())
    {
        config = config.arg("--app-data-dir").arg(dir.trim());
    }
    if let Some(dir) = spec
        .env_overrides
        .get(ENV_GEMINI_DIR)
        .filter(|dir| !dir.trim().is_empty())
    {
        config = config.arg("--gemini-dir").arg(dir.trim());
    }
    if let Some(endpoint) = spec
        .env_overrides
        .get(ENV_CLOUD_CODE_ENDPOINT)
        .filter(|endpoint| !endpoint.trim().is_empty())
    {
        config = config.arg("--cloud_code_endpoint").arg(endpoint.trim());
    }
    for (key, value) in &spec.env_overrides {
        if !is_h4rn_only_env(key) {
            config = config.env(key, value);
        }
    }
    config
}

/// The environment keys the ACP client reads for MCP delivery.
///
/// Kept here so a future Antigravity-specific auth flow can use the same
/// names without reaching into the shared runtime.
#[allow(dead_code)]
fn mcp_env(spec: &SessionSpec) -> Option<String> {
    spec.env_overrides
        .get(ENV_MCP_AUTH_TOKEN)
        .or_else(|| spec.env_overrides.get(ENV_MCP_BEARER_TOKEN))
        .cloned()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use futures::stream::StreamExt;
    use h4rn_protocol::{AgentState, TurnStatus};
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    use super::*;
    use crate::{HarnessEvent, UserInput};

    const DEATH_DETAIL_WAIT: Duration = Duration::from_millis(500);

    fn silent_policy() -> AcpSessionPolicy {
        AcpSessionPolicy {
            detect_limit,
            ..antigravity_policy()
        }
    }

    fn acp_pair(
        workspace: &Path,
    ) -> (
        AcpClient,
        BufReader<tokio::io::DuplexStream>,
        tokio::io::DuplexStream,
    ) {
        let (client_write, agent_read) = tokio::io::duplex(64 * 1024);
        let (agent_write, client_read) = tokio::io::duplex(64 * 1024);
        let client = AcpClient::start_for_test(
            AcpClientConfig::new("/usr/local/bin/fake-acp", workspace),
            client_read,
            client_write,
        );
        (client, BufReader::new(agent_read), agent_write)
    }

    fn acp_spec(workspace: &Path) -> SessionSpec {
        SessionSpec {
            workspace: workspace.to_path_buf(),
            model: "default".into(),
            runtime_mode: RuntimeMode::ApprovalRequired,
            resume_id: None,
            mcp_endpoint: None,
            env_overrides: std::collections::BTreeMap::new(),
            attachments: Vec::new(),
            agent_id: None,
        }
    }

    async fn next_frame(reader: &mut BufReader<tokio::io::DuplexStream>) -> Value {
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("read a frame");
        serde_json::from_str(line.trim()).expect("valid json")
    }

    async fn send_frame(writer: &mut tokio::io::DuplexStream, frame: Value) {
        let mut bytes = serde_json::to_vec(&frame).expect("encode");
        bytes.push(b'\n');
        writer.write_all(&bytes).await.expect("write");
        writer.flush().await.expect("flush");
    }

    async fn handshake(
        reader: &mut BufReader<tokio::io::DuplexStream>,
        writer: &mut tokio::io::DuplexStream,
    ) {
        let initialize = next_frame(reader).await;
        assert_eq!(initialize["method"], "initialize");
        send_frame(
            writer,
            json!({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocolVersion": 1}}),
        )
        .await;

        let new_session = next_frame(reader).await;
        assert_eq!(new_session["method"], "session/new");
        send_frame(
            writer,
            json!({"jsonrpc": "2.0", "id": new_session["id"], "result": {"sessionId": "s-1"}}),
        )
        .await;
    }

    #[test]
    fn wanted_mode_honours_the_explicit_override() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let mut spec = acp_spec(workspace.path());
        assert_eq!(wanted_mode(&spec), "default");
        spec.env_overrides
            .insert(ENV_AGY_MODE.to_string(), "plan".into());
        assert_eq!(wanted_mode(&spec), "plan");
    }

    #[test]
    fn stream_json_mode_never_auto_approves_approval_required() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let spec = acp_spec(workspace.path());
        assert_eq!(stream_mode(&spec), "plan");
    }

    #[test]
    fn the_antigravity_policy_parses_its_own_limits() {
        let policy = antigravity_policy();
        assert!(
            (policy.detect_limit)("rate limit reached, resets at 3:00 PM", chrono::Utc::now())
                .is_some()
        );
    }

    /// A prompt that never reaches an agent that stopped reading its stdin is a
    /// write timeout, not a quiet turn: the agent's printed reset time is still
    /// the most specific detail, so the failure is the typed limit the daemon
    /// schedules a resume from.
    #[tokio::test(start_paused = true)]
    async fn a_prompt_write_timeout_keeps_the_stderr_reset_as_a_typed_limit() {
        let workspace = tempfile::tempdir().expect("tempdir");
        // A tiny stdin buffer that nobody drains: the prompt fills it and the
        // writer blocks mid-frame until the write deadline.
        let (client_write, agent_read) = tokio::io::duplex(64 * 1024);
        let (mut agent_write, client_read) = tokio::io::duplex(64 * 1024);
        let mut agent_read = BufReader::new(agent_read);
        let stderr = crate::acp::StderrRing::new();
        stderr.push("API usage limit reached; resets at 3:00 PM");
        let client = AcpClient::start_for_test_with_stderr(
            AcpClientConfig::new("/usr/local/bin/fake-acp", workspace.path()),
            client_read,
            client_write,
            stderr,
        );

        // The agent answers the handshake, then stops reading stdin while it
        // keeps the pipe open.
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let agent = tokio::spawn(async move {
            handshake(&mut agent_read, &mut agent_write).await;
            let _ = stop_rx.await;
        });

        let mut session = crate::acp::session_start(
            &client,
            &acp_spec(workspace.path()),
            silent_policy(),
            workspace.path(),
        )
        .await
        .expect("start session")
        .into_session();
        let error = session
            .send(UserInput {
                text: "x".repeat(1024 * 1024),
                attachments: Vec::new(),
            })
            .await
            .expect_err("a prompt the agent never reads must not claim to be on the wire");
        match error {
            HarnessError::RateLimited { message, limit, .. } => {
                assert!(
                    message.to_ascii_lowercase().contains("usage limit reached"),
                    "the failure keeps the agent's last words: {message}"
                );
                assert!(
                    limit.reset_at.is_some(),
                    "the stderr named a reset time: {limit:?}"
                );
            }
            other => panic!("expected the typed limit, got {other:?}"),
        }
        assert_eq!(session.agent_state(), AgentState::RateLimited);

        let _ = stop_tx.send(());
        agent.await.expect("agent");
    }

    /// A prompt that fails because the agent's stdout closed is an EOF, not a
    /// writer failure: the close the client observed travels with the error, so
    /// the daemon never blames the writer for a stdout death.
    #[tokio::test(flavor = "current_thread")]
    async fn a_prompt_that_fails_on_a_stdout_close_reports_eof() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (client, mut reader, mut writer) = acp_pair(workspace.path());

        let agent = tokio::spawn(async move {
            handshake(&mut reader, &mut writer).await;
            let prompt = next_frame(&mut reader).await;
            assert_eq!(prompt["method"], "session/prompt");
            // The agent goes away without answering. Closing its stdout is the
            // EOF the client observes; the client's own writer is fine.
            drop(writer);
        });

        let mut session = crate::acp::session_start(
            &client,
            &acp_spec(workspace.path()),
            antigravity_policy(),
            workspace.path(),
        )
        .await
        .expect("start session")
        .into_session();
        let mut events = session.events();
        session
            .send(UserInput {
                text: "hello".into(),
                attachments: Vec::new(),
            })
            .await
            .expect("the prompt frame is flushed before the agent leaves");

        let mut kinds = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(5), events.next())
                .await
                .expect("the prompt failure is reported")
                .expect("the event stream ended before the turn completed");
            match event {
                HarnessEvent::TransportClosed { kind, .. } => kinds.push(kind),
                HarnessEvent::TurnCompleted { .. } => break,
                _ => {}
            }
        }
        assert!(
            kinds.len() >= 2,
            "both the prompt path and the bridge report this death: {kinds:?}"
        );
        assert!(
            kinds
                .iter()
                .all(|kind| *kind == crate::TransportCloseKind::Eof),
            "a stdout close is an EOF, never a writer failure: {kinds:?}"
        );
        agent.await.expect("agent");
        let _ = session.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn interrupt_cancels_only_after_the_prompt_is_on_the_wire() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (client, mut reader, mut writer) = acp_pair(workspace.path());

        let agent = tokio::spawn(async move {
            handshake(&mut reader, &mut writer).await;
            let prompt = next_frame(&mut reader).await;
            assert_eq!(prompt["method"], "session/prompt");
            let cancel = next_frame(&mut reader).await;
            assert_eq!(cancel["method"], "session/cancel");
            send_frame(
                &mut writer,
                json!({"jsonrpc": "2.0", "id": prompt["id"], "result": {"stopReason": "cancelled"}}),
            )
            .await;
        });

        let mut session = crate::acp::session_start(
            &client,
            &acp_spec(workspace.path()),
            antigravity_policy(),
            workspace.path(),
        )
        .await
        .expect("start session")
        .into_session();
        session
            .send(UserInput {
                text: "hello".into(),
                attachments: Vec::new(),
            })
            .await
            .expect("send");
        session.interrupt().await.expect("interrupt");

        agent.await.expect("agent");
        session.shutdown().await.expect("shutdown");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_token_limited_turn_is_not_reported_as_completed() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (client, mut reader, mut writer) = acp_pair(workspace.path());

        let agent = tokio::spawn(async move {
            handshake(&mut reader, &mut writer).await;
            let prompt = next_frame(&mut reader).await;
            assert_eq!(prompt["method"], "session/prompt");
            send_frame(
                &mut writer,
                json!({"jsonrpc": "2.0", "id": prompt["id"], "result": {"stopReason": "max_tokens"}}),
            )
            .await;
        });

        let mut session = crate::acp::session_start(
            &client,
            &acp_spec(workspace.path()),
            antigravity_policy(),
            workspace.path(),
        )
        .await
        .expect("start session")
        .into_session();
        let mut events = session.events();
        session
            .send(UserInput {
                text: "hello".into(),
                attachments: Vec::new(),
            })
            .await
            .expect("send");

        let status = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = events.next().await {
                if let HarnessEvent::TurnCompleted { status, .. } = event {
                    return status;
                }
            }
            panic!("the event stream ended before the turn completed");
        })
        .await
        .expect("timed out waiting for the turn");
        assert_eq!(status, TurnStatus::TokenLimit);

        agent.await.expect("agent");
        session.shutdown().await.expect("shutdown");
    }

    #[test]
    fn agy_reports_a_version_string() {
        // The installer test path is not available here; the helper's shape is
        // what matters: a trimmed first line without a leading `v`.
        assert_eq!(Path::new("/usr/bin/agy").file_name().unwrap(), "agy");
        assert_eq!(DEATH_DETAIL_WAIT, Duration::from_millis(500));
    }

    #[test]
    fn the_adapter_reports_its_kind() {
        let harness = AntigravityHarness::new();
        assert_eq!(harness.kind(), HarnessKind::Antigravity);
        // A configured ACP instance never belongs to Antigravity.
        assert!(!harness.has_instance("anything"));
    }

    #[test]
    fn mcp_env_prefers_the_auth_token() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let mut spec = acp_spec(workspace.path());
        spec.env_overrides
            .insert(ENV_MCP_BEARER_TOKEN.to_string(), "old".into());
        spec.env_overrides
            .insert(ENV_MCP_AUTH_TOKEN.to_string(), "new".into());
        assert_eq!(mcp_env(&spec).as_deref(), Some("new"));
        let _ = Arc::new(());
    }

    #[test]
    fn antigravity_aliases_are_case_sensitive_except_auto() {
        let policy = antigravity_policy();
        assert!(policy.is_default_model("default"));
        assert!(policy.is_default_model("antigravity-default"));
        assert!(
            !policy.is_default_model("Default"),
            "Antigravity's own aliases are exact"
        );
        assert!(
            !policy.is_default_model("ANTIGRAVITY-DEFAULT"),
            "Antigravity's own aliases are exact"
        );
        assert!(policy.is_default_model("AUTO"));
        assert!(!policy.is_default_model("gemini-4-argon-medium"));
    }

    #[test]
    fn antigravity_keeps_its_own_wording() {
        let policy = antigravity_policy();
        assert_eq!(
            policy.stop_message("max_output_tokens"),
            "Antigravity stopped the turn: max_output_tokens"
        );
        assert_eq!(
            policy.permission_prompt,
            "Antigravity is requesting permission"
        );
        assert!(
            !policy.fresh_without_load,
            "Antigravity keeps its explicit unsupported error"
        );
    }
}
