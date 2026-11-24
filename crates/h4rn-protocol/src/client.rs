//! Client-to-daemon frames.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::blob::BlobRange;
use crate::common::{
    ApprovalDecision, DeviceInfo, HarnessKind, RuntimeMode, ThreadFilter, ThreadTab, Topic,
    WorktreeRequest,
};
use crate::ids::{AttachmentId, DeviceId, HostId, TerminalId, ThreadId};

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
/// The daemon answers a request whose feature is not enabled on this host with
/// [`crate::ProtoErrorCode::Unsupported`] and a hint, and a request the wire
/// contract carries but this build does not implement with
/// [`crate::ProtoErrorCode::NotImplemented`].
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
        /// Configured ACP agent instance when `harness` is
        /// [`HarnessKind::Acp`]; ignored by built-in harnesses.
        #[serde(default)]
        agent_id: Option<String>,
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
        /// Reasoning effort in the harness's own vocabulary, when it has one.
        #[serde(default)]
        reasoning_effort: Option<String>,
        /// Ask for the harness's fast tier, where one exists (Codex).
        #[serde(default)]
        fast: bool,
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
    /// Change a thread's permission mode.
    ///
    /// Applies to a live session where the harness can switch in place, and
    /// always applies at the next session start. A mode the harness cannot
    /// honour is refused with [`crate::ProtoErrorCode::Unsupported`].
    SetThreadRuntimeMode {
        /// Thread to change.
        thread_id: ThreadId,
        /// New permission autonomy.
        mode: RuntimeMode,
    },
    /// Change a thread's model.
    ///
    /// Applies to a live session where the harness can switch in place, and
    /// always applies at the next session start. The conversation is kept
    /// through the harness's resume path when a session has to restart.
    SetThreadModel {
        /// Thread to change.
        thread_id: ThreadId,
        /// New model slug.
        model: String,
    },
    /// Change a thread's reasoning effort.
    ///
    /// `None` returns the thread to the harness's own default. Only a harness
    /// that exposes a real effort control accepts this; any other refuses with
    /// [`crate::ProtoErrorCode::Unsupported`].
    SetThreadReasoningEffort {
        /// Thread to change.
        thread_id: ThreadId,
        /// New effort value in the harness's vocabulary, or `None` for its
        /// default.
        effort: Option<String>,
    },
    /// Ask for (or stop asking for) a harness's fast tier.
    ///
    /// Only a harness with a real fast tier accepts this; any other refuses
    /// with [`crate::ProtoErrorCode::Unsupported`].
    SetThreadFast {
        /// Thread to change.
        thread_id: ThreadId,
        /// Whether to use the fast tier.
        fast: bool,
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
    // Terminal, filesystem, MCP, skills, proxy, usage and forge requests.
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
        /// Whether this client names the output position on every marked reply
        /// with protocol 2 numbering: the chunk where the query **starts**.
        ///
        /// The field is new rather than reused because the first version of
        /// positioned replies numbered answers by the chunk where the query
        /// *ended*. An older client sends no such field (or only
        /// `reply_positions`), so it is treated as legacy and its answers are
        /// never correlated with a position; an older daemon ignores the field
        /// and applies its own single-owner rule. While one such viewer is
        /// attached the terminal keeps that legacy rule.
        #[serde(default)]
        reply_positions_v2: bool,
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
        /// Also return the raw bytes as standard base64.
        ///
        /// The HTML preview uses this to inline relative images, fonts and
        /// other binary assets as `data:` URLs. Text reads leave it unset.
        #[serde(default)]
        bytes_base64: bool,
        /// Refuse a text read larger than this many bytes.
        ///
        /// The preview asks for a preview-sized cap so one decoded document or
        /// stylesheet fits a secure-transport frame. A larger file is not an
        /// error: it comes back with `content_omitted` and its size. Raw-byte
        /// reads keep their own frame-sized cap.
        #[serde(default)]
        #[ts(optional, type = "number")]
        max_bytes: Option<u64>,
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
    /// List the files a branch or commit range changed.
    FsDiffFiles {
        /// Host that owns the filesystem.
        host: HostId,
        /// Canonical workspace root.
        workspace: String,
        /// Scope to list; only `base` and `commit` are valid here.
        against: crate::fs::DiffAgainst,
    },
    /// List the newest commits in the branch comparison.
    FsDiffCommits {
        /// Host that owns the filesystem.
        host: HostId,
        /// Canonical workspace root.
        workspace: String,
        /// Branch to compare against, or `None` for the repository default.
        base: Option<String>,
    },
    /// List the branch names a diff can be compared against.
    FsBranches {
        /// Host that owns the filesystem.
        host: HostId,
        /// Canonical workspace root.
        workspace: String,
    },
    /// List every MCP server known to this daemon (phase 4).
    ///
    /// Kept as a distinct variant because it predates the general
    /// [`ClientRequest::Mcp`] envelope; it is equivalent to
    /// `Mcp { request: McpRequest::ListServers }`.
    McpList,
    /// Add or update an MCP server.
    McpUpsert {
        /// Full configuration, including any new secret values.
        input: crate::mcp::McpServerInput,
    },
    /// Remove an MCP server and its secrets.
    McpRemove {
        /// Server name.
        name: String,
    },
    /// One MCP hub operation (ADR-006).
    ///
    /// Every new operation rides this envelope so the MCP surface can grow
    /// without adding top-level request variants (and TypeScript
    /// discriminants) for each one.
    Mcp {
        /// Operation to perform.
        request: crate::mcp::McpRequest,
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
        /// Also take over each selected harness folder whose content matches
        /// the registry: h4rn writes its marker there (no skill file changes)
        /// and records the install, so later versions from sync replace it.
        #[serde(default)]
        manage: bool,
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
    // ------------------------------------------------------------------
    // Phase 5: CLIProxyAPI profiles (ADR-015).
    // ------------------------------------------------------------------
    /// List stored CLIProxyAPI profiles, without secrets.
    ListProxyProfiles,
    /// Add one profile and store its keys in the daemon secret store.
    AddProxyProfile {
        /// Profile fields and secrets.
        profile: crate::proxy::ProxyProfileDraft,
    },
    /// Update one profile. `None` fields are left unchanged.
    UpdateProxyProfile {
        /// Profile to update.
        profile_id: crate::proxy::ProxyProfileId,
        /// Field-wise patch.
        patch: crate::proxy::ProxyProfilePatch,
    },
    /// Remove one profile and its stored secrets.
    RemoveProxyProfile {
        /// Profile to remove.
        profile_id: crate::proxy::ProxyProfileId,
    },
    /// Probe one profile and return its capabilities and diagnostics.
    TestProxyProfile {
        /// Profile to probe.
        profile_id: crate::proxy::ProxyProfileId,
    },
    /// List the models one profile can serve, with provenance.
    ListProxyModels {
        /// Profile to read.
        profile_id: crate::proxy::ProxyProfileId,
    },
    /// Enable or disable routing one harness through one profile.
    SetProxyHarnessOverride {
        /// Profile whose toggle changes.
        profile_id: crate::proxy::ProxyProfileId,
        /// Harness affected.
        harness: HarnessKind,
        /// New state.
        enabled: bool,
        /// Model used when a thread did not pick a proxy model.
        default_model: Option<String>,
    },
    /// Make the saved harness overrides take effect now.
    ///
    /// Sessions already running keep the routing they started with, so the
    /// daemon interrupts any live turn of a routed harness and drops its
    /// session. The next message starts a fresh one with the new routing.
    ApplyProxyOverrides {
        /// Profile whose overrides were changed.
        profile_id: crate::proxy::ProxyProfileId,
    },
    /// Read credential health and cooldowns for one profile.
    ListProxyLimits {
        /// Profile to read.
        profile_id: crate::proxy::ProxyProfileId,
    },
    /// Pick a model (and account) to retry on after a limit.
    PickProxyFailover {
        /// Profile that will serve the retry.
        profile_id: crate::proxy::ProxyProfileId,
        /// Model that just hit its limit.
        current_model: String,
        /// Selection policy.
        policy: crate::proxy::ProxyFailoverPolicy,
    },
    /// Legacy usage query, kept for wire compatibility.
    ///
    /// Superseded by [`Self::GetUsage`]; kept so the wire contract does not
    /// rename a variant an older client may already know.
    UsageQuery {
        /// Optional lower bound.
        since: Option<DateTime<Utc>>,
        /// Optional host filter.
        host: Option<HostId>,
    },
    // ------------------------------------------------------------------
    // Phase 5: metering, limits and auto-resume (ADR-024).
    // ------------------------------------------------------------------
    /// Query the durable usage ledger.
    GetUsage {
        /// What to aggregate over.
        scope: crate::usage::UsageScope,
        /// Thread id or workspace path, required for those scopes.
        scope_id: Option<String>,
        /// Half-open time range.
        range: crate::usage::UsageRange,
        /// Breakdown to produce.
        group_by: crate::usage::UsageGroupBy,
        /// Optional hour/day series.
        bucket: Option<crate::usage::UsageBucket>,
        /// Optional grouping for the stacked series, when `bucket` is set.
        #[serde(default)]
        series_group_by: Option<crate::usage::UsageGroupBy>,
        /// Include subagent children in a thread-scoped report.
        ///
        /// With this set, `scope_id` names a parent thread and the report adds
        /// every child's turns; without it only the thread's own rows count.
        #[serde(default)]
        include_children: bool,
    },
    /// Devices that paired through a typed code and still await a human
    /// comparing the short authentication string with the new device's screen.
    PairingConfirmations,
    /// Record the host's decision on a pending pairing.
    ///
    /// Confirming admits the device key on the transport; rejecting removes and
    /// revokes it. Only a device holding `admin` may decide.
    ConfirmPairing {
        /// Connect device id from [`Self::PairingConfirmations`].
        device_id: DeviceId,
        /// `true` admits the device, `false` revokes it.
        confirm: bool,
    },
    /// Read the daemon's network-access setting and its dial addresses.
    ///
    /// Addresses are what another device on the same LAN or tailnet dials,
    /// for example `192.168.1.20:47810`; they are empty while the listener is
    /// off or failed to bind.
    GetNetworkAccess,
    /// Turn the dedicated network listener on or off.
    ///
    /// Enabling binds `<wildcard>:<network port>` (default `47810`) and serves
    /// only `/healthz` and the Noise routes `/secure` and `/secure-mcp`;
    /// plaintext `/ws` stays loopback-only. The setting is persisted in the
    /// daemon data directory and applies without a restart.
    SetNetworkAccess {
        /// Whether the listener should be running.
        enabled: bool,
    },
    /// Open a one-time direct (LAN/tailnet) pairing QR.
    ///
    /// Needs network access on and at least one non-loopback address. A new
    /// offer replaces any pairing that is already open, so the CLI and the UI
    /// never arm two secrets at once.
    CreateDirectPairing {
        /// Scopes the new device is granted; `view` and `approve` when empty.
        #[serde(default)]
        scopes: Vec<crate::connect::PairScope>,
        /// Label shown next to the QR; the host name when absent.
        #[serde(default)]
        label: Option<String>,
    },
    /// Close the open direct pairing and disarm its one-time secret.
    CancelDirectPairing,
    /// List devices paired directly with this computer (LAN/tailnet QR or
    /// `h4rnd pair --direct`). Devices paired through connect are not here.
    ListDirectDevices,
    /// Remove a directly paired device and drop its live sessions.
    RevokeDirectDevice {
        /// The device's Noise static key, hex, from [`Self::ListDirectDevices`].
        key: String,
    },
    /// Read known usage limits and the auto-resume policy.
    GetLimits,
    /// List persisted auto-resume schedules and limit history.
    ListResets,
    /// Cancel auto-resume for one thread.
    CancelResume {
        /// Thread whose schedule is cleared.
        thread_id: ThreadId,
    },
    /// Resume a scheduled thread immediately instead of waiting for its reset.
    ResumeNow {
        /// Thread to resume.
        thread_id: ThreadId,
    },
    /// List the models and accounts a limit-hit thread could fail over to.
    ///
    /// The daemon picks the default (`options.default`); the person may choose
    /// any entry of `options.options` for [`Self::FailOver`].
    ListFailoverOptions {
        /// Thread to inspect.
        thread_id: ThreadId,
    },
    /// Resume a limit-hit thread now on another model or account, instead of
    /// waiting for the reset.
    ///
    /// With no `model` the daemon's default is used. A named `model` (and
    /// optional `account`) must be one of the eligible options.
    FailOver {
        /// Thread to move.
        thread_id: ThreadId,
        /// Model to resume on, or `None` for the daemon's default.
        model: Option<String>,
        /// Account to prefer; only meaningful with `model`.
        account: Option<String>,
    },
    /// Replace the daemon-wide auto-resume policy.
    SetAutoResumePolicy {
        /// New policy.
        policy: crate::usage::AutoResumePolicy,
    },
    // ------------------------------------------------------------------
    // Scheduled tasks and webhook automations. The daemon owns the timer,
    // the journal and the webhook secret; clients read and edit tasks.
    // ------------------------------------------------------------------
    /// List every scheduled task on this host.
    ListSchedules,
    /// Create a scheduled task.
    CreateSchedule {
        /// The task to create.
        task: crate::ScheduleDraft,
    },
    /// Replace a scheduled task's editable fields.
    UpdateSchedule {
        /// Task to edit.
        task_id: crate::ScheduleTaskId,
        /// New fields.
        task: crate::ScheduleDraft,
    },
    /// Delete a scheduled task and its run history.
    DeleteSchedule {
        /// Task to delete.
        task_id: crate::ScheduleTaskId,
    },
    /// Enable or disable a scheduled task.
    SetScheduleEnabled {
        /// Task to change.
        task_id: crate::ScheduleTaskId,
        /// Whether the clock or webhook may start it.
        enabled: bool,
    },
    /// Start a scheduled task once, without changing its next clock run.
    RunScheduleNow {
        /// Task to run.
        task_id: crate::ScheduleTaskId,
    },
    /// Read a task's bounded run history, newest first.
    ListScheduleRuns {
        /// Task whose history to read.
        task_id: crate::ScheduleTaskId,
    },
    /// Replace a webhook task's URL path, invalidating the old one.
    ///
    /// The answer carries the new path, because a path is only ever revealed
    /// through this admin request or [`Self::GetScheduleWebhookUrl`].
    RotateScheduleWebhook {
        /// Task to rotate.
        task_id: crate::ScheduleTaskId,
    },
    /// Reveal a webhook task's secret URL path to an admin.
    GetScheduleWebhookUrl {
        /// Task whose path to read.
        task_id: crate::ScheduleTaskId,
    },
    // ------------------------------------------------------------------
    // Forge (pull request) operations. Credentials stay in the daemon's
    // secret store; every request names an account or lets the daemon pick
    // the only matching one.
    // ------------------------------------------------------------------
    /// List configured forge accounts and their capabilities.
    ForgeListAccounts,
    /// Add a forge account from a token the user just supplied.
    ///
    /// The daemon validates the token against the forge before storing it, so
    /// a typo is reported here rather than on the first PR fetch. `token` is
    /// never echoed back in a response, an event or an audit entry.
    ForgeAddAccount {
        /// Forge implementation.
        forge: crate::forge::ForgeKind,
        /// Public web base URL (`https://github.com`, `https://ghe.example.com`).
        base_url: String,
        /// Personal access token or OAuth access token.
        token: String,
        /// Login to record; the daemon discovers it when omitted.
        username: Option<String>,
        /// Where the token came from.
        source: crate::forge::ForgeCredentialSource,
        /// Repository the token was granted for, when the client already knows
        /// one (a detected workspace remote).
        ///
        /// Repository, project and workspace access tokens for Bitbucket Cloud
        /// cannot call `/user` or `/user/workspaces`, so the daemon validates
        /// them against this repository instead. `None` means no repository is
        /// known; such tokens then need a detected repository to be added.
        #[serde(default)]
        repository: Option<crate::forge::ForgeRepoRef>,
    },
    /// Remove a forge account and its stored token.
    ForgeRemoveAccount {
        /// Account to remove.
        account_id: String,
    },
    /// Start GitHub's OAuth device authorization grant.
    ///
    /// Only GitHub supports this. The daemon must have a client id configured
    /// (`H4RN_GITHUB_CLIENT_ID`); otherwise the request fails with a hint.
    ForgeDeviceFlowStart {
        /// Public base URL; only `https://github.com` supports the grant.
        base_url: Option<String>,
    },
    /// Poll one device authorization started by [`Self::ForgeDeviceFlowStart`].
    ForgeDeviceFlowPoll {
        /// Opaque flow id returned by the start request.
        flow_id: String,
    },
    /// Import a token the user already stored with `gh` or `glab`.
    ///
    /// The daemon only runs this after an explicit client action; nothing is
    /// imported at startup. The token is validated, stored, and never echoed.
    ForgeImportCli {
        /// Which CLI to read: GitHub (`gh`) or GitLab (`glab`).
        forge: crate::forge::ForgeKind,
        /// Self-hosted hostname the CLI knows about, when not the public one.
        base_url: Option<String>,
    },
    /// Detect the forge, owner and repository from a workspace's git remotes.
    ForgeDetectRepo {
        /// Absolute workspace path on this host.
        workspace: String,
    },
    /// List pull requests for one repository.
    ForgeListPrs {
        /// Account to use; the daemon picks the only matching one when omitted.
        account_id: Option<String>,
        /// Repository to query.
        repo: crate::forge::ForgeRepoRef,
        /// Which PRs to return.
        filter: crate::forge::ForgePrFilter,
        /// Page number, starting at 1.
        page: u32,
    },
    /// Fetch one pull request's detail.
    ForgeGetPr {
        /// Account to use.
        account_id: Option<String>,
        /// Repository.
        repo: crate::forge::ForgeRepoRef,
        /// PR number.
        number: u32,
    },
    /// Fetch one page of a pull request's diff.
    ForgeGetPrDiff {
        /// Account to use.
        account_id: Option<String>,
        /// Repository.
        repo: crate::forge::ForgeRepoRef,
        /// PR number.
        number: u32,
        /// Page number, starting at 1.
        page: u32,
        /// Files per page; the daemon clamps it.
        per_page: Option<u32>,
    },
    /// List general comments, inline review threads and reviews.
    ForgeListPrComments {
        /// Account to use.
        account_id: Option<String>,
        /// Repository.
        repo: crate::forge::ForgeRepoRef,
        /// PR number.
        number: u32,
    },
    /// List checks, jobs or commit statuses for the PR's head commit.
    ForgeListChecks {
        /// Account to use.
        account_id: Option<String>,
        /// Repository.
        repo: crate::forge::ForgeRepoRef,
        /// PR number.
        number: u32,
    },
    /// List a PR's commits, oldest first.
    ForgeListPrCommits {
        /// Account to use.
        account_id: Option<String>,
        /// Repository.
        repo: crate::forge::ForgeRepoRef,
        /// PR number.
        number: u32,
    },
    /// Re-run a PR's failed checks, then return the refreshed checks.
    ///
    /// Only where `ForgeChecks.rerun.supported` says so; elsewhere the daemon
    /// answers `unsupported` with the reason.
    ForgeRerunChecks {
        /// Account to use.
        account_id: Option<String>,
        /// Repository.
        repo: crate::forge::ForgeRepoRef,
        /// PR number.
        number: u32,
    },
    /// Reply in an existing review thread, or to a general comment.
    ///
    /// `thread_id` is the normalised thread/discussion id from
    /// `ForgeListPrComments`; forges without nested replies post a general
    /// comment instead and say so in their capabilities.
    ForgeReply {
        /// Account to use.
        account_id: Option<String>,
        /// Repository.
        repo: crate::forge::ForgeRepoRef,
        /// PR number.
        number: u32,
        /// Thread or discussion id, when replying to a review thread.
        thread_id: Option<String>,
        /// Comment being replied to.
        comment_id: String,
        /// Markdown body.
        body: String,
    },
    /// Create a general PR comment.
    ForgeComment {
        /// Account to use.
        account_id: Option<String>,
        /// Repository.
        repo: crate::forge::ForgeRepoRef,
        /// PR number.
        number: u32,
        /// Markdown body.
        body: String,
    },
    /// Open a pull request from a thread's branch.
    ///
    /// The daemon resolves the thread's workspace, detects its forge remote,
    /// pushes the branch when the remote does not already have it, then asks
    /// the forge to open the PR. The caller owns title and body generation.
    ForgeOpenPr {
        /// Thread whose branch becomes the PR head.
        thread_id: ThreadId,
        /// Explicit repository; detected from the workspace when omitted.
        repo: Option<crate::forge::ForgeRepoRef>,
        /// PR title.
        title: String,
        /// PR body.
        body: String,
        /// Target branch.
        base: String,
        /// Open as a draft.
        #[serde(default)]
        draft: bool,
    },
    // ------------------------------------------------------------------
    // Issues. Same accounts and clients as pull requests; Linear keeps its
    // own connection because its model (teams, workflow states) differs.
    // ------------------------------------------------------------------
    /// List a repository's issues.
    ForgeListIssues {
        /// Account to use; the only matching one when omitted.
        account_id: Option<String>,
        /// Repository to query.
        repo: crate::forge::ForgeRepoRef,
        /// Which issues to return.
        filter: crate::issues::ForgeIssueFilter,
        /// Page number, starting at 1.
        page: u32,
        /// The tracker's own search text, when non-empty.
        #[serde(default)]
        query: Option<String>,
    },
    /// Read one issue.
    ForgeGetIssue {
        /// Account to use.
        account_id: Option<String>,
        /// Repository.
        repo: crate::forge::ForgeRepoRef,
        /// Issue number.
        number: u32,
    },
    /// Read one issue's comments.
    ForgeListIssueComments {
        /// Account to use.
        account_id: Option<String>,
        /// Repository.
        repo: crate::forge::ForgeRepoRef,
        /// Issue number.
        number: u32,
        /// Provider continuation from the previous page, carried unchanged.
        #[serde(default)]
        cursor: Option<String>,
    },
    /// Post a comment on an issue.
    ForgeCommentIssue {
        /// Account to use.
        account_id: Option<String>,
        /// Repository.
        repo: crate::forge::ForgeRepoRef,
        /// Issue number.
        number: u32,
        /// Markdown body.
        body: String,
    },
    /// Change an issue's title, body, labels, assignees or state.
    ForgeUpdateIssue {
        /// Account to use.
        account_id: Option<String>,
        /// Repository.
        repo: crate::forge::ForgeRepoRef,
        /// Issue number.
        number: u32,
        /// Fields to change; the rest stay as they are.
        update: crate::issues::ForgeIssueUpdate,
    },
    /// List connected Linear workspaces (never the keys).
    LinearListAccounts,
    /// Validate an API key against Linear and store it.
    LinearConnect {
        /// Linear personal API key.
        api_key: String,
    },
    /// Forget a Linear connection and its key.
    LinearRemoveAccount {
        /// Connection to remove.
        account_id: String,
    },
    /// List the teams one Linear workspace exposes.
    LinearListTeams {
        /// Connection to use.
        account_id: String,
    },
    /// The Linear team chosen for each project.
    LinearProjectTeams,
    /// Attach a Linear team to a project, or detach it with `team: None`.
    LinearSetProjectTeam {
        /// h4rn project id.
        project_id: String,
        /// Chosen connection and team.
        #[serde(default)]
        team: Option<crate::issues::LinearProjectTeam>,
    },
    /// List the issues of the team a project chose.
    LinearListIssues {
        /// h4rn project id.
        project_id: String,
        /// Open, closed or every issue.
        filter: crate::issues::ForgeIssueFilter,
        /// Cursor returned by the previous page.
        #[serde(default)]
        cursor: Option<String>,
        /// Search text, when the tracker should search.
        #[serde(default)]
        query: Option<String>,
    },
    /// Read one Linear issue and its comments.
    LinearGetIssue {
        /// Connection to read with.
        account_id: String,
        /// Linear issue id.
        issue_id: String,
        /// Provider continuation for the next comment page.
        #[serde(default)]
        comments_after: Option<String>,
    },
    /// Comment on a Linear issue.
    LinearCommentIssue {
        /// Connection to comment with.
        account_id: String,
        /// Linear issue id.
        issue_id: String,
        /// Markdown body.
        body: String,
    },
    /// Link an issue to a thread. Linking reads it once so the answer carries
    /// its current state, and keeps the link in the journal across a restart.
    LinkIssue {
        /// Thread that will hold the link.
        thread_id: ThreadId,
        /// Account to read with; the matching one when omitted.
        account_id: Option<String>,
        /// Tracker coordinates.
        scope: crate::issues::IssueScope,
        /// Forge issue number, or Linear issue id.
        issue_id: String,
    },
    /// Remove an issue link from a thread.
    UnlinkIssue {
        /// Thread that holds the link.
        thread_id: ThreadId,
        /// Tracker coordinates.
        scope: crate::issues::IssueScope,
        /// Forge issue number, or Linear issue id.
        issue_id: String,
    },
    /// List a thread's linked issues with their latest tracker state.
    ListThreadIssues {
        /// Thread to list.
        thread_id: ThreadId,
    },
    // ------------------------------------------------------------------
    // Git actions for a thread.
    // ------------------------------------------------------------------
    /// Read the thread workspace's branch, upstream, counts and paths.
    GitStatus {
        /// Thread whose workspace is read.
        thread_id: ThreadId,
    },
    /// Stage what was asked and commit it.
    ///
    /// Without `paths` the commit records the index. `include_unstaged` stages
    /// tracked changes and new files first. With `paths`, only those paths are
    /// committed (`git commit --only`): other staged work stays staged, and the
    /// "nothing to commit" answer covers exactly the named paths. A detached
    /// `HEAD`, an in-progress rebase, merge or cherry-pick, or an empty index is
    /// refused with a message that names the reason.
    GitCommit {
        /// Thread whose workspace is committed.
        thread_id: ThreadId,
        /// Commit message, used verbatim.
        message: String,
        /// Stage unstaged changes before committing.
        #[serde(default)]
        include_unstaged: bool,
        /// Workspace-relative paths to stage; `None` stages everything.
        #[serde(default)]
        paths: Option<Vec<String>>,
    },
    /// Push the thread branch to its upstream, or set one on `origin`.
    ///
    /// A push is never forced.
    GitPush {
        /// Thread whose branch is pushed.
        thread_id: ThreadId,
        /// Create an upstream on `origin` when the branch has none.
        #[serde(default)]
        set_upstream: bool,
    },
    /// Write a commit message from the staged diff and recent subjects.
    ///
    /// The mode lives in Settings → Source control. Off returns an empty
    /// string and starts no model job; nothing is committed.
    GenerateGitCommitMessage {
        /// Thread whose workspace the diff comes from.
        thread_id: ThreadId,
        /// Describe the diff that would be committed with unstaged changes
        /// included. Does not stage anything.
        #[serde(default)]
        include_unstaged: bool,
        /// Workspace-relative paths the commit would include.
        #[serde(default)]
        paths: Option<Vec<String>>,
    },
    /// Write a pull request title and body from a thread's commits and diff.
    ///
    /// Nothing is pushed or opened.
    GeneratePullRequestText {
        /// Thread whose branch is summarised.
        thread_id: ThreadId,
        /// Branch the pull request would target; `None` uses the detected
        /// default branch.
        #[serde(default)]
        base: Option<String>,
    },
    /// Hand a pull request's unresolved threads and failing checks to an agent.
    ///
    /// With `thread_id` the daemon appends the context as a user message and
    /// starts a turn. Without one it creates a thread from `target` and sends
    /// the same context as the first prompt.
    ForgeHandPrToAgent {
        /// Account to use.
        account_id: Option<String>,
        /// Repository.
        repo: crate::forge::ForgeRepoRef,
        /// PR number.
        number: u32,
        /// Existing thread to update.
        thread_id: Option<ThreadId>,
        /// Creation target when `thread_id` is absent.
        target: Option<crate::forge::ForgeAgentTarget>,
    },
    /// Link a pull request to a thread.
    ///
    /// Linking reads the PR once from the forge so the answer carries its
    /// current state, and keeps the link in the thread journal across a restart.
    LinkPullRequest {
        /// Thread that will hold the link.
        thread_id: ThreadId,
        /// Account to read the PR with; the matching account when omitted.
        account_id: Option<String>,
        /// Repository.
        repo: crate::forge::ForgeRepoRef,
        /// PR number.
        number: u32,
    },
    /// Remove a pull request link from a thread, ending any watch on it.
    UnlinkPullRequest {
        /// Thread that holds the link.
        thread_id: ThreadId,
        /// Repository.
        repo: crate::forge::ForgeRepoRef,
        /// PR number.
        number: u32,
    },
    /// List a thread's linked pull requests with their latest forge state.
    ListThreadPullRequests {
        /// Thread to list.
        thread_id: ThreadId,
    },
    /// Threads that link to one pull request, for the PR page.
    ListPrThreads {
        /// Repository.
        repo: crate::forge::ForgeRepoRef,
        /// PR number.
        number: u32,
    },
    /// Watch a linked pull request so the agent wakes when it changes.
    ///
    /// The watch is daemon memory: links survive a restart, watches do not.
    WatchPullRequest {
        /// Thread that will be woken.
        thread_id: ThreadId,
        /// Repository.
        repo: crate::forge::ForgeRepoRef,
        /// PR number.
        number: u32,
    },
    /// Stop watching a linked pull request.
    UnwatchPullRequest {
        /// Thread that watches it.
        thread_id: ThreadId,
        /// Repository.
        repo: crate::forge::ForgeRepoRef,
        /// PR number.
        number: u32,
    },
    /// One cross-harness subagent operation (ADR-038).
    ///
    /// Like [`ClientRequest::Mcp`], every operation rides this single envelope
    /// so the delegation surface grows without new top-level discriminants.
    Subagent {
        /// Operation to perform.
        request: crate::subagents::SubagentRequest,
    },
    // ------------------------------------------------------------------
    // Harness CLI versions and updates.
    // ------------------------------------------------------------------
    /// Report each harness CLI's installed and available version.
    ///
    /// The latest version comes from the npm registry and is cached for a few
    /// hours. An offline registry is reported as unknown, never as an error.
    ListHarnessVersions,
    /// Update one harness CLI and stream its output as events.
    ///
    /// Refused while that harness has an active turn. The command runs with no
    /// shell and arguments passed as an array.
    UpdateHarness {
        /// Harness to update.
        harness: HarnessKind,
    },
    // ------------------------------------------------------------------
    // User-configured ACP agents.
    // ------------------------------------------------------------------
    /// List the local ACP agents configured on this daemon.
    ///
    /// Environment values are masked. Needs only the `view` scope.
    ListAcpAgents,
    /// Read one configuration with its real environment values.
    ///
    /// Admin-only: a value can be a credential, and the edit form is the only
    /// surface that needs it.
    GetAcpAgent {
        /// Instance id to read.
        id: String,
    },
    /// Add a local ACP agent.
    ///
    /// The daemon validates the id, command, arguments and environment and
    /// writes the list in its data directory. It does not run the command.
    AddAcpAgent {
        /// Configuration to persist.
        agent: crate::acp_agent::AcpAgentSpec,
    },
    /// Replace one configured ACP agent.
    UpdateAcpAgent {
        /// Instance id being changed.
        id: String,
        /// New configuration. `agent.id` must match `id`.
        agent: crate::acp_agent::AcpAgentSpec,
    },
    /// Remove one configured ACP agent. Threads keep their history.
    RemoveAcpAgent {
        /// Instance id to remove.
        id: String,
    },
    /// Start one configured agent and report what it advertises.
    ///
    /// This runs third-party code once (the ACP handshake and one
    /// `session/new`) and then stops it. Nothing about the process is kept.
    ProbeAcpAgent {
        /// Instance id to probe.
        id: String,
    },
    // ------------------------------------------------------------------
    // Worktrees.
    // ------------------------------------------------------------------
    /// List the git worktrees h4rn tracks: ones it made for subagents and ones
    /// threads and their subagents worked in.
    ///
    /// Answered with [`crate::ResponseBody::Worktrees`].
    ListWorktrees,
    /// Remove tracked worktrees from disk.
    ///
    /// Active worktrees are never removed. Without `force`, worktrees with
    /// uncommitted changes or unmerged owned branches are skipped. Deletes from
    /// disk, so it needs the `admin` scope.
    ClearWorktrees {
        /// Worktree paths to clear; `None` clears every non-active entry
        /// except thread workspaces, which are only removed when named.
        #[serde(default)]
        paths: Option<Vec<String>>,
        /// Remove even with uncommitted changes or unmerged commits.
        #[serde(default)]
        force: bool,
        /// Clear all: apply the same exclusions as `paths: None` — never a
        /// thread workspace — to the given paths.
        #[serde(default)]
        bulk: bool,
    },
    /// This machine's settings for new thread worktrees.
    ///
    /// Answered with [`crate::ResponseBody::WorktreeSettings`].
    GetWorktreeSettings,
    /// Change where new thread worktrees go and how their branches are named.
    ///
    /// Answered with the saved [`crate::ResponseBody::WorktreeSettings`].
    SetWorktreeSettings {
        /// New settings for this machine.
        settings: crate::WorktreeSettings,
    },
    // ------------------------------------------------------------------
    // Text generation: thread titles written by a model.
    // ------------------------------------------------------------------
    /// List the configured text generators.
    ///
    /// Keys are never included; each row only reports whether one is stored.
    ListTextGenerators,
    /// Add a text generator. Its key goes into the daemon secret store and
    /// needs the `admin` scope.
    AddTextGenerator {
        /// Generator to add.
        generator: crate::text_generation::TextGeneratorDraft,
    },
    /// Change a text generator. A replacement key needs the `admin` scope.
    UpdateTextGenerator {
        /// Generator to change.
        generator_id: crate::text_generation::TextGeneratorId,
        /// Fields to change.
        patch: crate::text_generation::TextGeneratorPatch,
    },
    /// Remove a text generator and its stored key.
    RemoveTextGenerator {
        /// Generator to remove.
        generator_id: crate::text_generation::TextGeneratorId,
    },
    /// Send a tiny prompt through one generator and return the honest result.
    TestTextGenerator {
        /// Generator to test.
        generator_id: crate::text_generation::TextGeneratorId,
        /// Model to test; `None` uses the generator's stored model.
        #[serde(default)]
        model: Option<String>,
    },
    /// Read which model writes thread titles.
    GetTextGenerationSettings,
    /// Replace the title model. Needs the `admin` scope.
    ///
    /// A client that predates the Commit & PR mode omits `source_control`; the
    /// daemon then keeps the mode already configured instead of resetting it.
    SetTextGenerationSettings {
        /// New settings.
        settings: crate::text_generation::TextGenerationSettingsUpdate,
    },
    /// Rename a thread. A generated title never replaces this name.
    RenameThread {
        /// Thread to rename.
        thread_id: ThreadId,
        /// New title, trimmed; empty is refused.
        title: String,
    },
    // ------------------------------------------------------------------
    // Projects: sidebar folders that exist before their first thread.
    // ------------------------------------------------------------------
    /// List the registered projects on a host.
    ListProjects {
        /// Host to list.
        host: HostId,
    },
    /// Add a project from an existing folder, a new folder or a clone.
    ///
    /// Adding a folder that is already registered returns that project.
    AddProject {
        /// Host that owns the folder.
        host: HostId,
        /// Where the project comes from.
        source: crate::projects::ProjectSource,
        /// Display name; defaults to the folder name.
        #[serde(default)]
        name: Option<String>,
    },
    /// Rename a project. The folder is not touched.
    RenameProject {
        /// Host that owns the project.
        host: HostId,
        /// Project to rename.
        project_id: String,
        /// New display name.
        name: String,
    },
    /// Remove a project from the sidebar. Its folder and threads are kept.
    RemoveProject {
        /// Host that owns the project.
        host: HostId,
        /// Project to remove.
        project_id: String,
    },
    /// List the folders inside one directory, for picking a project location.
    BrowseDirectories {
        /// Host to browse.
        host: HostId,
        /// Directory to list; `None` or `~` lists the home directory.
        #[serde(default)]
        path: Option<String>,
    },
}

impl ClientRequest {
    /// The scope a paired device must hold to send this request.
    ///
    /// The match is exhaustive on purpose: a new variant will not compile until
    /// it is classified. Reads of thread state and settings need only `view`.
    pub fn required_scope(&self) -> crate::connect::PairScope {
        use crate::connect::PairScope::{Admin, Approve, Files, Terminal, View};
        match self {
            Self::Hello { .. }
            | Self::ListHosts
            | Self::ListThreads { .. }
            | Self::GetThread { .. }
            | Self::Subscribe { .. }
            | Self::GetBlob { .. }
            | Self::McpList
            | Self::SkillsList { .. }
            | Self::SkillsConflicts { .. }
            | Self::SkillsDiff { .. }
            | Self::ListProxyProfiles
            | Self::ListProxyModels { .. }
            | Self::ListProxyLimits { .. }
            | Self::UsageQuery { .. }
            | Self::GetUsage { .. }
            | Self::GetLimits
            | Self::ListResets
            | Self::ListFailoverOptions { .. }
            | Self::ListHarnessVersions
            | Self::ListAcpAgents
            | Self::ListWorktrees
            | Self::GetWorktreeSettings
            | Self::ForgeListAccounts
            | Self::ForgeDetectRepo { .. }
            | Self::ForgeListPrs { .. }
            | Self::ForgeGetPr { .. }
            | Self::ForgeGetPrDiff { .. }
            | Self::ForgeListPrComments { .. }
            | Self::ForgeListChecks { .. }
            | Self::ForgeListPrCommits { .. }
            | Self::ListThreadPullRequests { .. }
            | Self::ListPrThreads { .. }
            | Self::ForgeListIssues { .. }
            | Self::ForgeGetIssue { .. }
            | Self::ForgeListIssueComments { .. }
            | Self::LinearListAccounts
            | Self::LinearListTeams { .. }
            | Self::LinearProjectTeams
            | Self::LinearListIssues { .. }
            | Self::LinearGetIssue { .. }
            | Self::ListThreadIssues { .. }
            | Self::ListProjects { .. }
            | Self::GetNetworkAccess
            | Self::ListDirectDevices
            | Self::ListTextGenerators
            | Self::GetTextGenerationSettings
            | Self::ListSchedules
            | Self::ListScheduleRuns { .. }
            | Self::GitStatus { .. } => View,
            Self::CreateThread { .. }
            | Self::SendMessage { .. }
            | Self::Interrupt { .. }
            | Self::RespondApproval { .. }
            | Self::BeginUpload { .. }
            | Self::CommitUpload { .. }
            | Self::AbortUpload { .. }
            | Self::QueryUpload { .. }
            | Self::SetThreadTab { .. }
            | Self::SetThreadRuntimeMode { .. }
            | Self::SetThreadModel { .. }
            | Self::SetThreadReasoningEffort { .. }
            | Self::SetThreadFast { .. }
            | Self::Snooze { .. }
            | Self::CancelResume { .. }
            | Self::ResumeNow { .. }
            | Self::FailOver { .. }
            | Self::GitCommit { .. }
            | Self::GitPush { .. }
            | Self::GenerateGitCommitMessage { .. }
            | Self::GeneratePullRequestText { .. }
            | Self::ForgeRerunChecks { .. }
            | Self::ForgeReply { .. }
            | Self::ForgeComment { .. }
            | Self::ForgeOpenPr { .. }
            | Self::ForgeHandPrToAgent { .. }
            | Self::LinkPullRequest { .. }
            | Self::UnlinkPullRequest { .. }
            | Self::WatchPullRequest { .. }
            | Self::UnwatchPullRequest { .. }
            | Self::ForgeCommentIssue { .. }
            | Self::ForgeUpdateIssue { .. }
            | Self::LinearCommentIssue { .. }
            | Self::LinearSetProjectTeam { .. }
            | Self::LinkIssue { .. }
            | Self::UnlinkIssue { .. }
            | Self::RenameThread { .. }
            | Self::RenameProject { .. }
            | Self::RemoveProject { .. } => Approve,
            Self::TerminalOpen { .. }
            | Self::TerminalAttach { .. }
            | Self::TerminalList { .. }
            | Self::TerminalResize { .. }
            | Self::TerminalAck { .. }
            | Self::TerminalClose { .. } => Terminal,
            Self::FsList { .. }
            | Self::FsRead { .. }
            | Self::FsWrite { .. }
            | Self::FsCreate { .. }
            | Self::FsRename { .. }
            | Self::FsDelete { .. }
            | Self::FsRestore { .. }
            | Self::FsSearch { .. }
            | Self::FsWatch { .. }
            | Self::FsUnwatch { .. }
            | Self::FsDiff { .. }
            | Self::FsTurnChanges { .. }
            | Self::FsDiffFiles { .. }
            | Self::FsDiffCommits { .. }
            | Self::FsBranches { .. }
            | Self::AddProject { .. }
            | Self::BrowseDirectories { .. } => Files,
            Self::McpUpsert { .. }
            | Self::McpRemove { .. }
            | Self::SkillsAdd { .. }
            | Self::SkillsUpdate { .. }
            | Self::SkillsRemove { .. }
            | Self::SkillsSetEnabled { .. }
            | Self::SkillsImportPreview { .. }
            | Self::SkillsImportCommit { .. }
            | Self::SkillsResolveConflict { .. }
            | Self::SkillsSyncNow
            | Self::AddProxyProfile { .. }
            | Self::UpdateProxyProfile { .. }
            | Self::RemoveProxyProfile { .. }
            | Self::TestProxyProfile { .. }
            | Self::SetProxyHarnessOverride { .. }
            | Self::ApplyProxyOverrides { .. }
            | Self::PickProxyFailover { .. }
            | Self::SetAutoResumePolicy { .. }
            | Self::GetAcpAgent { .. }
            | Self::AddAcpAgent { .. }
            | Self::UpdateAcpAgent { .. }
            | Self::RemoveAcpAgent { .. }
            | Self::ProbeAcpAgent { .. }
            | Self::ForgeAddAccount { .. }
            | Self::ForgeRemoveAccount { .. }
            | Self::ForgeDeviceFlowStart { .. }
            | Self::ForgeDeviceFlowPoll { .. }
            | Self::LinearConnect { .. }
            | Self::LinearRemoveAccount { .. }
            | Self::PairingConfirmations
            | Self::ConfirmPairing { .. }
            | Self::SetNetworkAccess { .. }
            | Self::CreateDirectPairing { .. }
            | Self::CancelDirectPairing
            | Self::RevokeDirectDevice { .. }
            | Self::ForgeImportCli { .. }
            | Self::UpdateHarness { .. }
            | Self::ClearWorktrees { .. }
            | Self::SetWorktreeSettings { .. }
            | Self::AddTextGenerator { .. }
            | Self::UpdateTextGenerator { .. }
            | Self::RemoveTextGenerator { .. }
            | Self::TestTextGenerator { .. }
            | Self::SetTextGenerationSettings { .. }
            | Self::CreateSchedule { .. }
            | Self::UpdateSchedule { .. }
            | Self::DeleteSchedule { .. }
            | Self::SetScheduleEnabled { .. }
            | Self::RunScheduleNow { .. }
            | Self::RotateScheduleWebhook { .. }
            | Self::GetScheduleWebhookUrl { .. } => Admin,
            Self::Mcp { request } => match request {
                crate::mcp::McpRequest::ListServers
                | crate::mcp::McpRequest::ListTools { .. }
                | crate::mcp::McpRequest::PendingApprovals => View,
                // Answering a remote call is the owner's approval, not an
                // administrative reconfiguration.
                crate::mcp::McpRequest::RespondApproval { .. } => Approve,
                _ => Admin,
            },
            Self::Subagent { request } => match request {
                crate::subagents::SubagentRequest::ListSubagents { .. }
                | crate::subagents::SubagentRequest::GetSubagent { .. } => View,
                _ => Approve,
            },
        }
    }
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
    fn required_scopes_follow_what_a_request_can_change() {
        use crate::connect::PairScope::{Admin, Approve, Files, Terminal, View};
        let repo = || crate::forge::ForgeRepoRef {
            forge: crate::forge::ForgeKind::GitHub,
            base_url: "https://github.com".into(),
            owner: "o".into(),
            repo: "r".into(),
        };
        let thread = || ThreadId::new("th_1");
        let cases: Vec<(ClientRequest, crate::connect::PairScope)> = vec![
            (ClientRequest::ListHosts, View),
            (
                ClientRequest::ListFailoverOptions {
                    thread_id: thread(),
                },
                View,
            ),
            (
                ClientRequest::FailOver {
                    thread_id: thread(),
                    model: None,
                    account: None,
                },
                Approve,
            ),
            (
                ClientRequest::ForgeListPrCommits {
                    account_id: None,
                    repo: repo(),
                    number: 1,
                },
                View,
            ),
            (
                ClientRequest::ForgeRerunChecks {
                    account_id: None,
                    repo: repo(),
                    number: 1,
                },
                Approve,
            ),
            // Linking and watching change thread-owned state; listing them
            // only reads.
            (
                ClientRequest::ListThreadPullRequests {
                    thread_id: thread(),
                },
                View,
            ),
            (
                ClientRequest::ListPrThreads {
                    repo: repo(),
                    number: 1,
                },
                View,
            ),
            (
                ClientRequest::LinkPullRequest {
                    thread_id: thread(),
                    account_id: None,
                    repo: repo(),
                    number: 1,
                },
                Approve,
            ),
            (
                ClientRequest::UnlinkPullRequest {
                    thread_id: thread(),
                    repo: repo(),
                    number: 1,
                },
                Approve,
            ),
            (
                ClientRequest::WatchPullRequest {
                    thread_id: thread(),
                    repo: repo(),
                    number: 1,
                },
                Approve,
            ),
            (
                ClientRequest::UnwatchPullRequest {
                    thread_id: thread(),
                    repo: repo(),
                    number: 1,
                },
                Approve,
            ),
            // Issue reads are view; writing to a tracker and linking a thread
            // are approve. Connecting Linear changes credentials, so admin.
            (
                ClientRequest::ForgeListIssues {
                    account_id: None,
                    repo: repo(),
                    filter: crate::issues::ForgeIssueFilter::Open,
                    page: 1,
                    query: None,
                },
                View,
            ),
            (
                ClientRequest::ForgeUpdateIssue {
                    account_id: None,
                    repo: repo(),
                    number: 1,
                    update: crate::issues::ForgeIssueUpdate::default(),
                },
                Approve,
            ),
            (
                ClientRequest::ForgeCommentIssue {
                    account_id: None,
                    repo: repo(),
                    number: 1,
                    body: "Confirmed.".into(),
                },
                Approve,
            ),
            (
                ClientRequest::LinearCommentIssue {
                    account_id: "lin_1".into(),
                    issue_id: "iss_1".into(),
                    body: "Shipped.".into(),
                },
                Approve,
            ),
            (
                ClientRequest::LinearSetProjectTeam {
                    project_id: "proj_1".into(),
                    team: None,
                },
                Approve,
            ),
            (
                ClientRequest::LinearRemoveAccount {
                    account_id: "lin_1".into(),
                },
                Admin,
            ),
            (
                ClientRequest::UnlinkIssue {
                    thread_id: thread(),
                    scope: crate::issues::IssueScope::Forge { repo: repo() },
                    issue_id: "1".into(),
                },
                Approve,
            ),
            (
                ClientRequest::LinkIssue {
                    thread_id: thread(),
                    account_id: None,
                    scope: crate::issues::IssueScope::Forge { repo: repo() },
                    issue_id: "1".into(),
                },
                Approve,
            ),
            (
                ClientRequest::ListThreadIssues {
                    thread_id: thread(),
                },
                View,
            ),
            (
                ClientRequest::LinearConnect {
                    api_key: "k".into(),
                },
                Admin,
            ),
            (
                ClientRequest::RespondApproval {
                    thread_id: thread(),
                    approval_id: "a".into(),
                    decision: ApprovalDecision::Approve,
                    option_id: None,
                },
                Approve,
            ),
            (ClientRequest::TerminalList { thread_id: None }, Terminal),
            (
                ClientRequest::FsDelete {
                    host: HostId::new("h"),
                    workspace: "/w".into(),
                    path: "a".into(),
                },
                Files,
            ),
            // A diff carries file contents, so a view-only device must not
            // be able to ask for one.
            (
                ClientRequest::FsDiff {
                    host: HostId::new("h"),
                    workspace: "/w".into(),
                    path: "a".into(),
                    against: crate::fs::DiffAgainst::Head,
                },
                Files,
            ),
            (
                ClientRequest::FsTurnChanges {
                    host: HostId::new("h"),
                    workspace: "/w".into(),
                    thread_id: thread(),
                    turn_id: crate::ids::TurnId::new("turn_1"),
                },
                Files,
            ),
            (
                ClientRequest::FsDiffFiles {
                    host: HostId::new("h"),
                    workspace: "/w".into(),
                    against: crate::fs::DiffAgainst::Base { base: None },
                },
                Files,
            ),
            (
                ClientRequest::FsDiffCommits {
                    host: HostId::new("h"),
                    workspace: "/w".into(),
                    base: None,
                },
                Files,
            ),
            (
                ClientRequest::FsBranches {
                    host: HostId::new("h"),
                    workspace: "/w".into(),
                },
                Files,
            ),
            (ClientRequest::McpList, View),
            (
                ClientRequest::Mcp {
                    request: crate::mcp::McpRequest::PendingApprovals,
                },
                View,
            ),
            (
                ClientRequest::Mcp {
                    request: crate::mcp::McpRequest::RespondApproval {
                        approval_id: "mcp-1".into(),
                        decision: ApprovalDecision::Approve,
                    },
                },
                Approve,
            ),
            // Both persist per-thread state, so a view-only device must not
            // be able to send them.
            (
                ClientRequest::SetThreadTab {
                    thread_id: thread(),
                    tab: ThreadTab::Working,
                    pinned: true,
                },
                Approve,
            ),
            (
                ClientRequest::SetThreadRuntimeMode {
                    thread_id: thread(),
                    mode: RuntimeMode::Plan,
                },
                Approve,
            ),
            (
                ClientRequest::SetThreadModel {
                    thread_id: thread(),
                    model: "m".into(),
                },
                Approve,
            ),
            (
                ClientRequest::SetThreadReasoningEffort {
                    thread_id: thread(),
                    effort: Some("high".into()),
                },
                Approve,
            ),
            (
                ClientRequest::SetThreadFast {
                    thread_id: thread(),
                    fast: true,
                },
                Approve,
            ),
            (
                ClientRequest::Snooze {
                    thread_id: thread(),
                    until: None,
                },
                Approve,
            ),
            (ClientRequest::McpRemove { name: "x".into() }, Admin),
            // Version reads are harmless; running a package manager is not.
            (ClientRequest::ListHarnessVersions, View),
            (ClientRequest::ListAcpAgents, View),
            (ClientRequest::GetAcpAgent { id: "demo".into() }, Admin),
            (
                ClientRequest::AddAcpAgent {
                    agent: crate::acp_agent::AcpAgentSpec {
                        id: "demo".into(),
                        name: "Demo".into(),
                        command: "demo".into(),
                        args: Vec::new(),
                        env: std::collections::BTreeMap::new(),
                        working_dir: crate::acp_agent::AcpWorkingDir::ThreadWorkspace,
                    },
                },
                Admin,
            ),
            (
                ClientRequest::UpdateAcpAgent {
                    id: "demo".into(),
                    agent: crate::acp_agent::AcpAgentSpec {
                        id: "demo".into(),
                        name: "Demo".into(),
                        command: "demo".into(),
                        args: Vec::new(),
                        env: std::collections::BTreeMap::new(),
                        working_dir: crate::acp_agent::AcpWorkingDir::ThreadWorkspace,
                    },
                },
                Admin,
            ),
            (ClientRequest::RemoveAcpAgent { id: "demo".into() }, Admin),
            (ClientRequest::ProbeAcpAgent { id: "demo".into() }, Admin),
            (
                ClientRequest::UpdateHarness {
                    harness: crate::common::HarnessKind::ClaudeCode,
                },
                Admin,
            ),
            // Clearing deletes checkouts from disk.
            (ClientRequest::ListWorktrees, View),
            (ClientRequest::GetWorktreeSettings, View),
            (
                ClientRequest::ClearWorktrees {
                    paths: None,
                    force: false,
                    bulk: false,
                },
                Admin,
            ),
            (
                ClientRequest::SetWorktreeSettings {
                    settings: crate::WorktreeSettings::default(),
                },
                Admin,
            ),
            // Reading task lists is harmless; every write schedules work or
            // hands out a webhook URL.
            (ClientRequest::ListSchedules, View),
            (
                ClientRequest::ListScheduleRuns {
                    task_id: crate::ids::ScheduleTaskId::new("sch_1"),
                },
                View,
            ),
            (
                ClientRequest::DeleteSchedule {
                    task_id: crate::ids::ScheduleTaskId::new("sch_1"),
                },
                Admin,
            ),
            (
                ClientRequest::RunScheduleNow {
                    task_id: crate::ids::ScheduleTaskId::new("sch_1"),
                },
                Admin,
            ),
            (ClientRequest::PairingConfirmations, Admin),
            (
                ClientRequest::ConfirmPairing {
                    device_id: DeviceId::new("dev_1"),
                    confirm: true,
                },
                Admin,
            ),
            // Reading the listener state is harmless; changing it, or opening
            // a pairing window that admits a device, is administrative.
            (ClientRequest::GetNetworkAccess, View),
            (ClientRequest::SetNetworkAccess { enabled: true }, Admin),
            (
                ClientRequest::CreateDirectPairing {
                    scopes: Vec::new(),
                    label: None,
                },
                Admin,
            ),
            (ClientRequest::CancelDirectPairing, Admin),
            (ClientRequest::ListDirectDevices, View),
            (
                ClientRequest::RevokeDirectDevice {
                    key: "00".repeat(32),
                },
                Admin,
            ),
        ];
        for (request, expected) in cases {
            assert_eq!(request.required_scope(), expected, "{request:?}");
        }
    }

    #[test]
    fn forge_add_account_repository_hint_is_optional() {
        let without = serde_json::json!({
            "type": "forge_add_account",
            "forge": "bitbucket",
            "base_url": "https://bitbucket.org",
            "token": "ATBB-token",
            "username": null,
            "source": "bitbucket_access_token",
        });
        let request: ClientRequest = serde_json::from_value(without).unwrap();
        let ClientRequest::ForgeAddAccount { repository, .. } = &request else {
            panic!("expected forge_add_account");
        };
        assert!(repository.is_none(), "the field defaults to no repository");

        let repo = crate::forge::ForgeRepoRef {
            forge: crate::forge::ForgeKind::Bitbucket,
            base_url: "https://bitbucket.org".into(),
            owner: "acme".into(),
            repo: "widget".into(),
        };
        let with = serde_json::json!({
            "type": "forge_add_account",
            "forge": "bitbucket",
            "base_url": "https://bitbucket.org",
            "token": "ATBB-token",
            "username": null,
            "source": "bitbucket_access_token",
            "repository": repo,
        });
        let request: ClientRequest = serde_json::from_value(with).unwrap();
        let ClientRequest::ForgeAddAccount { repository, .. } = request else {
            panic!("expected forge_add_account");
        };
        assert_eq!(repository, Some(repo));
    }

    #[test]
    fn an_older_positioned_attach_is_treated_as_legacy() {
        // The first positioned-reply flag numbered answers by the chunk where
        // the query ended. A client that still sends only that flag must not be
        // trusted as a protocol 2 viewer, so the new field stays false.
        let request: ClientRequest = serde_json::from_value(serde_json::json!({
            "type": "terminal_attach",
            "terminal_id": "term_1",
            "reply_positions": true,
        }))
        .expect("an older client's attach still parses");
        let ClientRequest::TerminalAttach {
            reply_positions_v2, ..
        } = request
        else {
            panic!("expected terminal_attach");
        };
        assert!(!reply_positions_v2, "an older reply flag is legacy");

        // A client that omits every flag is legacy too.
        let request: ClientRequest = serde_json::from_value(serde_json::json!({
            "type": "terminal_attach",
            "terminal_id": "term_1",
        }))
        .expect("an attach without flags parses");
        let ClientRequest::TerminalAttach {
            reply_positions_v2, ..
        } = request
        else {
            panic!("expected terminal_attach");
        };
        assert!(!reply_positions_v2);

        let request: ClientRequest = serde_json::from_value(serde_json::json!({
            "type": "terminal_attach",
            "terminal_id": "term_1",
            "reply_positions_v2": true,
        }))
        .expect("a protocol 2 attach parses");
        let ClientRequest::TerminalAttach {
            reply_positions_v2, ..
        } = request
        else {
            panic!("expected terminal_attach");
        };
        assert!(reply_positions_v2);
    }

    #[test]
    fn request_round_trips() {
        let frame = ClientFrame {
            id: 1,
            body: ClientRequest::SetThreadRuntimeMode {
                thread_id: ThreadId::new("th_1"),
                mode: RuntimeMode::Plan,
            },
        };
        let json = serde_json::to_string(&frame).unwrap();
        let back: ClientFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(frame, back);
        assert!(json.contains("\"type\":\"set_thread_runtime_mode\""));
        assert!(json.contains("\"mode\":\"plan\""));
    }

    #[test]
    fn an_old_create_thread_without_thread_controls_still_deserializes() {
        let json = serde_json::json!({
            "type": "create_thread",
            "host": "host_1",
            "harness": "claude_code",
            "workspace_path": "/tmp/work",
            "model": "claude-sonnet-5",
            "prompt": "hello",
            "attachments": [],
        });
        let request: ClientRequest = serde_json::from_value(json).expect("old JSON");
        let ClientRequest::CreateThread {
            runtime_mode,
            reasoning_effort,
            fast,
            ..
        } = request
        else {
            panic!("expected create_thread");
        };
        assert_eq!(runtime_mode, RuntimeMode::ApprovalRequired);
        assert_eq!(reasoning_effort, None);
        assert!(!fast);
    }

    #[test]
    fn link_requests_round_trip_with_stable_tags() {
        let repo = crate::forge::ForgeRepoRef {
            forge: crate::forge::ForgeKind::GitHub,
            base_url: "https://github.com".into(),
            owner: "o".into(),
            repo: "r".into(),
        };
        for request in [
            ClientRequest::LinkPullRequest {
                thread_id: ThreadId::new("th_1"),
                account_id: Some("acct_1".into()),
                repo: repo.clone(),
                number: 4,
            },
            ClientRequest::UnlinkPullRequest {
                thread_id: ThreadId::new("th_1"),
                repo: repo.clone(),
                number: 4,
            },
            ClientRequest::ListThreadPullRequests {
                thread_id: ThreadId::new("th_1"),
            },
            ClientRequest::ListPrThreads {
                repo: repo.clone(),
                number: 4,
            },
            ClientRequest::WatchPullRequest {
                thread_id: ThreadId::new("th_1"),
                repo: repo.clone(),
                number: 4,
            },
            ClientRequest::UnwatchPullRequest {
                thread_id: ThreadId::new("th_1"),
                repo,
                number: 4,
            },
        ] {
            let frame = ClientFrame {
                id: 2,
                body: request.clone(),
            };
            let json = serde_json::to_string(&frame).unwrap();
            let back: ClientFrame = serde_json::from_str(&json).unwrap();
            assert_eq!(frame, back);
        }
    }

    #[test]
    fn a_capabilities_row_from_an_older_daemon_still_parses() {
        let row: crate::forge::ForgeCapabilityEntry = serde_json::from_value(serde_json::json!({
            "forge": "github",
            "capabilities": {
                "device_flow": true,
                "review_threads": true,
                "resolve_threads": true,
                "nested_replies": true,
                "inline_review_comments": true,
                "checks": true,
                "pipelines": false,
                "diff_pagination": true,
                "graphql": true,
                "rerun_checks": true
            }
        }))
        .unwrap();
        assert!(!row.capabilities.conditional_poll);
    }
}
