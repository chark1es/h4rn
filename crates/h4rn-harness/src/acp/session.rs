//! The ACP session runtime shared by every ACP adapter.
//!
//! The Antigravity adapter and the generic local-command adapter differ in
//! how they spawn their child and in a handful of product rules (its fallback
//! transport, login flow and usage-limit parsing). Everything after the
//! handshake is the same protocol work: open or load a session, select a model
//! and mode, stream `session/update` into [`HarnessEvent`]s, answer
//! `session/request_permission`, cancel a prompt, and report a dead transport
//! as one failure row.
//!
//! That shared work lives here, parameterised by [`AcpSessionPolicy`]. An
//! adapter supplies the facts only it knows; it never re-implements the
//! mapping.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use futures::stream::{BoxStream, StreamExt};
use h4rn_protocol::{
    AgentState, ApprovalDecision, ApprovalKind, ApprovalOption, HarnessKind, PlanStep, RuntimeMode,
    ToolCall, ToolCallStatus, TurnStatus, Usage,
};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::acp::{
    AcpClient, AcpError, AcpEvent, AcpLocation, AcpMcpServer, AcpMode, AcpPermissionOption,
    AcpPermissionRequest, AcpServerInfo, AcpSessionInfo, AcpToolCall, AcpToolKind, AcpToolStatus,
    AcpUpdate, AcpUsage, AgentCapabilities, ENV_MCP_STDIO_BRIDGE, PromptContent,
};
use crate::attachments::{self, AttachmentClass, AttachmentDelivery};
use crate::{
    AttachmentRef, ENV_MCP_AUTH_TOKEN, ENV_MCP_BEARER_TOKEN, HarnessError, HarnessEvent,
    HarnessSession, LimitSignal, SessionSpec, TransportCloseKind, TransportHealth, TurnReport,
    UserInput, merge_transport_detail, most_specific_detail,
};

const EVENT_CHANNEL_CAPACITY: usize = 512;
const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const MAX_TOTAL_IMAGE_BYTES: usize = 80 * 1024 * 1024;
const MAX_TEXT_RESOURCE_BYTES: u64 = 2 * 1024 * 1024;
const TOUCHED_PATH_LIMIT: usize = 512;
const TURN_TEXT_LIMIT: usize = 16 * 1024;
/// How long a prompt that just lost its transport waits for the supervisor's
/// exit report before it publishes the turn's one failure row.
///
/// stdout can close before the process is reaped and its stderr drained; the
/// short bound lets that report (status, stderr, any rate-limit reset) reach
/// the row without holding a dead turn open.
const DEATH_DETAIL_WAIT: Duration = Duration::from_millis(500);

/// The ACP facts an adapter supplies.
///
/// The policy is deliberately small: protocol mapping never changes per
/// adapter, only how a death is named and whether a detail is a usage limit.
#[derive(Clone)]
pub(crate) struct AcpSessionPolicy {
    /// Harness kind used in typed errors.
    pub harness: HarnessKind,
    /// Label the exit report uses (`agy exited (...)`, `my-agent exited (...)`).
    pub label: String,
    /// How a failed turn names the agent (`Antigravity stopped the turn: ...`).
    pub failure_label: String,
    /// The one-line prompt when the agent raises a permission hand without a
    /// tool call to describe.
    pub permission_prompt: &'static str,
    /// The adapter's provider-limit parser. `None` means this adapter does not
    /// parse limits; the shared runtime never guesses one from text.
    pub detect_limit: fn(&str, DateTime<Utc>) -> Option<LimitSignal>,
    /// The mode this adapter asks for, given the thread's autonomy and its own
    /// environment overrides.
    pub mode_for: fn(&SessionSpec) -> String,
    /// Whether a requested mode the agent did not advertise is a caller error
    /// (the adapter had an explicit override) instead of a warning.
    pub missing_mode_is_error: fn(&SessionSpec) -> bool,
    /// Model ids that mean "the agent's own default".
    pub default_models: &'static [&'static str],
    /// How the unknown-model error names the list it prints ("the account
    /// offers" for Antigravity's account-scoped catalogue, "the agent offers"
    /// for a configured command).
    pub model_list_label: &'static str,
    /// Whether every alias except `auto` is matched exactly. Antigravity's
    /// model picker has always been case-sensitive here; a generic agent's
    /// ids are matched loosely.
    pub case_sensitive_default_models: bool,
    /// Whether a stored session id can be replaced by a fresh session when the
    /// agent does not advertise `session/load`. Generic agents start over;
    /// Antigravity keeps its explicit unsupported error.
    pub fresh_without_load: bool,
    /// Whether the v1 `configOptions` catalogue wins over the legacy
    /// `models`/`modes` objects when an agent advertises both.
    ///
    /// The catalogue and the RPC that selects from it belong together: choosing
    /// `configOptions` means selecting with `session/set_config_option`.
    /// Antigravity's account catalogue is the legacy one; a generic agent's v1
    /// options are its own.
    pub config_options_first: bool,
}

impl AcpSessionPolicy {
    fn limit(&self, text: &str) -> Option<LimitSignal> {
        (self.detect_limit)(text, Utc::now())
    }

    pub(crate) fn is_default_model(&self, model: &str) -> bool {
        self.default_models.iter().any(|alias| {
            if self.case_sensitive_default_models && !alias.eq_ignore_ascii_case("auto") {
                model == *alias
            } else {
                model.eq_ignore_ascii_case(alias)
            }
        })
    }

    pub(crate) fn stop_message(&self, reason: &str) -> String {
        format!("{} stopped the turn: {}", self.failure_label, reason)
    }

    /// The typed error for a requested model the agent did not offer.
    pub(crate) fn unknown_model(&self, requested: &str, session: &AcpSessionInfo) -> String {
        format!(
            "model `{requested}`; {}: {}",
            self.model_list_label,
            available_models(session)
        )
    }
}

/// One live ACP session.
pub(crate) struct AcpSession {
    client: AcpClient,
    session_id: String,
    events_rx: Option<mpsc::Receiver<HarnessEvent>>,
    bridge: tokio::task::JoinHandle<()>,
    prompt_task: Option<tokio::task::JoinHandle<()>>,
    shared: Arc<Mutex<AcpShared>>,
    prompt_running: Arc<AtomicBool>,
    interrupt_requested: Arc<AtomicBool>,
    shutting_down: Arc<AtomicBool>,
    capabilities: AgentCapabilities,
    close_session: bool,
    first_attachments: Vec<AttachmentRef>,
    first_send: bool,
    closed: bool,
    /// What the agent advertised when the session opened.
    info: AcpSessionInfo,
    policy: AcpSessionPolicy,
}

/// A started shared ACP session.
pub(crate) struct StartedAcpSession {
    session: AcpSession,
}

impl StartedAcpSession {
    /// The live session.
    pub(crate) fn into_session(self) -> Box<dyn HarnessSession> {
        Box::new(self.session)
    }
}

