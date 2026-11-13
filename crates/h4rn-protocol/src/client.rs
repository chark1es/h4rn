//! Client-to-daemon frames.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::blob::BlobRange;
use crate::common::{
    ApprovalDecision, DeviceInfo, HarnessKind, RuntimeMode, ThreadFilter, ThreadTab, Topic,
    WorktreeRequest,
};
use crate::ids::{AttachmentId, HostId, TerminalId, ThreadId};

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
    /// Start a chunked attachment upload (ADR-008).
    ///
    /// The daemon answers with a [`crate::UploadTicket`]; bytes follow as
    /// binary `blob_chunk` frames on its stream id, and
    /// [`Self::CommitUpload`] makes the blob addressable.
    BeginUpload {
        /// Original file name; the daemon sanitizes it before storing.
        name: String,
        /// Declared size in bytes; the daemon rejects a larger blob.
        #[ts(type = "number")]
        size: u64,
        /// Client's best guess at the media type; the daemon sniffs the bytes.
        #[serde(default)]
        mime_hint: Option<String>,
    },
    /// Verify and finish an upload.
    ///
    /// `blake3` is the 64-character lowercase hex BLAKE3 digest of the whole
    /// file. A mismatch keeps the upload open so the client can resume or
    /// abort; a matching digest returns the content address and dedupes
    /// against an existing blob.
    CommitUpload {
        /// Upload opened by [`Self::BeginUpload`].
        upload_id: String,
        /// Declared BLAKE3 digest.
        blake3: String,
    },
    /// Discard an upload and its partial bytes.
    AbortUpload {
        /// Upload opened by [`Self::BeginUpload`].
        upload_id: String,
    },
    /// Ask how many bytes an upload has accepted, so it can resume.
    QueryUpload {
        /// Upload opened by [`Self::BeginUpload`].
        upload_id: String,
    },
    /// Stream a committed blob back as binary `blob_chunk` frames.
    GetBlob {
        /// Content address from a [`crate::Attachment`].
        id: AttachmentId,
        /// Byte range to read; `None` reads the whole blob.
        #[serde(default)]
        range: Option<BlobRange>,
        /// Read the generated 256 px thumbnail instead of the original.
        #[serde(default)]
        thumbnail: bool,
    },
    // ------------------------------------------------------------------
    // TODO(phase 2+): terminal, filesystem, MCP, skills, proxy, usage and
    // forge requests. Their payloads are intentionally minimal placeholders.
    // ------------------------------------------------------------------
    /// Open a PTY on `host` in `cwd`, optionally from a thread.
    ///
    /// `shell` defaults to the daemon user's `$SHELL` (or `%ComSpec%` /
    /// `pwsh` on Windows); `args` are passed to it verbatim.
    TerminalOpen {
        /// Owning thread, when opened from one.
        #[serde(default)]
        thread_id: Option<ThreadId>,
        /// Host that runs the PTY.
        host: HostId,
        /// Absolute working directory.
        cwd: String,
        /// Executable to spawn; `None` uses the host default shell.
        #[serde(default)]
        shell: Option<String>,
        /// Extra arguments for the shell executable.
        #[serde(default)]
        args: Vec<String>,
        /// Initial columns.
        cols: u16,
        /// Initial rows.
        rows: u16,
    },
    /// Attach a viewer, returning a snapshot and the sequence it replaces.
    TerminalAttach {
        /// Terminal to attach.
        terminal_id: TerminalId,
        /// Viewer width, when known; participates in smallest-viewer sizing.
        #[serde(default)]
        cols: Option<u16>,
        /// Viewer height, when known.
        #[serde(default)]
        rows: Option<u16>,
    },
    /// List terminals owned by this daemon.
    TerminalList {
        /// Filter to one thread, or every terminal when `None`.
        #[serde(default)]
        thread_id: Option<ThreadId>,
    },
    /// Resize one viewer's viewport. The effective size is the smallest viewer.
    TerminalResize {
        /// Terminal to resize.
        terminal_id: TerminalId,
        /// New columns for this viewer.
        cols: u16,
        /// New rows for this viewer.
        rows: u16,
    },
    /// Acknowledge output through `up_to_chunk`, opening credit for more.
    TerminalAck {
        /// Terminal whose output is acknowledged.
        terminal_id: TerminalId,
        /// Highest contiguous chunk sequence the client applied.
        #[ts(type = "number")]
        up_to_chunk: u64,
    },
    /// Close a terminal and kill its process group.
    TerminalClose {
        /// Terminal to close.
        terminal_id: TerminalId,
    },
    /// List one directory lazily, respecting `.gitignore`.
    ///
    /// `workspace` is a canonical workspace root the daemon has authorised;
    /// `path` is relative to it and may be empty for the root.
    FsList {
        /// Host that owns the filesystem.
        host: HostId,
        /// Canonical workspace root.
        workspace: String,
        /// Workspace-relative directory path.
        path: String,
    },
    /// Read one file as text, with encoding detection and an etag.
    FsRead {
        /// Host that owns the filesystem.
        host: HostId,
        /// Canonical workspace root.
        workspace: String,
        /// Workspace-relative file path.
        path: String,
    },
    /// Write one file, refusing to overwrite a version the client did not see.
    FsWrite {
        /// Host that owns the filesystem.
        host: HostId,
        /// Canonical workspace root.
        workspace: String,
        /// Workspace-relative file path.
        path: String,
        /// Full replacement content.
        content: String,
        /// Etag the edit was based on; `None` writes only when the file is new.
        base_etag: Option<String>,
    },
    /// Create an empty file or a directory.
    FsCreate {
        /// Host that owns the filesystem.
        host: HostId,
        /// Canonical workspace root.
        workspace: String,
        /// Workspace-relative path to create.
        path: String,
        /// File or directory.
        kind: crate::fs::FsCreateKind,
    },
    /// Rename or move a path inside the workspace.
    FsRename {
        /// Host that owns the filesystem.
        host: HostId,
        /// Canonical workspace root.
        workspace: String,
        /// Current workspace-relative path.
        from: String,
        /// New workspace-relative path.
        to: String,
    },
    /// Move a path into the daemon's trash directory.
    FsDelete {
        /// Host that owns the filesystem.
        host: HostId,
        /// Canonical workspace root.
        workspace: String,
        /// Workspace-relative path.
        path: String,
    },
    /// Restore a previously deleted path from the trash directory.
    FsRestore {
        /// Host that owns the filesystem.
        host: HostId,
        /// Canonical workspace root.
        workspace: String,
        /// Trash id returned by `FsDelete`.
        trash_id: String,
    },
    /// Search the workspace, streaming results as `fs_search_batch` events.
    FsSearch {
        /// Host that owns the filesystem.
        host: HostId,
        /// Canonical workspace root.
        workspace: String,
        /// Text or regular expression to find.
        query: String,
        /// Treat `query` as a regular expression.
        regex: bool,
        /// Match case exactly.
        case_sensitive: bool,
        /// Optional glob filter such as `*.ts` or `src/**`.
        glob: Option<String>,
        /// Result cap; the daemon clamps it.
        max_results: Option<u32>,
    },
    /// Subscribe this connection to filesystem changes in one workspace.
    ///
    /// An empty `paths` list watches the whole tree. Changes arrive as
    /// transient `fs_changed` events.
    FsWatch {
        /// Host that owns the filesystem.
        host: HostId,
        /// Canonical workspace root.
        workspace: String,
        /// Workspace-relative files or directories to report.
        paths: Vec<String>,
    },
    /// Stop sending filesystem changes for this connection.
    FsUnwatch {
        /// Canonical workspace root.
        workspace: String,
    },
    /// Compare one file against `HEAD` or one agent turn's pre-turn snapshot.
    FsDiff {
        /// Host that owns the filesystem.
        host: HostId,
        /// Canonical workspace root.
        workspace: String,
        /// Workspace-relative file path.
        path: String,
        /// Which scope to compare against.
        against: crate::fs::DiffAgainst,
    },
    /// List the files one agent turn touched, with line counts.
    FsTurnChanges {
        /// Host that owns the filesystem.
        host: HostId,
        /// Canonical workspace root.
        workspace: String,
        /// Thread that owns the turn.
        thread_id: ThreadId,
        /// Turn whose pre-turn snapshots are the base.
        turn_id: crate::ids::TurnId,
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
    /// List skills, the install matrix and per-harness directories.
    SkillsList {
        /// Host to inspect.
        host: HostId,
    },
    /// Add a skill from a local folder, a pinned git commit or a harness
    /// directory, then install it into the selected harnesses.
    SkillsAdd {
        /// Host that owns the registry and the harness directories.
        host: HostId,
        /// Where to read the skill from.
        source: crate::skills::SkillSource,
        /// Harnesses to install into; empty adds to the registry only.
        #[serde(default)]
        harnesses: Vec<HarnessKind>,
        /// Override the frontmatter name.
        #[serde(default)]
        name: Option<String>,
    },
    /// Replace the current version from a source and reinstall managed copies.
    SkillsUpdate {
        /// Host that owns the registry.
        host: HostId,
        /// Skill to update.
        skill_id: String,
        /// Where to read the new version from.
        source: crate::skills::SkillSource,
    },
    /// Remove a skill and every h4rn-managed installed copy.
    SkillsRemove {
        /// Host that owns the registry.
        host: HostId,
        /// Skill to remove.
        skill_id: String,
    },
    /// Enable or disable a skill for one harness on this machine.
    SkillsSetEnabled {
        /// Host that owns the skill.
        host: HostId,
        /// Skill id.
        skill_id: String,
        /// Harness to toggle. Enabling installs the managed copy when missing.
        harness: HarnessKind,
        /// Whether the skill is enabled.
        enabled: bool,
    },
    /// Scan the harness directories and preview what an import would change.
    SkillsImportPreview {
        /// Host to scan.
        host: HostId,
    },
    /// Import the selected candidates from the last preview.
    SkillsImportCommit {
        /// Host that owns the registry.
        host: HostId,
        /// Candidate ids from [`ResponseBody::SkillsImportPreview`].
        candidate_ids: Vec<String>,
    },
    /// List unresolved skill conflicts with the ids `SkillsResolveConflict`
    /// expects.
    SkillsConflicts {
        /// Host that owns the registry.
        host: HostId,
    },
    /// Resolve one content conflict surfaced by an import or a sync.
    SkillsResolveConflict {
        /// Host that owns the registry.
        host: HostId,
        /// Conflict id.
        conflict_id: String,
        /// How to settle it.
        resolution: crate::skills::SkillConflictResolution,
    },
    /// Push local changes to connect and apply remote changes.
    SkillsSyncNow,
    /// Compare two versions of one skill.
    SkillsDiff {
        /// Host that owns the registry.
        host: HostId,
        /// Skill to compare.
        skill_id: String,
        /// Older version; `None` compares the current version against empty.
        #[serde(default)]
        from_version: Option<String>,
        /// Newer version; `None` uses the current version.
        #[serde(default)]
        to_version: Option<String>,
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
