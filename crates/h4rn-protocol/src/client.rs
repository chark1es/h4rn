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
    /// One cross-harness subagent operation (ADR-038).
    ///
    /// Like [`ClientRequest::Mcp`], every operation rides this single envelope
    /// so the delegation surface grows without new top-level discriminants.
    Subagent {
        /// Operation to perform.
        request: crate::subagents::SubagentRequest,
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
            | Self::SetThreadTab { .. }
            | Self::Snooze { .. }
            | Self::Subscribe { .. }
            | Self::GetBlob { .. }
            | Self::FsDiff { .. }
            | Self::FsTurnChanges { .. }
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
            | Self::ForgeListAccounts
            | Self::ForgeDetectRepo { .. }
            | Self::ForgeListPrs { .. }
            | Self::ForgeGetPr { .. }
            | Self::ForgeGetPrDiff { .. }
            | Self::ForgeListPrComments { .. }
            | Self::ForgeListChecks { .. } => View,
            Self::CreateThread { .. }
            | Self::SendMessage { .. }
            | Self::Interrupt { .. }
            | Self::RespondApproval { .. }
            | Self::BeginUpload { .. }
            | Self::CommitUpload { .. }
            | Self::AbortUpload { .. }
            | Self::QueryUpload { .. }
            | Self::CancelResume { .. }
            | Self::ResumeNow { .. }
            | Self::FailOver { .. }
            | Self::ForgeReply { .. }
            | Self::ForgeComment { .. }
            | Self::ForgeOpenPr { .. }
            | Self::ForgeHandPrToAgent { .. } => Approve,
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
            | Self::FsUnwatch { .. } => Files,
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
            | Self::ProxyProbe { .. }
            | Self::ProxyConfigure { .. }
            | Self::AddProxyProfile { .. }
            | Self::UpdateProxyProfile { .. }
            | Self::RemoveProxyProfile { .. }
            | Self::TestProxyProfile { .. }
            | Self::SetProxyHarnessOverride { .. }
            | Self::PickProxyFailover { .. }
            | Self::SetAutoResumePolicy { .. }
            | Self::ForgeAddAccount { .. }
            | Self::ForgeRemoveAccount { .. }
            | Self::ForgeDeviceFlowStart { .. }
            | Self::ForgeDeviceFlowPoll { .. }
            | Self::ForgeImportCli { .. } => Admin,
            Self::Mcp { request } => match request {
                crate::mcp::McpRequest::ListServers | crate::mcp::McpRequest::ListTools { .. } => {
                    View
                }
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
