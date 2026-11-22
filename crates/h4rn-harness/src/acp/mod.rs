//! A reusable Agent Client Protocol (ACP) client.
//!
//! ACP is JSON-RPC 2.0 over newline-delimited JSON on a child process's
//! stdio. This module is harness-agnostic: it negotiates capabilities, opens a
//! session, streams `session/update` notifications, answers
//! `session/request_permission` requests, and serves scoped `fs/*` requests.
//! The Antigravity adapter in [`crate::antigravity`] is one user; another ACP
//! agent can reuse the same client with a different spawn line.
//!
//! The wire shapes live in [`protocol`]; this module owns the transport,
//! request correlation, process supervision, and the filesystem scope. The
//! `AcpEvent` → [`crate::HarnessEvent`] mapping is owned by [`session`], the
//! runtime every adapter shares; an adapter only supplies its
//! [`AcpSessionPolicy`] (labels, its limit parser, model/mode preferences).
//!
//! # Process posture
//!
//! The agent is spawned into its own process group (ADR-021), so `interrupt`
//! and `shutdown` reach the tools it started. No shell is involved, secrets
//! travel through the environment only, and `kill` is the last resort.

mod client;
mod process;
mod protocol;
mod session;

pub use client::{AcpClient, AcpClientConfig, AcpError, AcpEvent, PendingPrompt};
pub use protocol::{
    ACP_PROTOCOL_VERSION, AcpAuthMethod, AcpCommand, AcpContentChunk, AcpDiff, AcpLocation,
    AcpMcpServer, AcpMode, AcpModel, AcpPermissionOption, AcpPermissionRequest, AcpPromptResult,
    AcpServerInfo, AcpSessionInfo, AcpToolCall, AcpToolKind, AcpToolStatus, AcpUpdate, AcpUsage,
    AgentCapabilities, ENV_MCP_STDIO_BRIDGE, JSONRPC_VERSION, PlanEntry, PromptContent,
    ToolCallContent, parse_session_update,
};
pub(crate) use session::{AcpSessionPolicy, StartedAcpSession, default_mode, map_acp_error};

/// Opens the shared ACP session runtime over a spawned client.
pub(crate) async fn session_start(
    client: &AcpClient,
    spec: &crate::SessionSpec,
    policy: AcpSessionPolicy,
    workspace: &std::path::Path,
) -> Result<StartedAcpSession, crate::HarnessError> {
    session::AcpSession::start(client, spec, policy, workspace).await
}

pub(crate) use process::{GroupSignal, StderrRing, signal_group, spawn_agent, spawn_stderr_reader};