impl AcpSession {
    /// Opens a session over an already spawned client.
    ///
    /// The caller owns the client and must kill it when this returns an error.
    pub(crate) async fn start(
        client: &AcpClient,
        spec: &SessionSpec,
        policy: AcpSessionPolicy,
        workspace: &Path,
    ) -> Result<StartedAcpSession, HarnessError> {
        let info: AcpServerInfo = client
            .initialize()
            .await
            .map_err(|error| map_acp_error(error, policy.harness))?;
        if info.protocol_version == 0 {
            return Err(HarnessError::Protocol {
                harness: policy.harness,
                message: "initialize reply did not carry a protocolVersion".into(),
            });
        }
        let capabilities = info.capabilities.clone();
        let servers = mcp_servers(spec, &capabilities);

        // Whether the agent continued an existing conversation. A load/resume
        // reply can leave out the catalogue, because the agent already knows
        // the selection from the session it is continuing.
        let resumed;
        let session: AcpSessionInfo =
            match spec.resume_id.as_deref().filter(|id| !id.trim().is_empty()) {
                Some(session_id) if capabilities.supports_load() => {
                    resumed = true;
                    client
                        .load_session(session_id, workspace, &servers)
                        .await
                        .map_err(|error| map_acp_error(error, policy.harness))?
                }
                // `session/resume` is a different method, not a synonym for
                // `session/load`: an agent that only advertises resume has no
                // load, so replaying an old conversation through `load` would
                // fail. The history is not replayed here; the agent keeps it.
                Some(session_id) if capabilities.supports_resume() => {
                    resumed = true;
                    client
                        .resume_session(session_id, workspace, &servers)
                        .await
                        .map_err(|error| map_acp_error(error, policy.harness))?
                }
                // A stored id from an agent that can neither load nor resume
                // means the agent changed (or never supported either). Starting
                // over is the only honest option; the new id replaces the old.
                Some(_) if policy.fresh_without_load => {
                    resumed = false;
                    client
                        .new_session(workspace, &servers)
                        .await
                        .map_err(|error| map_acp_error(error, policy.harness))?
                }
                Some(_) => {
                    return Err(HarnessError::Unsupported {
                        harness: policy.harness,
                        feature: "resuming with session/load; this agent does not advertise it"
                            .into(),
                    });
                }
                None => {
                    resumed = false;
                    client
                        .new_session(workspace, &servers)
                        .await
                        .map_err(|error| map_acp_error(error, policy.harness))?
                }
            };
        // Which catalogue a selection RPC understands is adapter policy:
        // Antigravity's legacy `models`/`modes` objects win, a generic agent's
        // v1 `configOptions` do.
        let mut session = if policy.config_options_first {
            session.prefer_config_options()
        } else {
            session
        };

        // A resumed session already has the thread's model and mode. When the
        // reply names no catalogue there is nothing to validate them against,
        // so the saved selection is kept and the prompt is sent; failing here
        // would turn a silent resume into a dead thread.
        let unknown_model_catalogue = resumed && session.models.is_empty();
        let requested = spec.model.trim();
        if !requested.is_empty() && !policy.is_default_model(requested) && !unknown_model_catalogue
        {
            match session.resolve_model(requested) {
                Some(model_id) => {
                    if session.current_model_id.as_deref() != Some(model_id.as_str()) {
                        match session.model_config_id.as_deref() {
                            Some(config_id) => client
                                .set_config_option(&session.session_id, config_id, &model_id)
                                .await
                                .map_err(|error| map_acp_error(error, policy.harness))?,
                            None => client
                                .set_model(&session.session_id, &model_id)
                                .await
                                .map_err(|error| map_acp_error(error, policy.harness))?,
                        }
                        // The selection succeeded: the session now runs this
                        // model, so its controls must name it rather than the
                        // model the agent restored on resume.
                        session.current_model_id = Some(model_id);
                    }
                }
                None => {
                    return Err(HarnessError::Unsupported {
                        harness: policy.harness,
                        feature: policy.unknown_model(requested, &session),
                    });
                }
            }
        }

        let wanted_mode = (policy.mode_for)(spec);
        // The same mapping the in-place switch uses: an agent may name its mode
        // `acceptEdits` or `bypassPermissions`, which is still h4rn's
        // auto-accept-edits / full-access mode. An explicit environment override
        // names an exact id first, so it keeps winning.
        let resolved_mode = session
            .modes
            .iter()
            .find(|mode| mode.id == wanted_mode)
            .map(|mode| mode.id.clone())
            .or_else(|| advertised_mode_for(spec.runtime_mode, &session.modes));
        if let Some(mode_id) = resolved_mode {
            if session.current_mode_id.as_deref() != Some(mode_id.as_str()) {
                match session.mode_config_id.as_deref() {
                    Some(config_id) => client
                        .set_config_option(&session.session_id, config_id, &mode_id)
                        .await
                        .map_err(|error| map_acp_error(error, policy.harness))?,
                    None => client
                        .set_mode(&session.session_id, &mode_id)
                        .await
                        .map_err(|error| map_acp_error(error, policy.harness))?,
                }
            }
        } else if resumed && session.modes.is_empty() {
            // The reply carried no modes: the agent kept the mode the session
            // was already in, which is the thread's saved selection.
            tracing::debug!(
                target: "h4rn_harness::acp",
                harness = ?policy.harness,
                "the resumed session advertised no modes; keeping its current mode"
            );
        } else if !session.modes.is_empty()
            || spec.runtime_mode != RuntimeMode::ApprovalRequired
            || (policy.missing_mode_is_error)(spec)
        {
            // The agent named modes and none of them is this one, or it named
            // none at all: say so instead of warning and starting in a mode the
            // thread did not ask for. With no catalogue only "approval
            // required" is honest, because it asks for no mode change at all.
            let offered = if session.modes.is_empty() {
                "the agent advertised no mode catalogue".to_string()
            } else {
                format!(
                    "the agent offers: {}",
                    session
                        .modes
                        .iter()
                        .map(|mode| mode.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            return Err(HarnessError::Unsupported {
                harness: policy.harness,
                feature: format!("mode `{wanted_mode}`; {offered}"),
            });
        } else {
            tracing::warn!(
                target: "h4rn_harness::acp",
                harness = ?policy.harness,
                mode = %wanted_mode,
                "the agent did not offer a mode catalogue; keeping its current mode"
            );
        }

        // A thread that asked for an effort must get it or be refused; an
        // agent with no effort option cannot honour one.
        if let Some(requested) = spec
            .reasoning_effort
            .as_deref()
            .map(str::trim)
            .filter(|effort| !effort.is_empty())
        {
            let Some(config_id) = session.effort_config_id.as_deref() else {
                return Err(HarnessError::Unsupported {
                    harness: policy.harness,
                    feature: format!(
                        "reasoning effort `{requested}`; the agent exposes no effort option"
                    ),
                });
            };
            let Some(value) = session
                .efforts
                .iter()
                .find(|value| value.eq_ignore_ascii_case(requested))
            else {
                return Err(HarnessError::Unsupported {
                    harness: policy.harness,
                    feature: format!(
                        "reasoning effort `{requested}`; the agent offers: {}",
                        session.efforts.join(", ")
                    ),
                });
            };
            client
                .set_config_option(&session.session_id, config_id, value)
                .await
                .map_err(|error| map_acp_error(error, policy.harness))?;
            session.current_effort = Some(value.clone());
        }

        let events = client.take_events().ok_or(HarnessError::Protocol {
            harness: policy.harness,
            message: "the ACP event stream was already consumed".into(),
        })?;
        let (events_tx, events_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let shared = Arc::new(Mutex::new(AcpShared::new()));
        let prompt_running = Arc::new(AtomicBool::new(false));
        let interrupt_requested = Arc::new(AtomicBool::new(false));
        let shutting_down = Arc::new(AtomicBool::new(false));
        let bridge = spawn_event_bridge(
            events,
            events_tx.clone(),
            shared.clone(),
            shutting_down.clone(),
            policy.clone(),
        );

        events_tx
            .send(HarnessEvent::SessionStarted {
                session_id: session.session_id.clone(),
            })
            .await
            .map_err(|_| HarnessError::Transport {
                harness: policy.harness,
                message: "the harness event channel closed during startup".into(),
            })?;

        Ok(StartedAcpSession {
            session: Self {
                client: client.clone(),
                session_id: session.session_id.clone(),
                events_rx: Some(events_rx),
                bridge,
                prompt_task: None,
                shared,
                prompt_running,
                interrupt_requested,
                shutting_down,
                capabilities,
                close_session: info.capabilities.close_session,
                first_attachments: spec.attachments.clone(),
                first_send: true,
                closed: false,
                info: session,
                policy,
            },
        })
    }

    fn set_state(&self, state: AgentState) {
        lock(&self.shared).state = state;
    }
}

fn available_models(session: &AcpSessionInfo) -> String {
    session
        .models
        .iter()
        .map(|model| model.model_id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The ACP mode a thread's autonomy maps to by default.
pub(crate) fn default_mode(spec: &SessionSpec) -> String {
    mode_id_for(spec.runtime_mode).to_string()
}

/// The ACP mode id h4rn's permission mode asks for.
pub(crate) fn mode_id_for(mode: RuntimeMode) -> &'static str {
    match mode {
        RuntimeMode::ApprovalRequired => "default",
        RuntimeMode::AutoAcceptEdits => "accept-edits",
        RuntimeMode::Plan => "plan",
        RuntimeMode::FullAccess => "auto",
    }
}

/// Maps an advertised ACP mode id onto h4rn's permission modes.
///
/// An id h4rn has no label for is not guessed: it is simply not offered.
pub(crate) fn runtime_mode_for_id(id: &str) -> Option<RuntimeMode> {
    let id = id.trim().to_ascii_lowercase();
    match id.as_str() {
        "default" | "ask" | "manual" | "supervised" => Some(RuntimeMode::ApprovalRequired),
        "accept-edits" | "acceptedits" | "accept_edits" => Some(RuntimeMode::AutoAcceptEdits),
        "plan" | "read-only" | "readonly" => Some(RuntimeMode::Plan),
        "auto" | "full-access" | "full_access" | "bypass" | "bypasspermissions" => {
            Some(RuntimeMode::FullAccess)
        }
        _ => None,
    }
}

/// The id of the advertised mode that maps to `mode`.
fn advertised_mode_for(mode: RuntimeMode, modes: &[AcpMode]) -> Option<String> {
    modes
        .iter()
        .find(|advertised| runtime_mode_for_id(&advertised.id) == Some(mode))
        .map(|advertised| advertised.id.clone())
}

fn mcp_servers(spec: &SessionSpec, capabilities: &AgentCapabilities) -> Vec<AcpMcpServer> {
    let Some(endpoint) = spec
        .mcp_endpoint
        .as_deref()
        .map(str::trim)
        .filter(|endpoint| !endpoint.is_empty())
    else {
        return Vec::new();
    };
    let token = spec
        .env_overrides
        .get(ENV_MCP_AUTH_TOKEN)
        .or_else(|| spec.env_overrides.get(ENV_MCP_BEARER_TOKEN))
        .map(String::as_str);
    if capabilities.supports_http_mcp() {
        return vec![AcpMcpServer::hub_http(endpoint, token)];
    }
    let bridge = spec
        .env_overrides
        .get(ENV_MCP_STDIO_BRIDGE)
        .map(String::as_str);
    vec![AcpMcpServer::hub_stdio(endpoint, bridge)]
}

/// Maps one client error to the adapter's typed error.
pub(crate) fn map_acp_error(error: AcpError, harness: HarnessKind) -> HarnessError {
    match error {
        AcpError::Spawn { message, .. } => HarnessError::Spawn { harness, message },
        AcpError::Closed { message, .. } => HarnessError::Transport { harness, message },
        AcpError::Protocol { message } => HarnessError::Protocol { harness, message },
        AcpError::Rpc { code, message, .. } => HarnessError::Protocol {
            harness,
            message: format!("agent error {code}: {message}"),
        },
        AcpError::Timeout { method, seconds } => HarnessError::Transport {
            harness,
            message: format!("`{method}` timed out after {seconds}s"),
        },
        AcpError::Cancelled { method } => HarnessError::Transport {
            harness,
            message: format!("`{method}` was cancelled"),
        },
        AcpError::OutsideWorkspace { path } => HarnessError::Protocol {
            harness,
            message: format!("agent requested a path outside the workspace: {path}"),
        },
        AcpError::Unsupported { feature } => HarnessError::Unsupported { harness, feature },
    }
}

#[async_trait]
impl HarnessSession for AcpSession {
    fn kind(&self) -> HarnessKind {
        self.policy.harness
    }

    fn controls(&self) -> Option<crate::SessionControls> {
        let modes = self
            .info
            .modes
            .iter()
            .filter_map(|mode| runtime_mode_for_id(&mode.id))
            .collect::<Vec<_>>();
        Some(crate::SessionControls {
            modes,
            models: (!self.info.models.is_empty()).then(|| {
                self.info
                    .models
                    .iter()
                    .map(|model| model.model_id.clone())
                    .collect()
            }),
            current_model: self.info.current_model_id.clone(),
            default_models: self
                .policy
                .default_models
                .iter()
                .map(|alias| (*alias).to_string())
                .collect(),
            efforts: self.info.efforts.clone(),
            fast: false,
            switch_mode: true,
            switch_model: true,
            // Attempt an effort switch even when the agent advertised no
            // option: the daemon must refuse that request, not defer a change
            // the next start cannot honour either.
            switch_effort: true,
            switch_fast: false,
        })
    }

    async fn set_runtime_mode(&mut self, mode: RuntimeMode) -> Result<(), HarnessError> {
        if self.closed || self.shutting_down.load(Ordering::SeqCst) {
            return Err(HarnessError::Transport {
                harness: self.policy.harness,
                message: "session is shut down".into(),
            });
        }
        let Some(mode_id) = advertised_mode_for(mode, &self.info.modes) else {
            return Err(HarnessError::Unsupported {
                harness: self.policy.harness,
                feature: format!(
                    "mode `{}`; the agent offers: {}",
                    mode_id_for(mode),
                    self.info
                        .modes
                        .iter()
                        .map(|mode| mode.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            });
        };
        let result = match self.info.mode_config_id.as_deref() {
            Some(config_id) => {
                self.client
                    .set_config_option(&self.session_id, config_id, &mode_id)
                    .await
            }
            None => self.client.set_mode(&self.session_id, &mode_id).await,
        };
        result.map_err(|error| map_acp_error(error, self.policy.harness))?;
        self.info.current_mode_id = Some(mode_id);
        Ok(())
    }

    async fn set_model(&mut self, model: &str) -> Result<(), HarnessError> {
        if self.closed || self.shutting_down.load(Ordering::SeqCst) {
            return Err(HarnessError::Transport {
                harness: self.policy.harness,
                message: "session is shut down".into(),
            });
        }
        let model = model.trim();
        // ACP has no "reset to default" method and the catalogue does not name
        // which entry is the default, so a live session cannot honour the
        // alias. It is still a valid request: the next session can start from
        // the agent's own default, so the change is deferred rather than
        // refused. Silently keeping the current model while the summary says
        // `default` would not be honest; detaching the session for the next
        // turn is.
        if self.policy.is_default_model(model) {
            return Err(HarnessError::Deferred {
                harness: self.policy.harness,
                feature: format!(
                    "the agent's default model (choose a model id to switch now: {})",
                    available_models(&self.info)
                ),
            });
        }
        let Some(model_id) = self.info.resolve_model(model) else {
            return Err(HarnessError::Unsupported {
                harness: self.policy.harness,
                feature: self.policy.unknown_model(model, &self.info),
            });
        };
        let result = match self.info.model_config_id.as_deref() {
            Some(config_id) => {
                self.client
                    .set_config_option(&self.session_id, config_id, &model_id)
                    .await
            }
            None => self.client.set_model(&self.session_id, &model_id).await,
        };
        result.map_err(|error| map_acp_error(error, self.policy.harness))?;
        self.info.current_model_id = Some(model_id);
        Ok(())
    }

    async fn set_reasoning_effort(&mut self, effort: Option<&str>) -> Result<(), HarnessError> {
        let Some(effort) = effort.map(str::trim).filter(|effort| !effort.is_empty()) else {
            return Err(HarnessError::Unsupported {
                harness: self.policy.harness,
                feature: "resetting reasoning effort on a live session; restart the session".into(),
            });
        };
        let Some(config_id) = self.info.effort_config_id.clone() else {
            return Err(HarnessError::Unsupported {
                harness: self.policy.harness,
                feature: format!("reasoning effort `{effort}`; the agent exposes no effort option"),
            });
        };
        let Some(value) = self
            .info
            .efforts
            .iter()
            .find(|value| value.eq_ignore_ascii_case(effort))
            .cloned()
        else {
            return Err(HarnessError::Unsupported {
                harness: self.policy.harness,
                feature: format!(
                    "reasoning effort `{effort}`; the agent offers: {}",
                    self.info.efforts.join(", ")
                ),
            });
        };
        self.client
            .set_config_option(&self.session_id, &config_id, &value)
            .await
            .map_err(|error| map_acp_error(error, self.policy.harness))?;
        self.info.current_effort = Some(value);
        Ok(())
    }

    async fn send(&mut self, input: UserInput) -> Result<(), HarnessError> {
        let harness = self.policy.harness;
        if self.closed || self.shutting_down.load(Ordering::SeqCst) {
            return Err(HarnessError::NotSent {
                harness,
                message: "session is shut down".into(),
            });
        }
        // A Stop that arrived after the previous turn's prompt had already
        // completed must not colour this turn; the flag belongs to one prompt.
        // It is cleared only once this send owns the prompt slot, so a rejected
        // concurrent send cannot clear a running turn's stop flag.
        if self.prompt_running.swap(true, Ordering::SeqCst) {
            return Err(HarnessError::Transport {
                harness,
                message: "a turn is already running".into(),
            });
        }
        self.interrupt_requested.store(false, Ordering::SeqCst);

        let mut attachments = input.attachments;
        if self.first_send {
            self.first_send = false;
            attachments.extend(self.first_attachments.iter().cloned());
        }
        let content = match build_prompt(
            &input.text,
            &attachments,
            &self.capabilities,
            self.policy.harness,
        ) {
            Ok(content) => content,
            Err(error) => {
                self.prompt_running.store(false, Ordering::SeqCst);
                return Err(error);
            }
        };

        {
            let mut shared = lock(&self.shared);
            shared.turn_text.clear();
            shared.tool_labels.clear();
            shared.state = AgentState::Thinking;
        }

        let client = self.client.clone();
        let session_id = self.session_id.clone();
        let shared = self.shared.clone();
        let prompt_running = self.prompt_running.clone();
        let interrupt_requested = self.interrupt_requested.clone();
        let started = Instant::now();
        let policy = self.policy.clone();

        // Wait until `session/prompt` is on the agent's stdin before returning:
        // an interrupt after this point can safely be sent as a cancel.
        //
        // Tell the bridge a turn is starting before the frame can be read: a
        // `session/update` that follows is this turn's, while one that follows
        // the settlement of the previous turn is not (and is dropped there).
        // The generation tags this turn so a settlement only closes its own
        // prompt's span.
        let generation = client.begin_prompt();
        let _ = client.queue_behind_reader(AcpEvent::PromptStarted { generation });
        let pending = match client.start_prompt(&session_id, &content).await {
            Ok(pending) => pending,
            Err(error) => {
                self.prompt_running.store(false, Ordering::SeqCst);
                // No turn is running after this failed write: close the span
                // the `PromptStarted` marker opened, so a late update from the
                // previous turn is dropped instead of reaching this one.
                let _ = client.queue_behind_reader(AcpEvent::Settled {
                    generation,
                    events: Vec::new(),
                });
                // A prompt that failed before its frame reached the agent is
                // the same death the prompt task would report: name the I/O
                // error, the agent's stderr and its exit, so the daemon's one
                // failure row keeps the agent's last words and any rate-limit
                // reset in them. A write timeout kills the agent just like a
                // close, so it must also read the stderr the agent printed
                // before it stopped reading: that is where a provider limit
                // names its reset, and the daemon arms the resume from it.
                let detail = if matches!(error, AcpError::Closed { .. } | AcpError::Timeout { .. })
                {
                    prompt_write_detail(&client, error.to_string(), &policy.label).await
                } else {
                    error.to_string()
                };
                let limit = policy.limit(&detail);
                {
                    let mut shared = lock(&self.shared);
                    shared.state = if limit.is_some() {
                        AgentState::RateLimited
                    } else {
                        AgentState::Error
                    };
                    shared.last_turn = Some(TurnReport {
                        num_turns: 0,
                        duration_ms: millis(started.elapsed()),
                        duration_api_ms: 0,
                        cost_micros: 0,
                        is_error: true,
                        stop_reason: None,
                        subtype: Some("error".into()),
                    });
                }
                // A closed writer before the prompt frame was accepted is a
                // definite pre-write failure: the agent never saw the text, so
                // the daemon can put it back instead of treating it as unclear.
                let unwritten = matches!(error, AcpError::Closed { .. });
                let mapped = map_acp_error(error, harness);
                // A parsed limit travels as its own typed error so the daemon
                // can arm the resume without parsing the text of a failure
                // that came from another adapter. Everything else keeps the
                // merged detail and the error variant the ACP layer chose.
                return Err(match limit {
                    Some(limit) => HarnessError::RateLimited {
                        harness,
                        message: detail,
                        limit,
                    },
                    None => match mapped {
                        HarnessError::Transport { harness, .. } if unwritten => {
                            HarnessError::NotSent {
                                harness,
                                message: detail,
                            }
                        }
                        HarnessError::Transport { harness, .. } => HarnessError::Transport {
                            harness,
                            message: detail,
                        },
                        other => other,
                    },
                });
            }
        };

        let task_policy = policy.clone();
        self.prompt_task = Some(tokio::spawn(async move {
            let policy = task_policy;
            let result = pending.wait().await;
            let duration_ms = millis(started.elapsed());
            // Read the stop flag before releasing the prompt slot: a new send
            // clears the flag for its own turn, so this turn must classify
            // first.
            let interrupted = interrupt_requested.swap(false, Ordering::SeqCst)
                || result.as_ref().is_ok_and(|prompt| prompt.cancelled());
            prompt_running.store(false, Ordering::SeqCst);
            match result {
                Ok(prompt) => {
                    let usage = usage_from_acp(&prompt.usage, duration_ms);
                    let turn_text = lock(&shared).turn_text.clone();
                    let limit = policy.limit(&turn_text);
                    let status = if interrupted {
                        TurnStatus::Interrupted
                    } else if prompt.completed() {
                        TurnStatus::Completed
                    } else if prompt.token_limited() {
                        TurnStatus::TokenLimit
                    } else if prompt.turn_limited() {
                        TurnStatus::TurnLimit
                    } else {
                        TurnStatus::Failed
                    };
                    {
                        let mut shared = lock(&shared);
                        shared.last_turn = Some(TurnReport {
                            num_turns: 1,
                            duration_ms,
                            duration_api_ms: 0,
                            cost_micros: 0,
                            is_error: status == TurnStatus::Failed,
                            stop_reason: (!prompt.stop_reason.is_empty())
                                .then(|| prompt.stop_reason.clone()),
                            subtype: None,
                        });
                        shared.state = turn_state(status, limit.is_some());
                    }
                    let mut settled = Vec::new();
                    if let Some(signal) = limit {
                        settled.push(HarnessEvent::LimitHit {
                            reset_at: signal.reset_at,
                            auto_resume: signal.reset_at.is_some(),
                        });
                    }
                    if !prompt.usage.is_empty() {
                        settled.push(HarnessEvent::Usage {
                            usage: usage.clone(),
                        });
                    }
                    if status == TurnStatus::Failed {
                        let message = if prompt.stop_reason.trim().is_empty() {
                            "the agent ended the turn without a stop reason".to_owned()
                        } else {
                            policy.stop_message(&prompt.stop_reason)
                        };
                        settled.push(HarnessEvent::Error {
                            message,
                            retryable: true,
                        });
                    }
                    settled.push(HarnessEvent::TurnCompleted { status, usage });
                    // Queued behind the frames the reader has already read, so
                    // the turn's last update can never arrive after its
                    // completion (see `AcpEvent::Settled`).
                    let _ = client.queue_behind_reader(AcpEvent::Settled {
                        generation,
                        events: settled,
                    });
                }
                Err(error) => {
                    // A transport death reaches this task and the bridge
                    // separately: the stdout close fails the prompt here, while
                    // the supervisor reaps the process and builds its exit
                    // report (status plus the stderr the reader drained). The
                    // close can win that race, so wait briefly for the report:
                    // the one row this turn publishes should name the exit and
                    // the agent's last words - including any rate-limit reset -
                    // rather than settle for the bare close notice.
                    let exit = if client.is_supervised() && matches!(error, AcpError::Closed { .. })
                    {
                        client.wait_for_exit(DEATH_DETAIL_WAIT).await
                    } else {
                        None
                    };
                    let exit = exit
                        .as_ref()
                        .map(|report| exit_detail(&policy.label, &report.status, &report.stderr));
                    let mut detail = error.to_string();
                    match &exit {
                        // The exit report is this death's specific account: it
                        // wins over the generic close notice even when its text
                        // is shorter, and it already appends the stderr it
                        // drained.
                        Some(report) => {
                            detail = merge_transport_detail(detail, true, report.clone(), false);
                        }
                        None => {
                            // No process to wait for (an in-process transport):
                            // the ring's tail is all the adapter knows.
                            let stderr = client.stderr_tail();
                            detail = merged_detail(detail, &stderr);
                        }
                    }
                    let limit = policy.limit(&detail);
                    let usage = Usage {
                        duration_ms,
                        ..Usage::default()
                    };
                    {
                        let mut shared = lock(&shared);
                        shared.state = if limit.is_some() {
                            AgentState::RateLimited
                        } else {
                            AgentState::Error
                        };
                        shared.last_turn = Some(TurnReport {
                            num_turns: 0,
                            duration_ms,
                            duration_api_ms: 0,
                            cost_micros: 0,
                            is_error: true,
                            stop_reason: None,
                            subtype: Some("error".into()),
                        });
                    }
                    let mut settled = Vec::new();
                    if let Some(signal) = limit {
                        settled.push(HarnessEvent::LimitHit {
                            reset_at: signal.reset_at,
                            auto_resume: signal.reset_at.is_some(),
                        });
                    } else {
                        // A prompt that failed because the transport is gone is
                        // the same death the bridge reports on its own stream:
                        // tag it so the two are one row. Any other failure here
                        // (a protocol or RPC error) stays an ordinary error and
                        // is never collapsed with a transport death. `wait()`
                        // takes no deadline, so `Closed` is the only transport
                        // error this task can see.
                        let message = detail.trim().to_string();
                        // A report that carries the exit status and the drained
                        // stderr is the non-retryable exit report, exactly as
                        // the bridge tags the same death; only the bare close
                        // notice is retryable.
                        let retryable = exit.is_none()
                            && !matches!(
                                error,
                                AcpError::Protocol { .. } | AcpError::OutsideWorkspace { .. }
                            );
                        let event = if let AcpError::Closed { kind, .. } = &error {
                            // The kind is the close the client observed: a
                            // closed stdout is an EOF, a failed writer a write
                            // failure, a reaped process the exit report. A
                            // reaped process outranks it, because its report is
                            // the specific account of the death.
                            HarnessEvent::TransportClosed {
                                message,
                                retryable,
                                kind: if exit.is_some() {
                                    TransportCloseKind::Exit
                                } else {
                                    *kind
                                },
                                // A death whose detail names a reset is in the
                                // limit branch above, so this report carries
                                // none.
                                reset_at: None,
                            }
                        } else {
                            HarnessEvent::Error { message, retryable }
                        };
                        settled.push(event);
                    }
                    settled.push(HarnessEvent::TurnCompleted {
                        status: TurnStatus::Failed,
                        usage,
                    });
                    // Queued behind the frames the reader has already read, so
                    // the failure row and completion land after every update
                    // this turn produced (see `AcpEvent::Settled`).
                    let _ = client.queue_behind_reader(AcpEvent::Settled {
                        generation,
                        events: settled,
                    });
                }
            }
        }));

        Ok(())
    }

    async fn interrupt(&mut self) -> Result<(), HarnessError> {
        let harness = self.policy.harness;
        if self.closed || self.shutting_down.load(Ordering::SeqCst) {
            return Err(HarnessError::Transport {
                harness,
                message: "session is shut down".into(),
            });
        }
        // `send` only returns once `session/prompt` was flushed, so a running
        // prompt is always on the wire by the time a cancel can reach here.
        // That holds because the daemon serializes `send` and `interrupt` on
        // this session's lock: a cancel can only run after the send that took
        // the prompt slot has returned.
        if self.prompt_running.load(Ordering::SeqCst) {
            self.interrupt_requested.store(true, Ordering::SeqCst);
            self.client
                .cancel(&self.session_id)
                .await
                .map_err(|error| map_acp_error(error, harness))?;
        }
        Ok(())
    }

    async fn respond_approval(
        &mut self,
        approval_id: &str,
        decision: ApprovalDecision,
        option_id: Option<&str>,
    ) -> Result<(), HarnessError> {
        let harness = self.policy.harness;
        if self.closed || self.shutting_down.load(Ordering::SeqCst) {
            return Err(HarnessError::Transport {
                harness,
                message: "session is shut down".into(),
            });
        }
        let pending = lock(&self.shared).pending.remove(approval_id);
        let Some(pending) = pending else {
            return Err(HarnessError::Protocol {
                harness,
                message: format!("approval `{approval_id}` is not pending"),
            });
        };
        let selected = select_permission_option(&pending.options, decision, option_id);
        if decision == ApprovalDecision::Approve && selected.is_none() {
            return Err(HarnessError::Protocol {
                harness,
                message: "the agent offered no allow option for this request".into(),
            });
        }
        self.client
            .respond_permission(&pending.request_id, selected.as_deref())
            .await
            .map_err(|error| map_acp_error(error, harness))?;
        self.set_state(AgentState::Thinking);
        Ok(())
    }

    fn events(&mut self) -> BoxStream<'static, HarnessEvent> {
        match self.events_rx.take() {
            Some(receiver) => ReceiverStream::new(receiver).boxed(),
            None => futures::stream::empty().boxed(),
        }
    }

    fn session_id(&self) -> Option<String> {
        Some(self.session_id.clone())
    }

    fn is_closed(&self) -> bool {
        self.closed || self.client.is_closed()
    }

    fn transport_health(&self) -> Option<TransportHealth> {
        Some(self.client.transport_health())
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
        self.client.stderr_tail()
    }

    async fn shutdown(&mut self) -> Result<(), HarnessError> {
        let harness = self.policy.harness;
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.shutting_down.store(true, Ordering::SeqCst);

        if self.close_session
            && let Err(error) = self.client.close_session(&self.session_id).await
        {
            tracing::debug!(
                target: "h4rn_harness::acp",
                %error,
                "session/close failed; shutdown continues"
            );
        }
        if let Some(task) = self.prompt_task.take() {
            task.abort();
        }
        let outcome = self
            .client
            .shutdown()
            .await
            .map_err(|error| map_acp_error(error, harness));
        self.bridge.abort();
        outcome
    }
}

impl Drop for AcpSession {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        self.shutting_down.store(true, Ordering::SeqCst);
        if let Some(task) = self.prompt_task.take() {
            task.abort();
        }
        self.bridge.abort();
        // Best effort: never leave an orphaned agent (ADR-021).
        self.client.kill();
    }
}

#[derive(Debug)]
struct PendingApproval {
    request_id: Value,
    options: Vec<AcpPermissionOption>,
}

#[derive(Debug)]
struct AcpShared {
    state: AgentState,
    touched: Vec<String>,
    tool_started: HashMap<String, DateTime<Utc>>,
    tool_labels: HashMap<String, ToolLabel>,
    pending: HashMap<String, PendingApproval>,
    turn_text: String,
    last_turn: Option<TurnReport>,
}

/// The raw fields of a tool call, merged across its `tool_call` and
/// `tool_call_update` frames.
///
/// Updates often carry only a status or output. Caching the raw fields instead
/// of a derived name and summary lets a later title change reach the row while
/// a status-only update cannot rename a kind-only call to "other".
#[derive(Debug, Default)]
struct ToolLabel {
    /// Last non-empty `name` the agent sent.
    name: Option<String>,
    /// Last non-empty title.
    title: Option<String>,
    /// Last kind the agent sent.
    kind: Option<AcpToolKind>,
    /// Last raw input the agent sent.
    raw_input: Option<Value>,
    /// Last locations the agent sent.
    locations: Vec<AcpLocation>,
}

impl ToolLabel {
    /// Merges the fields this frame carries; absent fields keep their value.
    fn merge(&mut self, call: &AcpToolCall) {
        if let Some(name) = call
            .raw
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
        {
            self.name = Some(name.to_owned());
        }
        if let Some(title) = call.title.as_deref().filter(|title| !title.is_empty()) {
            self.title = Some(title.to_owned());
        }
        if call.raw.get("kind").is_some() {
            self.kind = Some(call.kind.clone());
        }
        if let Some(input) = call
            .raw
            .get("rawInput")
            .or_else(|| call.raw.get("raw_input"))
        {
            self.raw_input = Some(input.clone());
        }
        if call.raw.get("locations").is_some() {
            self.locations = call.locations.clone();
        }
    }

    fn kind(&self) -> AcpToolKind {
        self.kind
            .clone()
            .unwrap_or(AcpToolKind::Other("other".into()))
    }

    fn name(&self) -> String {
        self.name
            .clone()
            .or_else(|| self.title.clone())
            .unwrap_or_else(|| self.kind().label().to_owned())
    }

    fn summary(&self) -> String {
        AcpToolCall::summary_of(
            self.title.as_deref(),
            self.raw_input.as_ref(),
            &self.locations,
        )
    }
}

impl AcpShared {
    fn new() -> Self {
        Self {
            state: AgentState::Idle,
            touched: Vec::new(),
            tool_started: HashMap::new(),
            tool_labels: HashMap::new(),
            pending: HashMap::new(),
            turn_text: String::new(),
            last_turn: None,
        }
    }
}

fn spawn_event_bridge(
    mut events: mpsc::Receiver<AcpEvent>,
    events_tx: mpsc::Sender<HarnessEvent>,
    shared: Arc<Mutex<AcpShared>>,
    shutting_down: Arc<AtomicBool>,
    policy: AcpSessionPolicy,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // The prompt the bridge is following, if any. Updates can only belong
        // to a turn between its `PromptStarted` and its settlement, and only
        // when their generation is that prompt's: an update outside that span
        // (or a finished turn's late one) has no turn to land on.
        let mut running: Option<u64> = Some(0);
        while let Some(event) = events.recv().await {
            let mapped: Vec<HarnessEvent> = match event {
                AcpEvent::PromptStarted { generation } => {
                    running = Some(generation);
                    Vec::new()
                }
                AcpEvent::SessionUpdate { generation, .. } if running != Some(generation) => {
                    tracing::debug!(
                        target: "h4rn_harness::acp",
                        generation,
                        running = ?running,
                        "dropped a session update that belongs to no running turn"
                    );
                    Vec::new()
                }
                AcpEvent::SessionUpdate { update, .. } => map_update(&update, &shared),
                AcpEvent::PermissionRequest {
                    request_id,
                    request,
                } => vec![register_approval(&request_id, &request, &shared, &policy)],
                AcpEvent::Notification { params, .. } => policy
                    .limit(&params.to_string())
                    .map(|signal| {
                        vec![HarnessEvent::LimitHit {
                            reset_at: signal.reset_at,
                            auto_resume: signal.reset_at.is_some(),
                        }]
                    })
                    .unwrap_or_default(),
                AcpEvent::Closed { message, kind } => {
                    if shutting_down.load(Ordering::SeqCst) {
                        Vec::new()
                    } else {
                        vec![HarnessEvent::TransportClosed {
                            message,
                            retryable: true,
                            // The reader observed whether the stdout ended or a
                            // read failed; carry that instead of calling every
                            // close an EOF.
                            kind,
                            reset_at: None,
                        }]
                    }
                }
                AcpEvent::Exited {
                    status,
                    stderr,
                    deliberate,
                } => {
                    if deliberate || shutting_down.load(Ordering::SeqCst) {
                        // The client ended the process on purpose (a write
                        // deadline, a shutdown). The failure that caused it is
                        // already on the stream; a second exit row would
                        // duplicate it.
                        Vec::new()
                    } else {
                        // The exit report is always the specific account of the
                        // death, even when its stderr carries a typed limit: the
                        // limit is applied first, and the report itself still
                        // tells the pump that the session is dead so it detaches
                        // it instead of waiting for a stream end the held
                        // session keeps from happening.
                        let limit = policy.limit(&stderr);
                        let reset_at = limit.as_ref().and_then(|signal| signal.reset_at);
                        let mut events = Vec::new();
                        if let Some(signal) = limit {
                            events.push(HarnessEvent::LimitHit {
                                reset_at: signal.reset_at,
                                auto_resume: signal.reset_at.is_some(),
                            });
                        }
                        events.push(HarnessEvent::TransportClosed {
                            message: exit_detail(&policy.label, &status, &stderr),
                            retryable: false,
                            kind: TransportCloseKind::Exit,
                            // The report carries the reset too, so a consumer
                            // that only kept the exit report (the daemon's
                            // closed-completion wait, which may have already
                            // applied the typed limit ahead of it) still has
                            // it.
                            reset_at,
                        });
                        events
                    }
                }
                AcpEvent::Error { message, retryable } => {
                    vec![HarnessEvent::Error { message, retryable }]
                }
                AcpEvent::TransportClosed {
                    message,
                    retryable,
                    kind,
                } => {
                    vec![HarnessEvent::TransportClosed {
                        message,
                        retryable,
                        kind,
                        reset_at: None,
                    }]
                }
                // The prompt task queued these behind the frames it had
                // already seen, so they arrive after the turn's own updates
                // and before anything the reader reads next. Only the prompt
                // they name may close the running span: a settlement that
                // arrives after a newer prompt started belongs to the turn
                // before it and leaves that turn alone.
                AcpEvent::Settled { generation, events } => {
                    if running == Some(generation) {
                        running = None;
                    }
                    events
                }
            };
            for event in mapped {
                if events_tx.send(event).await.is_err() {
                    return;
                }
            }
        }
    })
}

/// Names what a prompt that never reached the agent's stdin left behind.
async fn prompt_write_detail(client: &AcpClient, detail: String, label: &str) -> String {
    let mut detail = detail;
    if client.is_supervised()
        && let Some(report) = client.wait_for_exit(DEATH_DETAIL_WAIT).await
    {
        detail = merged_detail(detail, &report.stderr);
        return merged_detail(detail, &exit_detail(label, &report.status, &report.stderr));
    }
    merged_detail(detail, &client.stderr_tail())
}

/// The non-retryable exit/status report's text.
///
/// The exit status is the specific account of the death, so it leads, and the
/// stderr the reader drained is appended when it has any. Both the bridge and
/// the failed-prompt path name a death this way, so the daemon merges the two
/// reports into the same row.
fn exit_detail(label: &str, status: &str, stderr: &str) -> String {
    let stderr = stderr.trim();
    if stderr.is_empty() {
        format!("{label} exited ({status})")
    } else {
        format!("{label} exited ({status}): {stderr}")
    }
}

/// Adds `extra` to a failure's detail when it says something new.
///
/// The agent's stderr and the exit status describe the same death as the
/// transport error, so they are merged instead of published as extra rows.
fn merged_detail(detail: String, extra: &str) -> String {
    let extra = extra.trim();
    if extra.is_empty() || detail.contains(extra) {
        return detail;
    }
    most_specific_detail(detail.clone(), format!("{detail}\n{extra}"))
}

fn register_approval(
    request_id: &Value,
    request: &AcpPermissionRequest,
    shared: &Arc<Mutex<AcpShared>>,
    policy: &AcpSessionPolicy,
) -> HarnessEvent {
    let approval_id = approval_id(request_id);
    let kind = approval_kind(request);
    let prompt = approval_prompt(request, policy);
    let options = request
        .options
        .iter()
        .map(|option| ApprovalOption {
            id: option.option_id.clone(),
            label: option.name.clone(),
        })
        .collect();
    {
        let mut shared = lock(shared);
        shared.pending.insert(
            approval_id.clone(),
            PendingApproval {
                request_id: request_id.clone(),
                options: request.options.clone(),
            },
        );
        shared.state = AgentState::AwaitingApproval;
    }
    HarnessEvent::ApprovalRequested {
        approval_id,
        kind,
        prompt,
        options,
    }
}

fn approval_id(request_id: &Value) -> String {
    match request_id {
        Value::String(id) => id.clone(),
        other => other.to_string(),
    }
}

fn approval_kind(request: &AcpPermissionRequest) -> ApprovalKind {
    let Some(call) = &request.tool_call else {
        return ApprovalKind::Permission;
    };
    let name = tool_name(call)
        .unwrap_or_else(|| call.kind.label().to_string())
        .to_ascii_lowercase();
    if name.contains("ask_question") {
        return ApprovalKind::Question;
    }
    match call.kind {
        AcpToolKind::Execute => ApprovalKind::Command,
        AcpToolKind::Edit => ApprovalKind::FileChange,
        _ => ApprovalKind::Permission,
    }
}

fn approval_prompt(request: &AcpPermissionRequest, policy: &AcpSessionPolicy) -> String {
    let Some(call) = &request.tool_call else {
        return policy.permission_prompt.into();
    };
    let name = tool_name(call).unwrap_or_else(|| call.kind.label().to_string());
    let summary = call.summary();
    if summary.is_empty() {
        format!("Allow `{name}`?")
    } else {
        format!("Allow `{name}` to {summary}?")
    }
}

fn select_permission_option(
    options: &[AcpPermissionOption],
    decision: ApprovalDecision,
    requested: Option<&str>,
) -> Option<String> {
    if let Some(requested) = requested
        && options.iter().any(|option| option.option_id == requested)
    {
        return Some(requested.to_string());
    }
    match decision {
        ApprovalDecision::Approve => options
            .iter()
            .find(|option| is_allow_option(option))
            .map(|option| option.option_id.clone()),
        ApprovalDecision::Deny => options
            .iter()
            .find(|option| is_reject_option(option))
            .map(|option| option.option_id.clone()),
        ApprovalDecision::Cancel => None,
    }
}

fn is_allow_option(option: &AcpPermissionOption) -> bool {
    option.kind.starts_with("allow")
        || option.option_id.eq_ignore_ascii_case("allow")
        || option.option_id.to_ascii_lowercase().contains("approve")
}

fn is_reject_option(option: &AcpPermissionOption) -> bool {
    option.kind.starts_with("reject")
        || option.kind.starts_with("deny")
        || option.option_id.eq_ignore_ascii_case("deny")
        || option.option_id.to_ascii_lowercase().contains("reject")
}

fn map_update(update: &AcpUpdate, shared: &Arc<Mutex<AcpShared>>) -> Vec<HarnessEvent> {
    match update {
        AcpUpdate::AgentMessageChunk { .. } => match update.text() {
            Some(text) => {
                remember_turn_text(shared, &text);
                lock(shared).state = AgentState::Thinking;
                vec![HarnessEvent::TextDelta { text }]
            }
            None => Vec::new(),
        },
        AcpUpdate::AgentThoughtChunk { .. } => {
            if let Some(text) = update.text() {
                remember_turn_text(shared, &text);
            }
            lock(shared).state = AgentState::Thinking;
            Vec::new()
        }
        AcpUpdate::ToolCall { tool_call } | AcpUpdate::ToolCallUpdate { tool_call } => {
            vec![tool_event(tool_call, shared)]
        }
        AcpUpdate::Plan { entries } => {
            lock(shared).state = AgentState::Thinking;
            let entries = entries
                .iter()
                .map(|entry| PlanStep {
                    content: entry.content.clone(),
                    status: entry.status.clone(),
                    priority: entry.priority.clone(),
                })
                .collect();
            vec![HarnessEvent::Plan { entries }]
        }
        AcpUpdate::Usage { usage } => {
            if usage.is_empty() {
                Vec::new()
            } else {
                vec![HarnessEvent::Usage {
                    usage: usage_from_acp(usage, 0),
                }]
            }
        }
        _ => Vec::new(),
    }
}

fn remember_turn_text(shared: &Arc<Mutex<AcpShared>>, text: &str) {
    let mut shared = lock(shared);
    if shared.turn_text.len().saturating_add(text.len()) > TURN_TEXT_LIMIT {
        let keep = TURN_TEXT_LIMIT.saturating_sub(text.len());
        let mut split = shared.turn_text.len().saturating_sub(keep);
        while split < shared.turn_text.len() && !shared.turn_text.is_char_boundary(split) {
            split += 1;
        }
        shared.turn_text = shared.turn_text[split..].to_string();
    }
    shared.turn_text.push_str(text);
}

fn tool_event(call: &AcpToolCall, shared: &Arc<Mutex<AcpShared>>) -> HarnessEvent {
    let tool_call_id = call.tool_call_id.clone();
    let output = call.output_preview();
    let status = tool_status(&call.status);
    let finished = matches!(
        call.status,
        AcpToolStatus::Completed | AcpToolStatus::Failed | AcpToolStatus::Cancelled
    );
    let mut shared = lock(shared);
    // `tool_call_update` often carries only a status or output; merge it into
    // the fields the agent already sent so the name and summary stay stable.
    let (name, summary, kind, locations, input) = {
        let label = shared.tool_labels.entry(tool_call_id.clone()).or_default();
        label.merge(call);
        (
            label.name(),
            label.summary(),
            label.kind(),
            label.locations.clone(),
            label.raw_input.clone(),
        )
    };
    let started_at = shared
        .tool_started
        .entry(tool_call_id.clone())
        .or_insert_with(Utc::now)
        .to_owned();
    let completed_at = finished.then(Utc::now);
    if is_mutating(&kind, &name) {
        let mut paths: Vec<String> = locations
            .iter()
            .map(|location| location.path.clone())
            .collect();
        for path in call.paths() {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        for path in paths {
            if shared.touched.len() < TOUCHED_PATH_LIMIT && !shared.touched.contains(&path) {
                shared.touched.push(path);
            }
        }
    }
    if finished {
        shared.tool_labels.remove(&tool_call_id);
    }
    shared.state = tool_state(&kind, &status);
    HarnessEvent::ToolCall {
        tool_call: ToolCall {
            tool_call_id,
            name,
            status,
            summary,
            input,
            output,
            started_at,
            completed_at,
            stats: call.stats(),
        },
    }
}

fn is_mutating(kind: &AcpToolKind, name: &str) -> bool {
    if matches!(kind, AcpToolKind::Edit) {
        return true;
    }
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "write_to_file"
            | "replace_file_content"
            | "multi_replace_file_content"
            | "notebook_edit"
            | "sed_file"
            | "apply_patch"
            | "edit"
            | "write"
    )
}

fn tool_name(call: &AcpToolCall) -> Option<String> {
    call.raw
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .or_else(|| call.title.clone().filter(|title| !title.is_empty()))
}

fn tool_status(status: &AcpToolStatus) -> ToolCallStatus {
    match status {
        AcpToolStatus::Pending | AcpToolStatus::InProgress => ToolCallStatus::Running,
        AcpToolStatus::Completed => ToolCallStatus::Completed,
        AcpToolStatus::Failed => ToolCallStatus::Failed,
        AcpToolStatus::Cancelled => ToolCallStatus::Failed,
        AcpToolStatus::Other(_) => ToolCallStatus::Running,
    }
}

fn tool_state(kind: &AcpToolKind, status: &ToolCallStatus) -> AgentState {
    match status {
        ToolCallStatus::Running => match kind {
            AcpToolKind::Edit => AgentState::Editing,
            AcpToolKind::Read | AcpToolKind::Fetch => AgentState::Reading,
            AcpToolKind::Search => AgentState::Searching,
            AcpToolKind::Think => AgentState::Thinking,
            _ => AgentState::RunningTool,
        },
        ToolCallStatus::Completed => AgentState::Thinking,
        ToolCallStatus::Failed | ToolCallStatus::Denied => AgentState::Error,
    }
}

fn turn_state(status: TurnStatus, limited: bool) -> AgentState {
    if limited {
        return AgentState::RateLimited;
    }
    match status {
        TurnStatus::Completed => AgentState::Done,
        TurnStatus::Failed | TurnStatus::TokenLimit | TurnStatus::TurnLimit => AgentState::Error,
        TurnStatus::Interrupted => AgentState::Idle,
    }
}

fn usage_from_acp(usage: &AcpUsage, duration_ms: u64) -> Usage {
    Usage {
        // The h4rn counter includes cached input; ACP reports it separately.
        input_tokens: usage
            .input_tokens
            .saturating_add(usage.cached_read_tokens)
            .saturating_add(usage.cached_write_tokens),
        output_tokens: usage.output_tokens,
        cached_input_tokens: usage.cached_read_tokens,
        cost_micros: 0,
        duration_ms,
        ..Usage::default()
    }
}

fn build_prompt(
    text: &str,
    attachments: &[AttachmentRef],
    capabilities: &AgentCapabilities,
    harness: HarnessKind,
) -> Result<Vec<PromptContent>, HarnessError> {
    let mut content = Vec::new();
    let mut notes = Vec::new();
    let mut total_image_bytes = 0usize;

    for attachment in attachments {
        let name = attachment
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| attachment.id.to_string());
        let media_type = attachments::media_type(attachment);
        let class = AttachmentClass::of(&media_type, &attachment.path);

        if attachments::strategy(harness, class) == AttachmentDelivery::AcpImageBlock {
            if capabilities.image
                && let Ok(bytes) = std::fs::read(&attachment.path)
                && bytes.len() <= MAX_IMAGE_BYTES
                && total_image_bytes.saturating_add(bytes.len()) <= MAX_TOTAL_IMAGE_BYTES
            {
                total_image_bytes = total_image_bytes.saturating_add(bytes.len());
                content.push(PromptContent::image(
                    base64::engine::general_purpose::STANDARD.encode(&bytes),
                    media_type,
                ));
                continue;
            }
            if capabilities.image {
                notes.push(format!(
                    "[Attached image \"{name}\" could not be inlined; it is saved at: {}]",
                    attachment.path.display()
                ));
            } else {
                notes.push(attachments::path_line(attachment));
            }
            continue;
        }

        if capabilities.embedded_context
            && is_text_media(&media_type)
            && std::fs::metadata(&attachment.path)
                .map(|metadata| metadata.len() <= MAX_TEXT_RESOURCE_BYTES)
                .unwrap_or(false)
            && let Some(body) = attachments::read_text(&attachment.path)
        {
            content.push(PromptContent::resource_text(
                file_uri(&attachment.path),
                media_type,
                body,
            ));
            continue;
        }

        if capabilities.embedded_context {
            let size = std::fs::metadata(&attachment.path)
                .ok()
                .map(|metadata| metadata.len());
            content.push(PromptContent::resource_link(
                file_uri(&attachment.path),
                name,
                (!media_type.is_empty()).then_some(media_type),
                size,
            ));
        } else {
            notes.push(attachments::path_line(attachment));
        }
    }

    let mut full_text = text.to_string();
    if !notes.is_empty() {
        if !full_text.is_empty() {
            full_text.push('\n');
        }
        full_text.push_str(&notes.join("\n"));
    }
    if !full_text.trim().is_empty() {
        content.insert(0, PromptContent::text(full_text));
    }
    if content.is_empty() {
        content.push(PromptContent::text(String::new()));
    }
    Ok(content)
}

fn file_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

fn is_text_media(media_type: &str) -> bool {
    media_type.starts_with("text/")
        || matches!(
            media_type,
            "application/json"
                | "application/xml"
                | "application/javascript"
                | "application/x-javascript"
                | "application/toml"
                | "application/yaml"
                | "application/x-yaml"
        )
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    use super::*;
    use crate::acp::AcpClientConfig;
    use crate::acp::parse_session_update;
    use serde_json::json;

    pub(crate) fn policy() -> AcpSessionPolicy {
        AcpSessionPolicy {
            harness: HarnessKind::Acp,
            label: "test-agent".into(),
            failure_label: "test-agent".into(),
            permission_prompt: "The agent is requesting permission",
            detect_limit: |_, _| None,
            mode_for: default_mode,
            missing_mode_is_error: |_| false,
            default_models: &["default"],
            model_list_label: "the agent offers",
            case_sensitive_default_models: false,
            fresh_without_load: true,
            config_options_first: false,
        }
    }

    pub(crate) fn acp_pair(
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

    pub(crate) fn acp_spec(workspace: &Path) -> SessionSpec {
        SessionSpec {
            workspace: workspace.to_path_buf(),
            model: "default".into(),
            runtime_mode: RuntimeMode::ApprovalRequired,
            reasoning_effort: None,
            fast: false,
            resume_id: None,
            mcp_endpoint: None,
            env_overrides: std::collections::BTreeMap::new(),
            attachments: Vec::new(),
            agent_id: Some("test".into()),
        }
    }

    pub(crate) async fn next_frame(reader: &mut BufReader<tokio::io::DuplexStream>) -> Value {
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("read a frame");
        serde_json::from_str(line.trim()).expect("valid json")
    }

    pub(crate) async fn send_frame(writer: &mut tokio::io::DuplexStream, frame: Value) {
        let mut bytes = serde_json::to_vec(&frame).expect("encode");
        bytes.push(b'\n');
        writer.write_all(&bytes).await.expect("write");
        writer.flush().await.expect("flush");
    }

    pub(crate) async fn handshake(
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

    async fn next_turn_status(events: &mut BoxStream<'static, HarnessEvent>) -> TurnStatus {
        while let Some(event) = events.next().await {
            if let HarnessEvent::TurnCompleted { status, .. } = event {
                return status;
            }
        }
        panic!("the event stream ended before the turn completed");
    }

    #[test]
    fn approval_decisions_choose_the_matching_option() {
        let options = vec![
            AcpPermissionOption {
                option_id: "allow".into(),
                name: "Allow".into(),
                kind: "allow_once".into(),
            },
            AcpPermissionOption {
                option_id: "deny".into(),
                name: "Deny".into(),
                kind: "reject_once".into(),
            },
        ];
        assert_eq!(
            select_permission_option(&options, ApprovalDecision::Approve, None).as_deref(),
            Some("allow")
        );
        assert_eq!(
            select_permission_option(&options, ApprovalDecision::Deny, None).as_deref(),
            Some("deny")
        );
        assert_eq!(
            select_permission_option(&options, ApprovalDecision::Cancel, None),
            None
        );
    }

    #[test]
    fn an_unknown_model_error_names_the_agents_own_catalogue() {
        let policy = policy();
        let session = AcpSessionInfo::from_value(
            &json!({"models": {"availableModels": [{"modelId": "m-1", "name": "M"}]}}),
            "s-1",
        );
        assert_eq!(
            policy.unknown_model("nope", &session),
            "model `nope`; the agent offers: m-1"
        );
    }

    #[test]
    fn prompt_building_embeds_small_text_attachments() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let path = workspace.path().join("notes.md");
        std::fs::write(&path, "# Notes\n").expect("write");
        let capabilities = AgentCapabilities {
            embedded_context: true,
            ..AgentCapabilities::default()
        };
        let content = build_prompt(
            "read this",
            &[AttachmentRef {
                id: "blob-1".to_string().into(),
                path: path.clone(),
                media_type: "text/markdown".into(),
            }],
            &capabilities,
            HarnessKind::Acp,
        )
        .expect("prompt");
        assert!(matches!(&content[0], PromptContent::Text(text) if text == "read this"));
        assert!(matches!(
            &content[1],
            PromptContent::Resource { text: Some(body), .. } if body == "# Notes\n"
        ));
    }

    #[test]
    fn tool_updates_track_paths_and_status() {
        let shared = Arc::new(Mutex::new(AcpShared::new()));
        let call = AcpToolCall::from_value(&json!({
            "toolCallId": "call-1",
            "name": "replace_file_content",
            "kind": "edit",
            "status": "in_progress",
            "title": "Edit src/lib.rs",
            "locations": [{"path": "/w/src/lib.rs", "line": 4}],
            "rawInput": {"path": "/w/src/lib.rs"}
        }));
        let event = tool_event(&call, &shared);
        match event {
            HarnessEvent::ToolCall { tool_call } => {
                assert_eq!(tool_call.name, "replace_file_content");
                assert_eq!(tool_call.status, ToolCallStatus::Running);
                assert_eq!(tool_call.summary, "Edit src/lib.rs");
                assert_eq!(tool_call.completed_at, None);
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
        assert_eq!(lock(&shared).touched, vec!["/w/src/lib.rs"]);
        assert_eq!(lock(&shared).state, AgentState::Editing);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_rejected_send_does_not_clear_a_running_turns_stop() {
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
                json!({"jsonrpc": "2.0", "id": prompt["id"], "result": {"stopReason": "end_turn"}}),
            )
            .await;
        });

        let mut session = AcpSession::start(
            &client,
            &acp_spec(workspace.path()),
            policy(),
            workspace.path(),
        )
        .await
        .expect("start session")
        .into_session();
        let mut events = session.events();
        session
            .send(UserInput {
                text: "first".into(),
                attachments: Vec::new(),
            })
            .await
            .expect("send");
        session.interrupt().await.expect("stop");

        // A second prompt while one runs is rejected; that must not consume the
        // stop the running turn is waiting for.
        let rejected = session
            .send(UserInput {
                text: "second".into(),
                attachments: Vec::new(),
            })
            .await;
        assert!(rejected.is_err(), "a concurrent send is refused");

        assert_eq!(
            next_turn_status(&mut events).await,
            TurnStatus::Interrupted,
            "the running turn still honours the stop it was sent"
        );
        agent.await.expect("agent");
        session.shutdown().await.expect("shutdown");
    }

    #[test]
    fn prompt_building_embeds_utf16_text_attachments() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let path = workspace.path().join("notes.txt");
        let mut encoded = vec![0xFF, 0xFE];
        for unit in "Über notes\n".encode_utf16() {
            encoded.extend_from_slice(&unit.to_le_bytes());
        }
        std::fs::write(&path, encoded).expect("write");
        let capabilities = AgentCapabilities {
            embedded_context: true,
            ..AgentCapabilities::default()
        };
        let content = build_prompt(
            "",
            &[attachment(&path, "text/plain")],
            &capabilities,
            HarnessKind::Antigravity,
        )
        .expect("prompt");
        assert!(
            matches!(
                &content[0],
                PromptContent::Resource { text: Some(body), .. } if body == "Über notes\n"
            ),
            "the UTF-16 body must be embedded, got {content:?}"
        );
    }

    #[test]
    fn a_plan_update_becomes_a_plan_event() {
        let shared = Arc::new(Mutex::new(AcpShared::new()));
        let update = AcpUpdate::Plan {
            entries: vec![
                crate::acp::PlanEntry {
                    content: "Read the failing test".into(),
                    priority: "high".into(),
                    status: "completed".into(),
                },
                crate::acp::PlanEntry {
                    content: "Fix the race".into(),
                    priority: "medium".into(),
                    status: "in_progress".into(),
                },
            ],
        };
        let events = map_update(&update, &shared);
        match events.as_slice() {
            [HarnessEvent::Plan { entries }] => {
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[1].content, "Fix the race");
                assert_eq!(entries[1].status, "in_progress");
                assert_eq!(entries[1].priority, "medium");
            }
            other => panic!("expected one plan event, got {other:?}"),
        }
        assert_eq!(lock(&shared).state, AgentState::Thinking);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_restart_without_load_session_starts_fresh_and_reports_the_new_id() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (client, mut reader, mut writer) = acp_pair(workspace.path());

        let agent = tokio::spawn(async move {
            let initialize = next_frame(&mut reader).await;
            assert_eq!(initialize["method"], "initialize");
            // No `agentCapabilities.loadSession`: this agent cannot resume.
            send_frame(
                &mut writer,
                json!({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocolVersion": 1}}),
            )
            .await;
            let new_session = next_frame(&mut reader).await;
            assert_eq!(
                new_session["method"], "session/new",
                "a stored id the agent cannot load must start over, not fail"
            );
            send_frame(
                &mut writer,
                json!({"jsonrpc": "2.0", "id": new_session["id"], "result": {"sessionId": "s-new"}}),
            )
            .await;
        });

        let mut spec = acp_spec(workspace.path());
        spec.resume_id = Some("s-old".into());
        let mut session = AcpSession::start(&client, &spec, policy(), workspace.path())
            .await
            .expect("start session")
            .into_session();
        let mut events = session.events();
        match events.next().await.expect("session started") {
            HarnessEvent::SessionStarted { session_id } => {
                assert_eq!(
                    session_id, "s-new",
                    "the fresh session id replaces the stored one"
                );
            }
            other => panic!("unexpected first event {other:?}"),
        }
        drop(events);
        agent.await.expect("agent");
        session.shutdown().await.expect("shutdown");
    }

    /// An agent whose stdin is gone is a definite pre-write failure: the
    /// prompt frame never reached it, so the daemon can put the message back.
    #[tokio::test(flavor = "current_thread")]
    async fn a_closed_writer_is_a_definite_pre_write_failure() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (client, mut reader, mut writer) = acp_pair(workspace.path());
        let agent = tokio::spawn(async move {
            handshake(&mut reader, &mut writer).await;
        });

        let spec = acp_spec(workspace.path());
        let mut session = AcpSession::start(&client, &spec, policy(), workspace.path())
            .await
            .expect("start session")
            .into_session();
        let mut events = session.events();
        match events.next().await.expect("session started") {
            HarnessEvent::SessionStarted { .. } => {}
            other => panic!("unexpected first event {other:?}"),
        }
        // The agent task owns both ends; it drops them when the handshake is
        // done, closing the client's writer.
        agent.await.expect("agent");

        let error = session
            .send(UserInput {
                text: "hello".into(),
                attachments: Vec::new(),
            })
            .await
            .expect_err("the writer is gone");
        assert!(
            matches!(error, HarnessError::NotSent { .. }),
            "a frame the writer never accepted is not an uncertain transport failure: {error:?}"
        );
        session.shutdown().await.expect("shutdown");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_resume_only_agent_continues_with_session_resume() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (client, mut reader, mut writer) = acp_pair(workspace.path());

        let agent = tokio::spawn(async move {
            let initialize = next_frame(&mut reader).await;
            assert_eq!(initialize["method"], "initialize");
            // `resume` without `load`: the agent can continue a conversation
            // but cannot replay one.
            send_frame(
                &mut writer,
                json!({
                    "jsonrpc": "2.0",
                    "id": initialize["id"],
                    "result": {
                        "protocolVersion": 1,
                        "agentCapabilities": {"sessionCapabilities": {"resume": {}}}
                    }
                }),
            )
            .await;
            let resume = next_frame(&mut reader).await;
            assert_eq!(
                resume["method"], "session/resume",
                "the method the agent advertises, not session/load"
            );
            assert_eq!(resume["params"]["sessionId"], "s-old");
            send_frame(
                &mut writer,
                json!({"jsonrpc": "2.0", "id": resume["id"], "result": {"sessionId": "s-old"}}),
            )
            .await;
        });

        let mut spec = acp_spec(workspace.path());
        spec.resume_id = Some("s-old".into());
        let mut session = AcpSession::start(&client, &spec, policy(), workspace.path())
            .await
            .expect("start session")
            .into_session();
        let mut events = session.events();
        match events.next().await.expect("session started") {
            HarnessEvent::SessionStarted { session_id } => assert_eq!(session_id, "s-old"),
            other => panic!("unexpected first event {other:?}"),
        }
        drop(events);
        agent.await.expect("agent");
        session.shutdown().await.expect("shutdown");
    }

    /// A resumed ACP session keeps the model the agent restored. The controls
    /// must report that model so the daemon can show what the session really
    /// runs instead of the `default` alias the thread asked for.
    #[tokio::test(flavor = "current_thread")]
    async fn a_resumed_session_reports_the_model_the_agent_restored() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (client, mut reader, mut writer) = acp_pair(workspace.path());

        let agent = tokio::spawn(async move {
            let initialize = next_frame(&mut reader).await;
            assert_eq!(initialize["method"], "initialize");
            send_frame(
                &mut writer,
                json!({
                    "jsonrpc": "2.0",
                    "id": initialize["id"],
                    "result": {
                        "protocolVersion": 1,
                        "agentCapabilities": {"loadSession": true}
                    }
                }),
            )
            .await;
            let load = next_frame(&mut reader).await;
            assert_eq!(load["method"], "session/load");
            assert_eq!(load["params"]["sessionId"], "s-old");
            // The conversation was on `m-restored`; the reply names it as the
            // current selection along with the catalogue.
            send_frame(
                &mut writer,
                json!({
                    "jsonrpc": "2.0",
                    "id": load["id"],
                    "result": {
                        "sessionId": "s-old",
                        "models": {
                            "availableModels": [
                                {"modelId": "m-default", "name": "Default"},
                                {"modelId": "m-restored", "name": "Restored"}
                            ],
                            "currentModelId": "m-restored"
                        }
                    }
                }),
            )
            .await;
        });

        let mut spec = acp_spec(workspace.path());
        spec.model = "default".into();
        spec.resume_id = Some("s-old".into());
        let mut session = AcpSession::start(&client, &spec, policy(), workspace.path())
            .await
            .expect("start the resumed session")
            .into_session();
        let mut events = session.events();
        match events.next().await.expect("session started") {
            HarnessEvent::SessionStarted { session_id } => assert_eq!(session_id, "s-old"),
            other => panic!("unexpected first event {other:?}"),
        }
        drop(events);
        agent.await.expect("agent");

        let controls = session.controls().expect("the agent named a catalogue");
        assert_eq!(
            controls.current_model.as_deref(),
            Some("m-restored"),
            "the session says which model it is really running"
        );
        session.shutdown().await.expect("shutdown");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_generic_agent_prefers_its_config_options_catalogue() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (client, mut reader, mut writer) = acp_pair(workspace.path());

        let agent = tokio::spawn(async move {
            let initialize = next_frame(&mut reader).await;
            send_frame(
                &mut writer,
                json!({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocolVersion": 1}}),
            )
            .await;
            let new_session = next_frame(&mut reader).await;
            assert_eq!(new_session["method"], "session/new");
            // Both catalogues, disagreeing: the legacy one lists "legacy", the
            // v1 config option lists "v1-model".
            send_frame(
                &mut writer,
                json!({
                    "jsonrpc": "2.0",
                    "id": new_session["id"],
                    "result": {
                        "sessionId": "s-1",
                        "models": {
                            "availableModels": [{"modelId": "legacy", "name": "Legacy"}],
                            "currentModelId": "legacy"
                        },
                        "configOptions": [{
                            "id": "model",
                            "name": "Model",
                            "category": "model",
                            "type": "select",
                            "currentValue": "legacy",
                            "options": [
                                {"value": "legacy", "name": "Legacy"},
                                {"value": "v1-model", "name": "V1"}
                            ]
                        }]
                    }
                }),
            )
            .await;

            let model = next_frame(&mut reader).await;
            assert_eq!(
                model["method"], "session/set_config_option",
                "the catalogue the agent offered through configOptions is selected with the config RPC"
            );
            assert_eq!(model["params"]["configId"], "model");
            assert_eq!(model["params"]["value"], "v1-model");
            send_frame(
                &mut writer,
                json!({"jsonrpc": "2.0", "id": model["id"], "result": {}}),
            )
            .await;
        });

        let mut spec = acp_spec(workspace.path());
        spec.model = "v1-model".into();
        let policy = AcpSessionPolicy {
            config_options_first: true,
            ..policy()
        };
        let mut session = AcpSession::start(&client, &spec, policy, workspace.path())
            .await
            .expect("start session")
            .into_session();
        agent.await.expect("agent");
        session.shutdown().await.expect("shutdown");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn config_options_select_models_and_modes_with_set_config_option() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (client, mut reader, mut writer) = acp_pair(workspace.path());

        let agent = tokio::spawn(async move {
            let initialize = next_frame(&mut reader).await;
            send_frame(
                &mut writer,
                json!({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocolVersion": 1}}),
            )
            .await;
            let new_session = next_frame(&mut reader).await;
            assert_eq!(new_session["method"], "session/new");
            send_frame(
                &mut writer,
                json!({
                    "jsonrpc": "2.0",
                    "id": new_session["id"],
                    "result": {
                        "sessionId": "s-1",
                        "configOptions": [
                            {
                                "id": "model",
                                "name": "Model",
                                "category": "model",
                                "type": "select",
                                "currentValue": "m1",
                                "options": [
                                    {"value": "m1", "name": "One"},
                                    {"value": "m2", "name": "Two"}
                                ]
                            },
                            {
                                "id": "mode",
                                "name": "Mode",
                                "category": "mode",
                                "type": "select",
                                "currentValue": "other",
                                "options": [
                                    {"value": "default", "name": "Default"},
                                    {"value": "other", "name": "Other"}
                                ]
                            }
                        ]
                    }
                }),
            )
            .await;

            let model = next_frame(&mut reader).await;
            assert_eq!(model["method"], "session/set_config_option");
            assert_eq!(model["params"]["configId"], "model");
            assert_eq!(model["params"]["value"], "m2");
            send_frame(
                &mut writer,
                json!({"jsonrpc": "2.0", "id": model["id"], "result": {}}),
            )
            .await;

            let mode = next_frame(&mut reader).await;
            assert_eq!(mode["method"], "session/set_config_option");
            assert_eq!(mode["params"]["configId"], "mode");
            assert_eq!(mode["params"]["value"], "default");
            send_frame(
                &mut writer,
                json!({"jsonrpc": "2.0", "id": mode["id"], "result": {}}),
            )
            .await;
        });

        let mut spec = acp_spec(workspace.path());
        spec.model = "m2".into();
        let mut session = AcpSession::start(&client, &spec, policy(), workspace.path())
            .await
            .expect("start session")
            .into_session();
        agent.await.expect("agent");
        session.shutdown().await.expect("shutdown");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_start_time_model_selection_is_what_the_controls_report() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (client, mut reader, mut writer) = acp_pair(workspace.path());

        let agent = tokio::spawn(async move {
            let initialize = next_frame(&mut reader).await;
            send_frame(
                &mut writer,
                json!({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocolVersion": 1}}),
            )
            .await;
            let new_session = next_frame(&mut reader).await;
            assert_eq!(new_session["method"], "session/new");
            // The agent opened on `m-1`; the thread asked for `m-2`.
            send_frame(
                &mut writer,
                json!({
                    "jsonrpc": "2.0",
                    "id": new_session["id"],
                    "result": {
                        "sessionId": "s-1",
                        "models": {
                            "availableModels": [
                                {"modelId": "m-1", "name": "M one"},
                                {"modelId": "m-2", "name": "M two"}
                            ],
                            "currentModelId": "m-1"
                        }
                    }
                }),
            )
            .await;

            let model = next_frame(&mut reader).await;
            assert_eq!(model["method"], "session/set_model");
            assert_eq!(model["params"]["modelId"], "m-2");
            send_frame(
                &mut writer,
                json!({"jsonrpc": "2.0", "id": model["id"], "result": {}}),
            )
            .await;
        });

        let mut spec = acp_spec(workspace.path());
        spec.model = "m-2".into();
        let mut session = AcpSession::start(&client, &spec, policy(), workspace.path())
            .await
            .expect("start session")
            .into_session();
        agent.await.expect("agent");

        let controls = session.controls().expect("the agent named a catalogue");
        assert_eq!(
            controls.current_model.as_deref(),
            Some("m-2"),
            "the controls name the model the start selected, not the one the agent opened on"
        );
        session.shutdown().await.expect("shutdown");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn adapter_mode_ids_are_mapped_at_start_and_in_place() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (client, mut reader, mut writer) = acp_pair(workspace.path());

        let agent = tokio::spawn(async move {
            let initialize = next_frame(&mut reader).await;
            send_frame(
                &mut writer,
                json!({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocolVersion": 1}}),
            )
            .await;
            let new_session = next_frame(&mut reader).await;
            assert_eq!(new_session["method"], "session/new");
            // The agent names its own ids, not h4rn's `accept-edits`/`auto`.
            send_frame(
                &mut writer,
                json!({
                    "jsonrpc": "2.0",
                    "id": new_session["id"],
                    "result": {
                        "sessionId": "s-1",
                        "modes": {
                            "currentModeId": "architect",
                            "availableModes": [
                                {"id": "architect", "name": "Architect"},
                                {"id": "acceptEdits", "name": "Accept edits"},
                                {"id": "bypassPermissions", "name": "Bypass"}
                            ]
                        }
                    }
                }),
            )
            .await;

            let start = next_frame(&mut reader).await;
            assert_eq!(
                start["method"], "session/set_mode",
                "a session created in full access asks for the agent's own id"
            );
            assert_eq!(start["params"]["sessionId"], "s-1");
            assert_eq!(start["params"]["modeId"], "bypassPermissions");
            send_frame(
                &mut writer,
                json!({"jsonrpc": "2.0", "id": start["id"], "result": {}}),
            )
            .await;

            let switch = next_frame(&mut reader).await;
            assert_eq!(switch["method"], "session/set_mode");
            assert_eq!(switch["params"]["sessionId"], "s-1");
            assert_eq!(switch["params"]["modeId"], "acceptEdits");
            send_frame(
                &mut writer,
                json!({"jsonrpc": "2.0", "id": switch["id"], "result": {}}),
            )
            .await;
        });

        let mut spec = acp_spec(workspace.path());
        spec.runtime_mode = RuntimeMode::FullAccess;
        let mut session = AcpSession::start(&client, &spec, policy(), workspace.path())
            .await
            .expect("start session")
            .into_session();

        let modes = session
            .controls()
            .expect("the agent advertised its modes")
            .modes;
        assert!(
            modes.contains(&RuntimeMode::AutoAcceptEdits)
                && modes.contains(&RuntimeMode::FullAccess),
            "adapter ids map onto h4rn's modes: {modes:?}"
        );
        session
            .set_runtime_mode(RuntimeMode::AutoAcceptEdits)
            .await
            .expect("switch in place through the mapped id");
        agent.await.expect("agent");
        session.shutdown().await.expect("shutdown");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_advertised_mode_that_maps_to_none_is_refused_at_start() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (client, mut reader, mut writer) = acp_pair(workspace.path());

        let agent = tokio::spawn(async move {
            let initialize = next_frame(&mut reader).await;
            send_frame(
                &mut writer,
                json!({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocolVersion": 1}}),
            )
            .await;
            let new_session = next_frame(&mut reader).await;
            send_frame(
                &mut writer,
                json!({
                    "jsonrpc": "2.0",
                    "id": new_session["id"],
                    "result": {
                        "sessionId": "s-1",
                        "modes": {
                            "currentModeId": "code",
                            "availableModes": [
                                {"id": "code", "name": "Code"},
                                {"id": "architect", "name": "Architect"}
                            ]
                        }
                    }
                }),
            )
            .await;
        });

        let spec = acp_spec(workspace.path());
        let error = AcpSession::start(&client, &spec, policy(), workspace.path())
            .await
            .err()
            .expect("no advertised mode is h4rn's approval mode");
        agent.await.expect("agent");
        match error {
            HarnessError::Unsupported { feature, .. } => {
                assert!(
                    feature.contains("code"),
                    "the refusal names the catalogue: {feature}"
                );
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_non_default_mode_is_refused_when_the_agent_advertised_no_modes() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (client, mut reader, mut writer) = acp_pair(workspace.path());

        let agent = tokio::spawn(async move {
            handshake(&mut reader, &mut writer).await;
        });

        let mut spec = acp_spec(workspace.path());
        spec.runtime_mode = RuntimeMode::FullAccess;
        let error = AcpSession::start(&client, &spec, policy(), workspace.path())
            .await
            .err()
            .expect("an empty catalogue cannot map a non-default mode");
        agent.await.expect("agent");
        match error {
            HarnessError::Unsupported { feature, .. } => {
                assert!(
                    feature.contains("auto") && feature.contains("no mode catalogue"),
                    "the refusal explains the missing catalogue: {feature}"
                );
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_default_mode_still_starts_without_a_mode_catalogue() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (client, mut reader, mut writer) = acp_pair(workspace.path());

        let agent = tokio::spawn(async move {
            handshake(&mut reader, &mut writer).await;
        });

        let mut session = AcpSession::start(
            &client,
            &acp_spec(workspace.path()),
            policy(),
            workspace.path(),
        )
        .await
        .expect("approval required asks for no mode change")
        .into_session();
        agent.await.expect("agent");
        session.shutdown().await.expect("shutdown");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_default_model_alias_is_deferred_on_a_live_session() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (client, mut reader, mut writer) = acp_pair(workspace.path());

        let agent = tokio::spawn(async move {
            let initialize = next_frame(&mut reader).await;
            send_frame(
                &mut writer,
                json!({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocolVersion": 1}}),
            )
            .await;
            let new_session = next_frame(&mut reader).await;
            send_frame(
                &mut writer,
                json!({
                    "jsonrpc": "2.0",
                    "id": new_session["id"],
                    "result": {
                        "sessionId": "s-1",
                        "models": {
                            "availableModels": [
                                {"modelId": "m-1", "name": "M one"},
                                {"modelId": "m-2", "name": "M two"}
                            ],
                            "currentModelId": "m-1"
                        }
                    }
                }),
            )
            .await;
        });

        let mut spec = acp_spec(workspace.path());
        spec.model = "m-1".into();
        let mut session = AcpSession::start(&client, &spec, policy(), workspace.path())
            .await
            .expect("start session")
            .into_session();
        agent.await.expect("agent");

        let error = session
            .set_model("default")
            .await
            .expect_err("a live session cannot reset to the agent's default");
        match error {
            HarnessError::Deferred { feature, .. } => assert!(
                feature.contains("default"),
                "the deferral names the value: {feature}"
            ),
            other => panic!("expected Deferred, got {other:?}"),
        }
        session.shutdown().await.expect("shutdown");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_effort_config_option_is_sent_at_start_and_in_place() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (client, mut reader, mut writer) = acp_pair(workspace.path());

        let agent = tokio::spawn(async move {
            let initialize = next_frame(&mut reader).await;
            send_frame(
                &mut writer,
                json!({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocolVersion": 1}}),
            )
            .await;
            let new_session = next_frame(&mut reader).await;
            assert_eq!(new_session["method"], "session/new");
            send_frame(
                &mut writer,
                json!({
                    "jsonrpc": "2.0",
                    "id": new_session["id"],
                    "result": {
                        "sessionId": "s-1",
                        "configOptions": [{
                            "id": "reasoning_effort",
                            "name": "Reasoning effort",
                            "category": "thought_level",
                            "type": "select",
                            "currentValue": "low",
                            "options": [
                                {"value": "low", "name": "Low"},
                                {"value": "high", "name": "High"}
                            ]
                        }]
                    }
                }),
            )
            .await;

            let start = next_frame(&mut reader).await;
            assert_eq!(start["method"], "session/set_config_option");
            assert_eq!(start["params"]["sessionId"], "s-1");
            assert_eq!(start["params"]["configId"], "reasoning_effort");
            assert_eq!(start["params"]["value"], "high");
            send_frame(
                &mut writer,
                json!({"jsonrpc": "2.0", "id": start["id"], "result": {}}),
            )
            .await;

            let switch = next_frame(&mut reader).await;
            assert_eq!(switch["method"], "session/set_config_option");
            assert_eq!(switch["params"]["configId"], "reasoning_effort");
            assert_eq!(switch["params"]["value"], "low");
            send_frame(
                &mut writer,
                json!({"jsonrpc": "2.0", "id": switch["id"], "result": {}}),
            )
            .await;
        });

        let mut spec = acp_spec(workspace.path());
        spec.reasoning_effort = Some("high".into());
        let mut session = AcpSession::start(&client, &spec, policy(), workspace.path())
            .await
            .expect("start session")
            .into_session();
        session
            .set_reasoning_effort(Some("low"))
            .await
            .expect("switch effort in place");
        agent.await.expect("agent");
        session.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn a_deliberate_teardown_does_not_publish_a_second_exit_error() {
        let (tx, rx) = mpsc::channel(8);
        let (out_tx, mut out_rx) = mpsc::channel(8);
        let bridge = spawn_event_bridge(
            rx,
            out_tx,
            Arc::new(Mutex::new(AcpShared::new())),
            Arc::new(AtomicBool::new(false)),
            policy(),
        );

        // The client killed the agent itself after a write deadline; the failure
        // that caused it is already on the stream.
        tx.send(AcpEvent::Exited {
            status: "exited: 9".into(),
            stderr: String::new(),
            deliberate: true,
        })
        .await
        .expect("send");
        tx.send(AcpEvent::Error {
            message: "the prompt timed out".into(),
            retryable: false,
        })
        .await
        .expect("send");
        match out_rx.recv().await.expect("event") {
            HarnessEvent::Error { message, .. } => {
                assert_eq!(message, "the prompt timed out");
            }
            other => panic!("unexpected event {other:?}"),
        }

        // A crash the client did not cause still reports the exit.
        tx.send(AcpEvent::Exited {
            status: "exited: 1".into(),
            stderr: "boom".into(),
            deliberate: false,
        })
        .await
        .expect("send");
        match out_rx.recv().await.expect("event") {
            HarnessEvent::TransportClosed { message, .. } => {
                assert!(
                    message.contains("test-agent exited (exited: 1)"),
                    "unexpected exit error: {message}"
                );
            }
            other => panic!("unexpected event {other:?}"),
        }
        bridge.abort();
    }

    #[tokio::test]
    async fn an_exit_report_with_a_reset_still_reports_the_death() {
        fn limit(text: &str, _: DateTime<Utc>) -> Option<LimitSignal> {
            text.contains("rate limit").then(|| LimitSignal {
                reset_at: Some(Utc::now()),
                message: text.to_string(),
            })
        }
        let policy = AcpSessionPolicy {
            detect_limit: limit,
            ..policy()
        };
        let (tx, rx) = mpsc::channel(8);
        let (out_tx, mut out_rx) = mpsc::channel(8);
        let bridge = spawn_event_bridge(
            rx,
            out_tx,
            Arc::new(Mutex::new(AcpShared::new())),
            Arc::new(AtomicBool::new(false)),
            policy,
        );

        tx.send(AcpEvent::Exited {
            status: "exited: 7".into(),
            stderr: "rate limit reached".into(),
            deliberate: false,
        })
        .await
        .expect("send");

        assert!(
            matches!(
                out_rx.recv().await.expect("limit"),
                HarnessEvent::LimitHit {
                    reset_at: Some(_),
                    auto_resume: true,
                }
            ),
            "the exit report's reset reaches the schedule"
        );
        match out_rx.recv().await.expect("exit") {
            HarnessEvent::TransportClosed { kind, reset_at, .. } => {
                assert_eq!(kind, TransportCloseKind::Exit);
                assert!(
                    reset_at.is_some(),
                    "the exit report carries the reset, so a consumer that kept it has it"
                );
            }
            other => panic!("the exit report is still the death: {other:?}"),
        }
        bridge.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_stop_after_end_turn_does_not_interrupt_the_next_turn() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (client, mut reader, mut writer) = acp_pair(workspace.path());

        let agent = tokio::spawn(async move {
            handshake(&mut reader, &mut writer).await;
            for _ in 0..2 {
                let prompt = next_frame(&mut reader).await;
                assert_eq!(prompt["method"], "session/prompt");
                send_frame(
                    &mut writer,
                    json!({"jsonrpc": "2.0", "id": prompt["id"], "result": {"stopReason": "end_turn"}}),
                )
                .await;
            }
        });

        let mut session = AcpSession::start(
            &client,
            &acp_spec(workspace.path()),
            policy(),
            workspace.path(),
        )
        .await
        .expect("start session")
        .into_session();
        let mut events = session.events();
        session
            .send(UserInput {
                text: "first".into(),
                attachments: Vec::new(),
            })
            .await
            .expect("send");
        assert_eq!(next_turn_status(&mut events).await, TurnStatus::Completed);

        // The agent ended its turn; the daemon has not consumed the completion
        // yet, so a Stop still arrives while the thread looks busy.
        session.interrupt().await.expect("stop");

        session
            .send(UserInput {
                text: "second".into(),
                attachments: Vec::new(),
            })
            .await
            .expect("send");
        assert_eq!(
            next_turn_status(&mut events).await,
            TurnStatus::Completed,
            "a late Stop must not mark the next turn interrupted"
        );

        agent.await.expect("agent");
        session.shutdown().await.expect("shutdown");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_missing_stop_reason_fails_the_turn() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (client, mut reader, mut writer) = acp_pair(workspace.path());

        let agent = tokio::spawn(async move {
            handshake(&mut reader, &mut writer).await;
            let prompt = next_frame(&mut reader).await;
            assert_eq!(prompt["method"], "session/prompt");
            send_frame(
                &mut writer,
                json!({"jsonrpc": "2.0", "id": prompt["id"], "result": {}}),
            )
            .await;
        });

        let mut session = AcpSession::start(
            &client,
            &acp_spec(workspace.path()),
            policy(),
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

        let (status, errors) = tokio::time::timeout(Duration::from_secs(5), async {
            let mut errors = Vec::new();
            loop {
                match events.next().await.expect("event") {
                    HarnessEvent::Error { message, .. } => errors.push(message),
                    HarnessEvent::TurnCompleted { status, .. } => return (status, errors),
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for the turn");
        assert_eq!(status, TurnStatus::Failed);
        assert!(
            errors
                .iter()
                .any(|message| message == "the agent ended the turn without a stop reason"),
            "the failure names the missing stop reason: {errors:?}"
        );
        assert_eq!(
            session.last_turn().expect("a turn report").stop_reason,
            None
        );

        agent.await.expect("agent");
        session.shutdown().await.expect("shutdown");
    }

    /// A turn's last update reaches its turn, not the one after it.
    ///
    /// The prompt task publishes the turn's settlement and the bridge forwards
    /// the turn's updates on the same channel, from two tasks. Without the
    /// reader's order the settlement could overtake an update the reader had
    /// already taken, and the text the agent wrote before its reply would land
    /// after the completion.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_turns_last_update_is_never_delivered_after_its_completion() {
        for run in 0..100 {
            let workspace = tempfile::tempdir().expect("tempdir");
            let (client, mut reader, mut writer) = acp_pair(workspace.path());
            let agent = tokio::spawn(async move {
                handshake(&mut reader, &mut writer).await;
                let prompt = next_frame(&mut reader).await;
                assert_eq!(prompt["method"], "session/prompt");
                // The last words arrive, then the reply: exactly the order a
                // real peer writes them in.
                send_frame(
                    &mut writer,
                    json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {
                            "sessionId": "s-1",
                            "update": {
                                "sessionUpdate": "agent_message_chunk",
                                "content": {"type": "text", "text": "last words"}
                            }
                        }
                    }),
                )
                .await;
                send_frame(
                    &mut writer,
                    json!({
                        "jsonrpc": "2.0",
                        "id": prompt["id"],
                        "result": {"stopReason": "end_turn"}
                    }),
                )
                .await;
            });

            let mut session = AcpSession::start(
                &client,
                &acp_spec(workspace.path()),
                policy(),
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

            let mut text = String::new();
            let mut completed = false;
            while let Some(event) = events.next().await {
                match event {
                    HarnessEvent::TextDelta { text: delta } => {
                        assert!(
                            !completed,
                            "run {run}: the turn's text arrived after its completion"
                        );
                        text.push_str(&delta);
                    }
                    HarnessEvent::TurnCompleted { status, .. } => {
                        assert_eq!(status, TurnStatus::Completed, "run {run}");
                        completed = true;
                        break;
                    }
                    _ => {}
                }
            }
            assert!(completed, "run {run}: the turn never completed");
            assert_eq!(text, "last words", "run {run}");

            agent.await.expect("agent");
            session.shutdown().await.expect("shutdown");
        }
    }

    /// An update that arrives after its turn settled has no turn to land on.
    ///
    /// The bridge forwards a turn's updates and its settlement on one channel.
    /// Once the settlement is delivered, a `session/update` still coming from
    /// the agent belongs to the turn that already ended; applying it would put
    /// the finished turn's words on the next one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_update_after_a_turns_settlement_is_not_applied_to_the_next_turn() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (client, mut reader, mut writer) = acp_pair(workspace.path());
        let (settled_tx, settled_rx) = tokio::sync::oneshot::channel::<()>();
        let agent = tokio::spawn(async move {
            handshake(&mut reader, &mut writer).await;
            let prompt = next_frame(&mut reader).await;
            send_frame(
                &mut writer,
                json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": "s-1",
                        "update": {
                            "sessionUpdate": "agent_message_chunk",
                            "content": {"type": "text", "text": "first turn"}
                        }
                    }
                }),
            )
            .await;
            send_frame(
                &mut writer,
                json!({"jsonrpc": "2.0", "id": prompt["id"], "result": {"stopReason": "end_turn"}}),
            )
            .await;
            // The consumer has now read the settlement: everything the agent
            // writes from here belongs to no turn.
            let _ = settled_rx.await;
            send_frame(
                &mut writer,
                json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": "s-1",
                        "update": {
                            "sessionUpdate": "agent_message_chunk",
                            "content": {"type": "text", "text": "late words"}
                        }
                    }
                }),
            )
            .await;
            // Answer the second prompt with no text of its own.
            let second = next_frame(&mut reader).await;
            send_frame(
                &mut writer,
                json!({"jsonrpc": "2.0", "id": second["id"], "result": {"stopReason": "end_turn"}}),
            )
            .await;
        });

        let mut session = AcpSession::start(
            &client,
            &acp_spec(workspace.path()),
            policy(),
            workspace.path(),
        )
        .await
        .expect("start session")
        .into_session();
        let mut events = session.events();
        session
            .send(UserInput {
                text: "first".into(),
                attachments: Vec::new(),
            })
            .await
            .expect("send");

        let mut first_text = String::new();
        let mut settled = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while let Ok(Some(event)) = tokio::time::timeout_at(deadline, events.next()).await {
            match event {
                HarnessEvent::TextDelta { text } => first_text.push_str(&text),
                HarnessEvent::TurnCompleted { status, .. } => {
                    assert_eq!(status, TurnStatus::Completed);
                    settled = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(settled, "the first turn settled");
        assert_eq!(first_text, "first turn");

        // Everything the agent writes now is dropped before it reaches the
        // stream: wait for a moment that would deliver it if it were kept.
        let _ = settled_tx.send(());
        let quiet = tokio::time::timeout(Duration::from_millis(200), async {
            while let Some(event) = events.next().await {
                if let HarnessEvent::TextDelta { text } = event {
                    return Some(text);
                }
            }
            None
        })
        .await;
        assert!(
            !matches!(quiet, Ok(Some(_))),
            "the finished turn's late update was delivered: {quiet:?}"
        );

        // The next turn starts clean.
        session
            .send(UserInput {
                text: "second".into(),
                attachments: Vec::new(),
            })
            .await
            .expect("send");
        let mut second_text = String::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while let Ok(Some(event)) = tokio::time::timeout_at(deadline, events.next()).await {
            match event {
                HarnessEvent::TextDelta { text } => second_text.push_str(&text),
                HarnessEvent::TurnCompleted { status, .. } => {
                    assert_eq!(status, TurnStatus::Completed);
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(second_text, "", "the next turn has no text of its own");

        agent.await.expect("agent");
        session.shutdown().await.expect("shutdown");
    }

    /// An update tagged with a settled prompt never reaches the turn that
    /// replaced it, even when it is read after the next prompt started.
    ///
    /// ACP carries no turn id on `session/update`, so the bridge tags every
    /// update with the prompt generation the reader saw when it read the frame.
    /// An update whose tag is not the running prompt's belongs to the turn that
    /// already ended: applying it would put the finished turn's words on the new
    /// prompt (and keep a promoted turn from going back to the queue).
    #[tokio::test]
    async fn an_update_from_a_settled_prompt_is_dropped_after_the_next_prompt_starts() {
        let (tx, rx) = mpsc::channel(8);
        let (out_tx, mut out_rx) = mpsc::channel(8);
        let bridge = spawn_event_bridge(
            rx,
            out_tx,
            Arc::new(Mutex::new(AcpShared::new())),
            Arc::new(AtomicBool::new(false)),
            policy(),
        );
        let update = |text: &str, generation: u64| AcpEvent::SessionUpdate {
            generation,
            session_id: "s-1".into(),
            update: parse_session_update(&json!({
                "sessionId": "s-1",
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": text}
                }
            }))
            .expect("update")
            .1,
        };

        // Turn 1's update and settlement, then turn 2 starts.
        tx.send(AcpEvent::PromptStarted { generation: 1 })
            .await
            .expect("send");
        tx.send(update("first turn", 1)).await.expect("send");
        tx.send(AcpEvent::Settled {
            generation: 1,
            events: vec![HarnessEvent::TurnCompleted {
                status: TurnStatus::Completed,
                usage: Usage::default(),
            }],
        })
        .await
        .expect("send");
        tx.send(AcpEvent::PromptStarted { generation: 2 })
            .await
            .expect("send");
        // A stale frame read after turn 2 started still carries turn 1's tag.
        tx.send(update("late words", 1)).await.expect("send");
        tx.send(update("second turn", 2)).await.expect("send");
        tx.send(AcpEvent::Settled {
            generation: 2,
            events: vec![HarnessEvent::TurnCompleted {
                status: TurnStatus::Completed,
                usage: Usage::default(),
            }],
        })
        .await
        .expect("send");

        let mut text = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = out_rx.recv().await {
                match event {
                    HarnessEvent::TextDelta { text: delta } => text.push(delta),
                    HarnessEvent::TurnCompleted { .. } => {}
                    other => panic!("unexpected event {other:?}"),
                }
                if text.len() == 2 {
                    break;
                }
            }
        })
        .await
        .expect("the bridge published both turns");
        assert_eq!(
            text,
            vec!["first turn".to_string(), "second turn".to_string()],
            "the settled prompt's late update was applied to the next turn"
        );
        bridge.abort();
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

        let mut session = AcpSession::start(
            &client,
            &acp_spec(workspace.path()),
            policy(),
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
        assert_eq!(
            session
                .last_turn()
                .expect("a turn report")
                .stop_reason
                .as_deref(),
            Some("max_tokens")
        );

        agent.await.expect("agent");
        session.shutdown().await.expect("shutdown");
    }

    fn attachment(path: &Path, media_type: &str) -> AttachmentRef {
        AttachmentRef {
            id: "blob-1".to_string().into(),
            path: path.to_path_buf(),
            media_type: media_type.to_string(),
        }
    }

    #[test]
    fn images_are_inlined_only_when_capable() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let path = workspace.path().join("pixel.png");
        std::fs::write(&path, [0x89, b'P', b'N', b'G']).expect("write");
        let content = build_prompt(
            "",
            &[attachment(&path, "image/png")],
            &AgentCapabilities::default(),
            HarnessKind::Acp,
        )
        .expect("prompt");
        assert!(matches!(
            &content[0],
            PromptContent::Text(text) if text.contains("saved at")
        ));

        let content = build_prompt(
            "",
            &[attachment(&path, "image/png")],
            &AgentCapabilities {
                image: true,
                ..AgentCapabilities::default()
            },
            HarnessKind::Acp,
        )
        .expect("prompt");
        assert!(matches!(&content[0], PromptContent::Image { .. }));
    }

    #[test]
    fn usage_includes_cached_input() {
        let usage = AcpUsage {
            input_tokens: 10,
            output_tokens: 2,
            cached_read_tokens: 3,
            cached_write_tokens: 4,
            thought_tokens: 1,
            total_tokens: 19,
        };
        let mapped = usage_from_acp(&usage, 1200);
        assert_eq!(mapped.input_tokens, 17);
        assert_eq!(mapped.output_tokens, 2);
        assert_eq!(mapped.cached_input_tokens, 3);
        assert_eq!(mapped.duration_ms, 1200);
    }

    #[test]
    fn a_status_only_completion_keeps_the_tool_input() {
        let shared = Arc::new(Mutex::new(AcpShared::new()));
        let started = AcpToolCall::from_value(&json!({
            "toolCallId": "call-3",
            "name": "read_file",
            "kind": "read",
            "status": "in_progress",
            "title": "Read package.json",
            "rawInput": {"path": "/w/package.json"},
            "locations": [{"path": "/w/package.json"}]
        }));
        let HarnessEvent::ToolCall { tool_call } = tool_event(&started, &shared) else {
            panic!("expected a tool call");
        };
        assert_eq!(tool_call.input, Some(json!({"path": "/w/package.json"})));

        let update = AcpToolCall::from_value(&json!({
            "toolCallId": "call-3",
            "status": "completed"
        }));
        let HarnessEvent::ToolCall { tool_call } = tool_event(&update, &shared) else {
            panic!("expected a tool call");
        };
        assert_eq!(
            tool_call.input,
            Some(json!({"path": "/w/package.json"})),
            "a status-only update must not drop the input the row expands into"
        );
    }

    #[test]
    fn a_title_only_call_takes_a_later_title_as_its_name() {
        let shared = Arc::new(Mutex::new(AcpShared::new()));
        let started = AcpToolCall::from_value(&json!({
            "toolCallId": "call-4",
            "status": "in_progress",
            "title": "Read package.json"
        }));
        let HarnessEvent::ToolCall { tool_call } = tool_event(&started, &shared) else {
            panic!("expected a tool call");
        };
        assert_eq!(tool_call.name, "Read package.json");

        let update = AcpToolCall::from_value(&json!({
            "toolCallId": "call-4",
            "status": "in_progress",
            "title": "Read apps/ui/package.json"
        }));
        let HarnessEvent::ToolCall { tool_call } = tool_event(&update, &shared) else {
            panic!("expected a tool call");
        };
        assert_eq!(
            tool_call.name, "Read apps/ui/package.json",
            "a title-only call has no other name, so the newer title becomes it"
        );
    }

    #[test]
    fn a_status_only_update_keeps_a_kind_only_tools_name() {
        let shared = Arc::new(Mutex::new(AcpShared::new()));
        let started = AcpToolCall::from_value(&json!({
            "toolCallId": "call-2",
            "kind": "read",
            "status": "in_progress"
        }));
        let HarnessEvent::ToolCall { tool_call } = tool_event(&started, &shared) else {
            panic!("expected a tool call");
        };
        assert_eq!(tool_call.name, "Read");

        let update = AcpToolCall::from_value(&json!({
            "toolCallId": "call-2",
            "status": "completed"
        }));
        let HarnessEvent::ToolCall { tool_call } = tool_event(&update, &shared) else {
            panic!("expected a tool call");
        };
        assert_eq!(
            tool_call.name, "Read",
            "a status-only update must not rename the call to other"
        );
        assert_eq!(tool_call.summary, "");
    }

    #[test]
    fn a_later_title_updates_the_summary_but_keeps_the_tool_name() {
        let shared = Arc::new(Mutex::new(AcpShared::new()));
        let started = AcpToolCall::from_value(&json!({
            "toolCallId": "call-1",
            "name": "read_file",
            "kind": "read",
            "status": "in_progress",
            "title": "Read package.json",
            "locations": [{"path": "/w/package.json"}]
        }));
        let HarnessEvent::ToolCall { tool_call } = tool_event(&started, &shared) else {
            panic!("expected a tool call");
        };
        assert_eq!(tool_call.name, "read_file");

        let update = AcpToolCall::from_value(&json!({
            "toolCallId": "call-1",
            "status": "in_progress",
            "title": "Read apps/ui/package.json"
        }));
        let HarnessEvent::ToolCall { tool_call } = tool_event(&update, &shared) else {
            panic!("expected a tool call");
        };
        assert_eq!(
            tool_call.name, "read_file",
            "the title is not the tool name"
        );
        assert_eq!(tool_call.summary, "Read apps/ui/package.json");
    }

    #[tokio::test]
    async fn a_failed_stdout_read_is_not_reported_as_an_eof() {
        let (tx, rx) = mpsc::channel(8);
        let (out_tx, mut out_rx) = mpsc::channel(8);
        let bridge = spawn_event_bridge(
            rx,
            out_tx,
            Arc::new(Mutex::new(AcpShared::new())),
            Arc::new(AtomicBool::new(false)),
            policy(),
        );

        tx.send(AcpEvent::Closed {
            message: "the ACP agent stdout closed".into(),
            kind: TransportCloseKind::Read,
        })
        .await
        .expect("send");

        match out_rx.recv().await.expect("event") {
            HarnessEvent::TransportClosed { kind, .. } => assert_eq!(
                kind,
                TransportCloseKind::Read,
                "the bridge keeps the read error the reader observed"
            ),
            other => panic!("unexpected event {other:?}"),
        }
        bridge.abort();
    }

    #[test]
    fn titleless_tool_updates_keep_the_earlier_name_and_summary() {
        let shared = Arc::new(Mutex::new(AcpShared::new()));
        let started = AcpToolCall::from_value(&json!({
            "toolCallId": "call-1",
            "name": "read_file",
            "kind": "read",
            "status": "in_progress",
            "title": "Read package.json",
            "locations": [{"path": "/w/package.json"}]
        }));
        let HarnessEvent::ToolCall { tool_call } = tool_event(&started, &shared) else {
            panic!("expected a tool call");
        };
        assert_eq!(tool_call.name, "read_file");
        assert_eq!(tool_call.summary, "Read package.json");

        let update = AcpToolCall::from_value(&json!({
            "toolCallId": "call-1",
            "kind": "read",
            "status": "completed",
            "rawOutput": "{\"name\":\"h4rn\"}"
        }));
        let HarnessEvent::ToolCall { tool_call } = tool_event(&update, &shared) else {
            panic!("expected a tool call");
        };
        assert_eq!(tool_call.name, "read_file");
        assert_eq!(tool_call.summary, "Read package.json");
        assert!(tool_call.output.is_some());
        assert!(lock(&shared).tool_labels.is_empty(), "the id is forgotten");
    }

    #[test]
    fn advertised_modes_map_to_h4rn_permissions() {
        assert_eq!(
            runtime_mode_for_id("default"),
            Some(RuntimeMode::ApprovalRequired)
        );
        assert_eq!(
            runtime_mode_for_id("accept-edits"),
            Some(RuntimeMode::AutoAcceptEdits)
        );
        assert_eq!(runtime_mode_for_id("plan"), Some(RuntimeMode::Plan));
        assert_eq!(runtime_mode_for_id("auto"), Some(RuntimeMode::FullAccess));
        assert_eq!(runtime_mode_for_id("agent-specific-mode"), None);

        let modes = vec![AcpMode {
            id: "plan".into(),
            name: "Plan".into(),
            description: None,
        }];
        assert_eq!(
            advertised_mode_for(RuntimeMode::Plan, &modes).as_deref(),
            Some("plan")
        );
        assert_eq!(advertised_mode_for(RuntimeMode::FullAccess, &modes), None);
    }
}
