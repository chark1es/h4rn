//! Client-to-daemon frames.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::common::{
    ApprovalDecision, DeviceInfo, HarnessKind, RuntimeMode, ThreadFilter, ThreadTab, Topic,
    WorktreeRequest,
};
use crate::ids::{AttachmentId, HostId, ThreadId};

/// One request sent by a client.
///
/// `id` is chosen by the client and echoed verbatim in
/// [`crate::ServerFrame::Response`]. Requests are idempotent where a retry is
/// possible, so a duplicate `id` returns the stored result rather than
/// executing twice (ADR-003).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct ClientFrame {
    /// Client-chosen request sequence number.
    #[ts(type = "number")]
    pub id: u64,
    /// The request body.
    pub body: ClientRequest,
}

/// Everything a client can ask a daemon to do.
///
/// Variants after `Subscribe` are deliberately skeletal: the daemon rejects
/// them with [`crate::ProtoErrorCode::Unsupported`] until their phase lands.
/// They exist now so the generated TypeScript barrel is stable and later
/// phases append rather than rename.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientRequest {
    /// Open the connection and authenticate a device.
    Hello {
        /// Wire protocol version the client speaks.
        protocol_version: u32,
        /// Identity of the connecting device.
        device: DeviceInfo,
    },
    /// List hosts reachable by this daemon.
    ListHosts,
    /// List threads, optionally filtered.
    ListThreads {
        /// Filter to apply.
        filter: ThreadFilter,
    },
    /// Fetch one thread and its event stream after `since_seq`.
    GetThread {
        /// Thread to fetch.
        thread_id: ThreadId,
        /// Last sequence number the client has already applied.
        #[ts(type = "number")]
        since_seq: u64,
    },
    /// Create a thread and enqueue its first turn.
    CreateThread {
        /// Host that runs the harness.
        host: HostId,
        /// Harness adapter to use.
        harness: HarnessKind,
        /// Absolute workspace path on the host.
        workspace_path: String,
        /// Model slug.
        model: String,
        /// First user prompt.
        prompt: String,
        /// Attachments uploaded before this request.
        attachments: Vec<AttachmentId>,
        /// Permission autonomy for the whole thread.
        #[serde(default)]
        runtime_mode: RuntimeMode,
        /// Run in an isolated git worktree, optionally naming the branch.
        #[serde(default)]
        worktree: Option<WorktreeRequest>,
    },
    /// Append a user message to an existing thread.
    SendMessage {
        /// Thread to append to.
        thread_id: ThreadId,
        /// Message text.
        text: String,
        /// Attachments uploaded before this request.
        attachments: Vec<AttachmentId>,
    },
    /// Interrupt the running turn.
    Interrupt {
        /// Thread whose turn should stop.
        thread_id: ThreadId,
    },
    /// Answer a harness approval request.
    RespondApproval {
        /// Thread raising the hand.
        thread_id: ThreadId,
        /// Opaque approval id from the event stream.
        approval_id: String,
        /// User decision.
        decision: ApprovalDecision,
        /// Selected [`crate::ApprovalOption`] id, when answering a question.
        #[serde(default)]
        option_id: Option<String>,
    },
    /// Move a thread by hand. The override sticks until the next turn starts.
    SetThreadTab {
        /// Thread to move.
        thread_id: ThreadId,
        /// Destination tab.
        tab: ThreadTab,
        /// Keep an idle thread in Working.
        pinned: bool,
    },
    /// Snooze a thread until `until`; `None` wakes it immediately.
    Snooze {
        /// Thread to snooze.
        thread_id: ThreadId,
        /// Wake time.
        until: Option<DateTime<Utc>>,
    },
    /// Subscribe to event topics after `Hello`.
    Subscribe {
        /// Topics to subscribe to.
        topics: Vec<Topic>,
        /// Replay daemon-global events after this sequence before going live.
        ///
        /// `None` starts at the live edge. The daemon reads the journal while
        /// holding its append lock, so the replay is a gap-free prefix of the
        /// stream that follows.
        #[serde(default)]
        #[ts(type = "number | null")]
        since_seq: Option<u64>,
    },
    // ------------------------------------------------------------------
    // TODO(phase 2+): terminal, filesystem, MCP, skills, proxy, usage and
    // forge requests. Their payloads are intentionally minimal placeholders.
    // ------------------------------------------------------------------
    /// TODO(phase 2): open a PTY attached to a thread.
    TerminalOpen {
        /// Owning thread.
        thread_id: ThreadId,
        /// Initial columns.
        cols: u16,
        /// Initial rows.
        rows: u16,
    },
    /// TODO(phase 2): write user keystrokes to a PTY.
    TerminalInput {
        /// Terminal stream id.
        terminal_id: String,
        /// Raw input bytes, base64-encoded until binary framing lands.
        data: String,
    },
    /// TODO(phase 2): resize a PTY.
    TerminalResize {
        /// Terminal stream id.
        terminal_id: String,
        /// New columns.
        cols: u16,
        /// New rows.
        rows: u16,
    },
    /// TODO(phase 2): close a PTY.
    TerminalClose {
        /// Terminal stream id.
        terminal_id: String,
    },
    /// TODO(phase 3): list a directory on a host.
    FsList {
        /// Host that owns the filesystem.
        host: HostId,
        /// Absolute directory path.
        path: String,
    },
    /// TODO(phase 3): read a file from a host.
    FsRead {
        /// Host that owns the filesystem.
        host: HostId,
        /// Absolute file path.
        path: String,
    },
    /// TODO(phase 3): write a file on a host.
    FsWrite {
        /// Host that owns the filesystem.
        host: HostId,
        /// Absolute file path.
        path: String,
        /// Full replacement contents.
        contents: String,
    },
    /// TODO(phase 4): list MCP servers.
    McpList,
    /// TODO(phase 4): add or update an MCP server.
    McpUpsert {
        /// Server name.
        name: String,
        /// Transport (`http`, `sse`, `stdio`).
        transport: String,
        /// Endpoint URL for HTTP/SSE transports.
        url: Option<String>,
    },
    /// TODO(phase 4): remove an MCP server.
    McpRemove {
        /// Server name.
        name: String,
    },
    /// TODO(phase 5): list skills on a host.
    SkillsList {
        /// Host to inspect.
        host: HostId,
    },
    /// TODO(phase 5): enable or disable a skill.
    SkillsSetEnabled {
        /// Host that owns the skill.
        host: HostId,
        /// Skill id.
        skill_id: String,
        /// Whether the skill is enabled.
        enabled: bool,
    },
    /// TODO(phase 6): probe a CLIProxyAPI-compatible endpoint (ADR-015).
    ProxyProbe {
        /// Base URL to probe.
        base_url: String,
    },
    /// TODO(phase 6): configure proxy routing for one harness.
    ProxyConfigure {
        /// Harness to route.
        harness: HarnessKind,
        /// Proxy base URL, or `None` to go direct.
        base_url: Option<String>,
        /// Whether the proxy is enabled for this harness.
        enabled: bool,
    },
    /// TODO(phase 6): query the usage ledger.
    UsageQuery {
        /// Optional lower bound.
        since: Option<DateTime<Utc>>,
        /// Optional host filter.
        host: Option<HostId>,
    },
    /// TODO(phase 7): list pull requests for a repository.
    ForgeListPrs {
        /// Host that owns the forge credentials.
        host: HostId,
        /// `owner/name`.
        repository: String,
    },
    /// TODO(phase 7): open a pull request.
    ForgeCreatePr {
        /// Host that owns the forge credentials.
        host: HostId,
        /// `owner/name`.
        repository: String,
        /// PR title.
        title: String,
        /// PR body.
        body: String,
        /// Source branch.
        head: String,
        /// Target branch.
        base: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::DeviceInfo;
    use crate::ids::DeviceId;

    #[test]
    fn hello_serialises_with_an_internal_type_tag() {
        let frame = ClientFrame {
            id: 7,
            body: ClientRequest::Hello {
                protocol_version: crate::PROTOCOL_VERSION,
                device: DeviceInfo {
                    device_id: DeviceId::new("dev_1"),
                    name: "test".into(),
                    platform: "linux".into(),
                    app_version: "0.1.0".into(),
                },
            },
        };
        let value = serde_json::to_value(&frame).unwrap();
        assert_eq!(value["id"], 7);
        assert_eq!(value["body"]["type"], "hello");
        assert_eq!(value["body"]["protocol_version"], 1);
    }

    #[test]
    fn request_round_trips() {
        let frame = ClientFrame {
            id: 1,
            body: ClientRequest::SetThreadTab {
                thread_id: ThreadId::new("th_1"),
                tab: ThreadTab::Working,
                pinned: true,
            },
        };
        let json = serde_json::to_string(&frame).unwrap();
        let back: ClientFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(frame, back);
        assert!(json.contains("\"type\":\"set_thread_tab\""));
    }
}
