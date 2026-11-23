//! Daemon-to-client frames, responses and events.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::blob::{BlobDownload, BlobInfo, UploadState, UploadTicket};
use crate::common::{
    AgentState, ApprovalKind, ApprovalOption, HostSummary, Item, ThreadSummary, ToolCall,
    TurnStatus, Usage,
};
use crate::ids::{DeviceId, HostId, SubagentId, ThreadId, TurnId};
use crate::subagents::SubagentView;
use crate::terminal::{TerminalExitNotice, TerminalInfo};

/// One frame sent by the daemon.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerFrame {
    /// Reply to one [`crate::ClientFrame`].
    Response {
        /// The `id` from the request being answered.
        #[ts(type = "number")]
        id: u64,
        /// Success payload or a typed protocol error.
        result: Result<ResponseBody, ProtoError>,
    },
    /// An event on the daemon-global ordered stream.
    Event {
        /// Durable, daemon-global monotonically increasing sequence number.
        ///
        /// The value survives daemon restarts and is shared by every thread, so
        /// a client resumes by passing the last value it applied (ADR-003).
        #[ts(type = "number")]
        seq: u64,
        /// The event itself.
        event: Event,
    },
    /// An ephemeral terminal notice, outside the durable thread event log.
    ///
    /// Terminal exit carries no durable sequence: the terminal id is scoped to
    /// one daemon run and a reconnecting client learns the status from
    /// [`ResponseBody::Terminals`] or [`ResponseBody::TerminalAttached`].
    TerminalEvent {
        /// The terminal that changed.
        event: TerminalExitNotice,
    },
    /// An ephemeral CLIProxyAPI change, outside the durable thread log.
    ///
    /// Proxy profiles are daemon-global rather than thread-scoped, so they do
    /// not carry a sequence number. A client re-reads the list with
    /// [`crate::ClientRequest::ListProxyProfiles`] after a reconnect.
    ProxyEvent {
        /// The change.
        event: crate::proxy::ProxyEvent,
    },
    /// A live metering frame, outside the durable thread log.
    ///
    /// Counters are throttled to about one per second and a reconnecting client
    /// learns the authoritative totals from the ledger (`GetUsage`) or from
    /// the thread summary instead.
    UsageEvent {
        /// The tick, limit status or schedule change.
        event: crate::usage::UsageStreamEvent,
    },
    /// Stepped output of a running harness update, outside the durable log.
    ///
    /// A reconnecting client re-reads the versions with
    /// [`crate::ClientRequest::ListHarnessVersions`]; the stream is not replayed.
    HarnessEvent {
        /// One output line or the final result.
        event: crate::HarnessUpdateEvent,
    },
    /// An ephemeral scheduled-task change, outside the durable thread log.
    ///
    /// A reconnecting client re-reads the list with
    /// [`crate::ClientRequest::ListSchedules`].
    ScheduleEvent {
        /// The task that was created, changed or deleted.
        event: crate::ScheduleEvent,
    },
}

/// A device paired directly (LAN/tailnet) with this computer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct DirectDevice {
    /// Noise static key, hex. Identifies the device for a revoke.
    pub key: String,
    /// Scopes the device holds.
    pub scopes: Vec<crate::connect::PairScope>,
    /// When it paired; absent for devices paired before this was recorded.
    pub paired_at: Option<DateTime<Utc>>,
    /// Whether it has a live secure session right now.
    pub connected: bool,
}

/// One durable event with its daemon-global sequence number.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct SeqEvent {
    /// Durable daemon-global sequence.
    #[ts(type = "number")]
    pub seq: u64,
    /// The projected event.
    pub event: Event,
}

/// Successful response payloads.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseBody {
    /// Completed the connection handshake.
    Hello {
        /// Protocol version the daemon speaks.
        protocol_version: u32,
        /// Daemon build version.
        server_version: String,
        /// Device id the daemon resolved for this connection.
        device_id: DeviceId,
        /// Connect machine id this daemon owns, when it is signed in.
        ///
        /// It comes from the daemon's own stored connect session, never from
        /// the connecting device's claim, so a client can treat it as the
        /// machine a direct socket actually reached. Absent for a signed-out
        /// daemon and for an older daemon that does not report it.
        #[serde(default)]
        daemon_machine_id: Option<String>,
        /// Host id of the machine this daemon runs on.
        ///
        /// Every daemon-scoped request that names a host (skills, the
        /// filesystem, terminals) uses this id for "this machine". Hosts in
        /// `hosts` that are not this one are entries the daemon can see but
        /// does not execute on.
        local_host: HostId,
        /// Hosts this daemon can route to.
        hosts: Vec<HostSummary>,
        /// The daemon machine's IANA time zone, such as `Europe/Berlin`.
        ///
        /// An editor that creates a schedule uses this as the default, so a
        /// floating date and time mean the daemon's wall clock rather than a
        /// phone in another zone. An older daemon omits it; the client falls
        /// back to its own zone.
        #[serde(default)]
        daemon_timezone: String,
    },
    /// Host list.
    Hosts {
        /// Reachable hosts.
        hosts: Vec<HostSummary>,
    },
    /// Thread list.
    Threads {
        /// Matching threads.
        threads: Vec<ThreadSummary>,
    },
    /// One thread, its summary, and any events after the requested sequence.
    Thread {
        /// Current projection.
        summary: ThreadSummary,
        /// Events the client has not seen, each with its durable sequence.
        events: Vec<SeqEvent>,
        /// Next sequence the client should ask for.
        #[ts(type = "number")]
        next_seq: u64,
    },
    /// A thread was created and its first turn is durable.
    ///
    /// The client uses these ids to reconcile the optimistic row it rendered
    /// before the round trip, without waiting for `thread_upserted`.
    ThreadCreated {
        /// New thread id.
        thread_id: ThreadId,
        /// Turn enqueued for the first prompt.
        turn_id: TurnId,
        /// Current projection of the new thread.
        summary: ThreadSummary,
    },
    /// A user message was appended to a thread.
    MessageAccepted {
        /// Thread that accepted the message.
        thread_id: ThreadId,
        /// Turn the message belongs to (the running turn when it was queued).
        turn_id: TurnId,
        /// Durable daemon-global sequence of the last event written for it.
        #[ts(type = "number")]
        seq: u64,
    },
    /// A terminal was opened.
    TerminalOpened {
        /// The new terminal, including its effective size.
        terminal: TerminalInfo,
    },
    /// A viewer attached to a terminal.
    TerminalAttached {
        /// The terminal as the viewer sees it.
        terminal: TerminalInfo,
        /// ANSI that recreates the screen and scrollback on a fresh grid.
        snapshot: String,
        /// Last output sequence already contained in `snapshot`.
        #[ts(type = "number")]
        start_seq: u64,
    },
    /// Terminals owned by the daemon, oldest first.
    Terminals {
        /// Matching terminals.
        terminals: Vec<TerminalInfo>,
    },
    /// One directory listing for IDE mode.
    FsListing {
        /// The listing.
        listing: crate::fs::FsListing,
    },
    /// One file read for IDE mode.
    FsFile {
        /// The file and its etag.
        file: crate::fs::FsFileContent,
    },
    /// A write completed or lost the etag race.
    FsWritten {
        /// Result, including any conflict payload.
        result: crate::fs::FsWriteResult,
    },
    /// A path was created.
    FsEntryCreated {
        /// The new entry.
        entry: crate::fs::FileEntry,
    },
    /// A path was moved into the trash directory.
    FsDeleted {
        /// Trash id and destination.
        result: crate::fs::FsDeleteResult,
    },
    /// A path was restored from the trash directory.
    FsRestored {
        /// Workspace-relative path that is back on disk.
        path: String,
    },
    /// Search started; results follow as `fs_search_batch` events.
    FsSearchStarted {
        /// Correlation id for the batches.
        search_id: String,
    },
    /// The connection is now watching this workspace.
    FsWatching {
        /// Canonical workspace root.
        workspace: String,
        /// Workspace-relative paths being watched; empty means the whole tree.
        paths: Vec<String>,
    },
    /// Both sides of one diff.
    FsDiff {
        /// The diff.
        diff: crate::fs::FsDiff,
    },
    /// The files one agent turn touched.
    FsTurnChanges {
        /// Turn change set.
        changes: crate::fs::FsTurnChanges,
    },
    /// The files a branch or commit range changed.
    FsDiffFiles {
        /// File list.
        files: crate::fs::FsDiffFiles,
    },
    /// The newest commits in the branch comparison.
    FsDiffCommits {
        /// Commit list.
        commits: crate::fs::FsDiffCommitList,
    },
    /// Branch names a diff can be compared against.
    FsBranches {
        /// Branch list.
        branches: crate::fs::FsBranchList,
    },
    /// The skill registry and the per-harness install matrix.
    Skills {
        /// Inventory the Skills screen renders.
        inventory: crate::skills::SkillInventory,
    },
    /// One skill and its canonical manifest.
    SkillDetail {
        /// Summary and manifest.
        detail: crate::skills::SkillDetail,
    },
    /// A skill was added or updated.
    SkillAdded {
        /// New registry row.
        skill: crate::skills::SkillSummary,
    },
    /// Unresolved skill conflicts.
    SkillConflicts {
        /// Conflicts the user can settle.
        conflicts: Vec<crate::skills::SkillConflict>,
    },
    /// Preview of an import scan.
    SkillsImportPreview {
        /// Candidates and conflict counts.
        preview: crate::skills::SkillImportPreview,
    },
    /// A diff between two skill versions.
    SkillDiff {
        /// Changed files.
        diff: crate::skills::SkillDiff,
    },
    /// Result of the latest sync.
    SkillSyncStatus {
        /// State, counters and conflicts.
        status: crate::skills::SkillSyncStatus,
    },
    /// An upload stream is ready for binary chunks.
    UploadStarted {
        /// Opaque upload id and its binary stream id.
        upload: UploadTicket,
    },
    /// An upload was committed and the blob is now addressable.
    UploadCommitted {
        /// Metadata of the committed (or deduplicated) blob.
        blob: BlobInfo,
    },
    /// Resumable progress of one upload.
    UploadStatus {
        /// Received bytes and commit state.
        upload: UploadState,
    },
    /// A blob stream follows as binary `blob_chunk` frames.
    Blob {
        /// Metadata and the binary stream id to collect.
        download: BlobDownload,
    },
    /// Stored CLIProxyAPI profiles, without secrets.
    ProxyProfiles {
        /// Every profile, default first.
        profiles: Vec<crate::proxy::ProxyProfile>,
    },
    /// One profile was added or updated.
    ProxyProfile {
        /// The profile as clients may see it.
        profile: crate::proxy::ProxyProfile,
    },
    /// A profile capability test finished.
    ProxyTested {
        /// The capabilities and diagnostics.
        report: crate::proxy::ProxyTestReport,
    },
    /// One profile's model catalogue.
    ProxyModels {
        /// The catalogue.
        catalog: crate::proxy::ProxyModelCatalog,
    },
    /// One profile's credential health.
    ProxyLimits {
        /// The health snapshot.
        limits: crate::proxy::ProxyLimits,
    },
    /// Which sessions an override change restarted.
    ProxyApplied {
        /// Threads whose live harness session was dropped. Each starts a new
        /// one on its next message.
        restarted: Vec<crate::ThreadId>,
        /// The subset that had a running turn, which was interrupted.
        interrupted: Vec<crate::ThreadId>,
    },
    /// A failover candidate, or `None` when the proxy has no other model.
    ProxyFailover {
        /// The chosen candidate.
        choice: Option<crate::proxy::ProxyFailoverChoice>,
    },
    /// Configured forge accounts and per-forge capabilities.
    ForgeAccounts {
        /// Accounts on this daemon.
        accounts: Vec<crate::forge::ForgeAccount>,
        /// Capability rows, one per supported forge.
        capabilities: Vec<crate::forge::ForgeCapabilityEntry>,
        /// What Linear's issue tracker can do. Linear is not a forge and never
        /// appeared in `capabilities`, so it travels separately: a client
        /// built before this field simply ignores it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        linear: Option<crate::forge::ForgeCapabilities>,
    },
    /// A forge account was validated and stored.
    ForgeAccount {
        /// Public account metadata (never the token).
        account: crate::forge::ForgeAccount,
    },
    /// A started GitHub device authorization.
    ForgeDeviceFlow {
        /// Short code, verification URL and poll id.
        flow: crate::forge::ForgeDeviceFlow,
    },
    /// The state of one device authorization poll.
    ForgeDeviceFlowStatus {
        /// Pending, slow down, complete or terminal.
        status: crate::forge::ForgeDeviceFlowStatus,
    },
    /// Forge repositories detected in a workspace.
    ForgeRepos {
        /// Detected remotes, preferred remote first.
        repos: Vec<crate::forge::ForgeRemote>,
    },
    /// One page of pull requests.
    ForgePrs {
        /// Matching PRs.
        prs: Vec<crate::forge::ForgePrSummary>,
        /// Page number returned.
        page: u32,
        /// Another page exists.
        has_more: bool,
    },
    /// One pull request.
    ForgePr {
        /// Detail.
        pr: crate::forge::ForgePullRequest,
    },
    /// One page of a pull request diff.
    ForgeDiff {
        /// Files and pagination state.
        diff: crate::forge::ForgeDiffPage,
    },
    /// PR comments, review threads and reviews.
    ForgeComments {
        /// Conversation page.
        page: crate::forge::ForgeCommentPage,
    },
    /// PR checks and their aggregate state.
    ForgeChecks {
        /// Checks snapshot.
        checks: crate::forge::ForgeChecks,
    },
    /// A PR's commits.
    ForgeCommits {
        /// The commits, oldest first.
        commits: crate::forge::ForgeCommits,
    },
    /// A general comment was posted.
    ForgeCommentPosted {
        /// The new comment.
        comment: crate::forge::ForgeComment,
    },
    /// A review-thread reply was posted.
    ForgeReviewCommentPosted {
        /// The new review comment.
        comment: crate::forge::ForgeReviewComment,
    },
    /// A pull request was opened.
    ForgePrOpened {
        /// The new PR.
        pr: crate::forge::ForgePullRequest,
    },
    /// One page of forge issues.
    ForgeIssues {
        /// Matching issues.
        issues: Vec<crate::issues::ForgeIssueSummary>,
        /// Page number returned.
        page: u32,
        /// Another page exists.
        has_more: bool,
        /// The local checkout does not own the repository, so writes are off.
        read_only: bool,
    },
    /// One forge issue.
    ForgeIssue {
        /// Detail.
        issue: crate::issues::ForgeIssue,
        /// The local checkout does not own the repository, so writes are off.
        read_only: bool,
    },
    /// One forge issue's comments.
    ForgeIssueComments {
        /// Comments page.
        page: crate::issues::ForgeIssueCommentPage,
    },
    /// A comment was posted on a forge issue.
    ForgeIssueCommentPosted {
        /// The new comment.
        comment: crate::issues::ForgeIssueComment,
    },
    /// Connected Linear workspaces.
    LinearAccounts {
        /// Connections, oldest first.
        accounts: Vec<crate::issues::LinearAccount>,
    },
    /// Teams exposed by one Linear workspace.
    LinearTeams {
        /// Teams.
        teams: Vec<crate::issues::LinearTeam>,
    },
    /// The Linear team attached to each project.
    LinearProjectTeams {
        /// Mappings.
        teams: Vec<crate::issues::LinearProjectTeam>,
    },
    /// One page of Linear issues.
    LinearIssues {
        /// Matching issues.
        issues: Vec<crate::issues::LinearIssueSummary>,
        /// Cursor for the next page.
        next_cursor: Option<String>,
        /// The local checkout does not own the project, so writes are off.
        read_only: bool,
    },
    /// One Linear issue with its comments.
    LinearIssue {
        /// Detail.
        issue: crate::issues::LinearIssue,
        /// The issue's team is not attached to a project here, so writes are off.
        read_only: bool,
    },
    /// A comment was posted on a Linear issue.
    LinearIssueCommentPosted {
        /// The new comment.
        comment: crate::issues::LinearIssueComment,
    },
    /// One thread's linked issues, with their latest tracker state.
    ThreadIssues {
        /// Thread the links belong to.
        thread_id: crate::ThreadId,
        /// Linked issues, oldest link first.
        issues: Vec<crate::issues::ThreadIssue>,
    },
    /// One linked issue after a link change.
    ThreadIssue {
        /// The link and its latest state.
        issue: crate::issues::ThreadIssue,
    },
    /// The thread workspace's git state.
    GitStatus {
        /// Branch, upstream, counts and paths.
        status: crate::git_actions::GitThreadStatus,
    },
    /// A commit was created.
    GitCommitted {
        /// The commit, and the fresh status after it.
        result: crate::git_actions::GitCommitResult,
    },
    /// A branch was pushed.
    GitPushed {
        /// The branch, its upstream and the fresh status.
        result: crate::git_actions::GitPushResult,
    },
    /// A generated commit message. Empty when generation is off.
    GeneratedCommitMessage {
        /// The message, or an empty string when the mode is Off.
        message: String,
    },
    /// Generated pull request text. Empty when generation is off.
    GeneratedPullRequestText {
        /// One-line title.
        title: String,
        /// Markdown body.
        body: String,
    },
    /// A PR context block was handed to a thread.
    ForgeHandoff {
        /// Thread, turn and the exact markdown sent.
        handoff: crate::forge::ForgeHandoff,
    },
    /// One thread's linked pull requests, with their latest forge state.
    ThreadPullRequests {
        /// Thread the links belong to.
        thread_id: crate::ThreadId,
        /// Linked PRs, oldest link first.
        pulls: Vec<crate::forge::ThreadPullRequest>,
    },
    /// One linked pull request after a link or watch change.
    ThreadPullRequest {
        /// The link and its latest state.
        pull: crate::forge::ThreadPullRequest,
    },
    /// Threads that link to one pull request.
    PrThreads {
        /// Repository.
        repo: crate::forge::ForgeRepoRef,
        /// PR number.
        number: u32,
        /// Linking threads.
        threads: Vec<crate::forge::PrLinkedThread>,
    },
    /// MCP servers known to this daemon, with the status of each (ADR-006).
    McpServers {
        /// Servers in configuration order.
        servers: Vec<crate::mcp::McpServerView>,
    },
    /// One MCP server after a mutation.
    McpServer {
        /// Current view.
        server: crate::mcp::McpServerView,
    },
    /// Preview of a harness's native MCP configuration.
    McpImportPreview {
        /// Entries and conflicts; nothing was written.
        preview: crate::mcp::McpImportPreview,
    },
    /// Tools one server exposes through the hub.
    McpTools {
        /// Server name.
        server: String,
        /// Qualified tools.
        tools: Vec<crate::mcp::McpToolInfo>,
    },
    /// Result of one diagnostic tool call.
    McpCall {
        /// Flattened result.
        result: crate::mcp::McpTestCallResult,
    },
    /// Remote tool calls waiting for the owner's decision (ADR-037).
    McpApprovals {
        /// Pending requests, oldest first.
        approvals: Vec<crate::mcp::McpShareApproval>,
    },
    /// Outside agents connected through OAuth, and the URL they use.
    OutsideAgents {
        /// Connected agents, newest first.
        agents: Vec<crate::mcp::OutsideAgentView>,
        /// Streamable-HTTP MCP endpoint an outside agent connects to.
        mcp_url: String,
    },
    /// h4rn's own tools and their per-harness switches.
    AgentTools {
        /// Current settings.
        settings: crate::mcp::AgentToolSettings,
    },
    /// Outside-agent sign-ins waiting for the person's decision.
    OutsideAgentSignIns {
        /// Waiting sign-ins, oldest first.
        sign_ins: Vec<crate::mcp::OutsideAgentSignIn>,
    },
    /// Aggregated usage from the durable ledger.
    UsageReport {
        /// The report.
        report: crate::usage::UsageReport,
    },
    /// Known limits and the auto-resume policy.
    Limits {
        /// Limits and policy.
        report: crate::usage::LimitsReport,
    },
    /// Persisted schedules and limit history.
    Resets {
        /// Schedules and history.
        resets: crate::usage::Resets,
    },
    /// Where a limit-hit thread could fail over to.
    FailoverOptions {
        /// The default and the alternatives.
        options: crate::usage::FailoverOptions,
    },
    /// Devices awaiting short-authentication-string confirmation.
    PairingConfirmations {
        /// Newest first. An entry stays until the host confirms or rejects it.
        confirmations: Vec<crate::connect::PairingConfirmation>,
    },
    /// The daemon's dedicated network listener and what a device would dial.
    NetworkAccess {
        /// Whether the persisted setting is on. A bind failure leaves this
        /// true and reports `error`.
        enabled: bool,
        /// Whether a socket is bound and serving right now.
        listening: bool,
        /// Port the listener uses: the bound port, or the configured one while
        /// it is off.
        port: u16,
        /// `host:port` candidates, most preferred first. Empty while the
        /// listener is off.
        addresses: Vec<String>,
        /// Why the listener could not bind, when it could not.
        error: Option<String>,
    },
    /// Devices paired directly with this computer.
    DirectDevices {
        /// Most recently paired first.
        devices: Vec<DirectDevice>,
    },
    /// A one-time direct pairing QR and its advertised candidates.
    DirectPairingOffer {
        /// `h4rn://pair?...` payload to render as a QR.
        uri: String,
        /// Expiry, Unix seconds.
        #[ts(type = "number")]
        expires_at: i64,
        /// `host:port` candidates the payload carries.
        addresses: Vec<String>,
        /// Scopes the paired device will hold.
        scopes: Vec<crate::connect::PairScope>,
    },
    /// A schedule was armed, moved or cleared.
    ResumeChanged {
        /// The current schedule, or `None` when none remains.
        schedule: Option<crate::usage::ResumeSchedule>,
    },
    /// Subagent children of one parent (or the whole daemon).
    Subagents {
        /// Matching subagents, newest first.
        subagents: Vec<SubagentView>,
    },
    /// One subagent after a spawn or a cancel.
    Subagent {
        /// Current view.
        subagent: SubagentView,
    },
    /// Harness CLIs on this host and their available updates.
    HarnessVersions {
        /// One row per harness, in stable order.
        harnesses: Vec<crate::HarnessVersionInfo>,
    },
    /// Local ACP agents configured on this daemon.
    AcpAgents {
        /// Configured agents, in creation order.
        agents: Vec<crate::AcpAgent>,
    },
    /// One configured ACP agent after an add or update.
    AcpAgent {
        /// The stored agent with its installation facts.
        agent: crate::AcpAgent,
    },
    /// One configured ACP agent's writable configuration.
    ///
    /// Only the admin-scoped get request returns this shape, because `env`
    /// carries real values.
    AcpAgentSpec {
        /// Configuration exactly as stored.
        agent: crate::AcpAgentSpec,
    },
    /// What one configured ACP agent advertises.
    AcpAgentProbe {
        /// The probe result.
        probe: crate::AcpAgentProbe,
    },
    /// Tracked git worktrees.
    Worktrees {
        /// Active first, then orphaned, then settled; newest first within each.
        worktrees: Vec<crate::TrackedWorktree>,
    },
    /// The outcome of [`crate::ClientRequest::ClearWorktrees`].
    WorktreesCleared {
        /// Paths removed.
        removed: Vec<String>,
        /// Paths left in place, with reasons.
        skipped: Vec<crate::SkippedWorktree>,
        /// The fresh list after clearing.
        worktrees: Vec<crate::TrackedWorktree>,
    },
    /// Registered projects on one host, oldest first.
    Projects {
        /// Projects.
        projects: Vec<crate::projects::Project>,
    },
    /// One project after it was added or renamed.
    Project {
        /// The project.
        project: crate::projects::Project,
    },
    /// Folders inside one directory.
    Directories {
        /// The listing.
        listing: crate::projects::DirectoryListing,
    },
    /// Configured text generators. Keys are never included.
    TextGenerators {
        /// Generators, oldest first.
        generators: Vec<crate::text_generation::TextGenerator>,
    },
    /// One text generator after it was added or changed.
    TextGenerator {
        /// The generator.
        generator: crate::text_generation::TextGenerator,
    },
    /// The outcome of [`crate::ClientRequest::TestTextGenerator`].
    TextGeneratorTested {
        /// What the tiny prompt did.
        report: crate::text_generation::TextGeneratorTestReport,
    },
    /// Which model writes thread titles.
    TextGenerationSettings {
        /// Current settings.
        settings: crate::text_generation::TextGenerationSettings,
    },
    /// Every scheduled task on this daemon, oldest first.
    Schedules {
        /// Stored tasks.
        tasks: Vec<crate::ScheduleTask>,
    },
    /// One scheduled task after a create, edit, enable/disable, run or rotate.
    ScheduleTask {
        /// The task's current state.
        task: crate::ScheduleTask,
    },
    /// One task's run history, newest first.
    ScheduleRuns {
        /// Recorded runs, at most the daemon's history bound.
        runs: Vec<crate::ScheduleRun>,
    },
    /// A webhook task's secret URL path, revealed to an admin.
    ScheduleWebhookUrl {
        /// The task the path belongs to.
        task_id: crate::ScheduleTaskId,
        /// The path to post to, absent when the task has no webhook URL.
        path: Option<String>,
    },
    /// A thread row after the owner renamed it.
    ThreadRenamed {
        /// The thread with its new title.
        summary: ThreadSummary,
    },
    /// A subscription was accepted and the replay it asked for is finished.
    ///
    /// Every journal event at or below `replay_through` was either sent as
    /// replay or was already at or below the client's `since_seq`; anything
    /// above it is live. A client uses this boundary instead of guessing from
    /// arrival order, because events can arrive between `Hello` and this
    /// answer while still being brand new.
    Subscribed {
        /// Journal head at the moment the subscription was served.
        #[ts(type = "number")]
        replay_through: u64,
    },
    /// A request with no payload completed successfully.
    Ack,
}

/// A protocol-level failure.
///
/// `hint` is written for the user and says what to do next; the UI may render
/// it verbatim under the error. Never put a secret in `message` or `hint`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ProtoError {
    /// Stable machine-readable error class.
    pub code: ProtoErrorCode,
    /// What happened.
    pub message: String,
    /// What to do about it.
    pub hint: Option<String>,
}

impl ProtoError {
    /// Builds an error with a user-facing hint.
    pub fn new(code: ProtoErrorCode, message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: Some(hint.into()),
        }
    }

    /// Builds an error without a hint.
    pub fn plain(code: ProtoErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: None,
        }
    }

    /// The standard response for a request whose phase has not landed.
    pub fn unsupported(feature: &str, phase: &str) -> Self {
        Self::new(
            ProtoErrorCode::Unsupported,
            format!("{feature} is not implemented yet"),
            format!("Upgrade the daemon when {phase} ships, or use the CLI"),
        )
    }

    /// The standard response for a request this daemon build does not implement.
    pub fn not_implemented(feature: &str) -> Self {
        Self::new(
            ProtoErrorCode::NotImplemented,
            format!("{feature} is not implemented in this daemon"),
            format!("Upgrade the daemon when {feature} ships; no state was changed"),
        )
    }
}

/// Machine-readable protocol error classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ProtoErrorCode {
    /// The frame did not match the contract.
    InvalidRequest,
    /// The device is not paired or its token expired.
    Unauthorized,
    /// The device is paired but lacks the required scope.
    Forbidden,
    /// The thread, host or resource does not exist.
    NotFound,
    /// The request conflicts with current state.
    Conflict,
    /// The feature has not shipped in this daemon at all.
    Unsupported,
    /// The feature is part of the contract but not implemented by this build.
    ///
    /// Unlike [`Self::Unsupported`], this names a known phase gap rather than
    /// an unknown protocol version or capability.
    NotImplemented,
    /// The daemon or an upstream is rate limited.
    RateLimited,
    /// An internal failure; retryable at the client's discretion.
    Internal,
}

/// One ordered event.
///
/// Events are appends to a thread's log. The UI applies them in `seq` order;
/// unknown variants must be ignored so a newer daemon can talk to an older
/// client without a protocol break (ADR-003).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// The thread projection changed; upsert the row.
    ThreadUpserted(ThreadSummary),
    /// A turn started.
    TurnStarted {
        /// Thread.
        thread_id: ThreadId,
        /// Turn.
        turn_id: TurnId,
        /// Start time.
        started_at: DateTime<Utc>,
    },
    /// Streaming text from the agent.
    MessageDelta {
        /// Thread.
        thread_id: ThreadId,
        /// Turn.
        turn_id: TurnId,
        /// Stable item id for the message being streamed.
        item_id: String,
        /// Text chunk.
        text: String,
    },
    /// A stream item finished and can be committed to the transcript.
    ItemCompleted {
        /// Thread.
        thread_id: ThreadId,
        /// Turn the item belongs to.
        turn_id: TurnId,
        /// The completed item.
        item: Item,
    },
    /// A tool call changed state.
    ToolCall {
        /// Thread.
        thread_id: ThreadId,
        /// Turn.
        turn_id: TurnId,
        /// Current tool-call state.
        tool_call: ToolCall,
    },
    /// The agent published a plan, replacing any earlier one.
    Plan {
        /// Thread.
        thread_id: ThreadId,
        /// Turn the plan belongs to.
        turn_id: TurnId,
        /// Every step, in the agent's order.
        entries: Vec<crate::PlanStep>,
    },
    /// The harness is waiting for a decision.
    ApprovalRequested {
        /// Thread.
        thread_id: ThreadId,
        /// Turn.
        turn_id: TurnId,
        /// Opaque approval id.
        approval_id: String,
        /// Subagent that raised the hand, when it belongs to one.
        ///
        /// The approval is emitted on the parent thread with a namespaced
        /// approval id; this names the child so the UI can attribute it.
        #[serde(default)]
        subagent_id: Option<SubagentId>,
        /// What kind of hand is raised.
        kind: ApprovalKind,
        /// What the harness wants to do.
        prompt: String,
        /// Selectable answers, when the hand is a question.
        options: Vec<ApprovalOption>,
    },
    /// An approval or question was answered.
    ///
    /// The daemon emits this after the harness accepted the answer, so every
    /// client can resolve the cue from the stream instead of trusting the
    /// device that pressed Go/Hold.
    ApprovalResolved {
        /// Thread.
        thread_id: ThreadId,
        /// Approval that was answered.
        approval_id: String,
        /// Selected option id, when the hand was a question.
        option_id: Option<String>,
        /// Device that answered, when the daemon knows it.
        by_device: Option<DeviceId>,
    },
    /// A turn finished.
    TurnCompleted {
        /// Thread.
        thread_id: ThreadId,
        /// Turn.
        turn_id: TurnId,
        /// Usage for this turn.
        usage: Usage,
        /// How it finished.
        status: TurnStatus,
        /// Files this turn changed, when the daemon recorded snapshots.
        ///
        /// The stream renders its change-set card from this; older journals
        /// simply omit it and the card falls back to the Changes panel.
        #[serde(default)]
        #[ts(optional)]
        changes: Option<crate::fs::FsTurnChanges>,
    },
    /// The agent state changed; drives the orb or static glyph.
    AgentStateChanged {
        /// Thread.
        thread_id: ThreadId,
        /// New state.
        state: AgentState,
    },
    /// A usage limit was hit.
    UsageLimitHit {
        /// Thread.
        thread_id: ThreadId,
        /// When the provider resets, if known.
        reset_at: Option<DateTime<Utc>>,
        /// Whether the daemon has armed auto-resume.
        auto_resume: bool,
    },
    /// A daemon or harness error that the user should see.
    Error {
        /// Thread, when the error belongs to one.
        thread_id: Option<ThreadId>,
        /// What happened.
        message: String,
        /// What to do.
        hint: Option<String>,
        /// Whether a retry is safe.
        retryable: bool,
    },
    /// A watched path changed on disk.
    ///
    /// Filesystem events are **transient**: they are not written to the journal
    /// and carry the current durable sequence number instead of a new one, so a
    /// client must deliver them without advancing its resume cursor.
    FsChanged {
        /// Canonical workspace root.
        workspace: String,
        /// Workspace-relative path that changed.
        path: String,
        /// What happened.
        change: crate::fs::FsChangeKind,
        /// When the daemon observed it.
        at: DateTime<Utc>,
    },
    /// One streamed batch of workspace search results.
    ///
    /// Like [`Event::FsChanged`], this is transient and never journaled.
    FsSearchBatch {
        /// Correlation id from `FsSearchStarted`.
        search_id: String,
        /// Matches in this batch.
        results: Vec<crate::fs::FsSearchMatch>,
        /// True on the last batch.
        done: bool,
        /// True when the daemon hit its result cap.
        truncated: bool,
    },
    /// A watched pull request's checks changed.
    ///
    /// Produced by the daemon's backoff poller rather than by the forge's own
    /// webhooks, so it is transient: it carries the current durable sequence
    /// number and is never journaled. Clients refresh the checks tab when they
    /// see it.
    ForgeChecksUpdated {
        /// Repository.
        repo: crate::forge::ForgeRepoRef,
        /// PR number.
        number: u32,
        /// New snapshot, including the aggregate state.
        checks: crate::forge::ForgeChecks,
    },
    /// The skill registry or one installed copy changed.
    ///
    /// Transient like [`Event::FsChanged`]: a client re-lists instead of
    /// trying to replay skill state from the journal.
    SkillsChanged {
        /// Host whose skills changed.
        host: crate::HostId,
        /// When the daemon observed it.
        at: DateTime<Utc>,
    },
    /// The result of a background skill sync.
    ///
    /// Transient; the durable state is the registry itself. Clients render the
    /// status bar from this event and fall back to `SkillsList` on reconnect.
    SyncStatus(crate::skills::SkillSyncStatus),
    /// The client's multipath transport changed state (ADR-009).
    ///
    /// The daemon never emits this: the client owns path selection and
    /// synthesises the event locally so the status bar can show which path is
    /// carrying the session, its RTT, and that the session is encrypted. Like
    /// [`Event::FsChanged`], it is transient and carries no durable sequence.
    TransportStatus(crate::transport::TransportStatus),
    /// One MCP server changed runtime status (ADR-006).
    ///
    /// This is a **transient** event: the hub can start, fail and restart a
    /// server without a client turn, so it must not consume a durable
    /// sequence. A client reconnecting learns the current state from
    /// [`crate::ClientRequest::McpList`].
    McpServerStatus {
        /// Server name.
        server: String,
        /// New status.
        status: crate::mcp::McpServerStatusKind,
        /// Cached tool count.
        tool_count: u32,
        /// Redacted failure or restart reason.
        message: Option<String>,
        /// When the hub observed it.
        at: DateTime<Utc>,
    },
    /// One MCP tool call passed through the hub, for the live audit view.
    ///
    /// Transient for the same reason as [`Event::McpServerStatus`]. The
    /// daemon's durable `RemoteAudit` trail records the authoritative entry.
    McpToolCalled {
        /// Server namespace.
        server: String,
        /// Unqualified tool name.
        tool: String,
        /// Fully qualified name the model used.
        qualified_name: String,
        /// Who invoked it.
        caller: crate::mcp::McpCaller,
        /// Whether the call succeeded.
        ok: bool,
        /// Round trip in milliseconds.
        duration_ms: u64,
        /// Redacted failure message.
        error: Option<String>,
        /// When the call returned.
        at: DateTime<Utc>,
    },
    /// A shared MCP tool call is waiting for the owner's decision (ADR-037).
    ///
    /// Transient: the pending set is re-read with
    /// [`crate::mcp::McpRequest::PendingApprovals`] after a reconnect. The call
    /// fails closed when the approval expires or is cancelled.
    McpShareApprovalRequested {
        /// What the owner is asked to approve.
        approval: crate::mcp::McpShareApproval,
    },
    /// A pending shared MCP tool call was answered or expired.
    ///
    /// Transient for the same reason as
    /// [`Event::McpShareApprovalRequested`].
    McpShareApprovalResolved {
        /// Approval that is no longer pending.
        approval_id: String,
        /// What happened to it.
        decision: crate::ApprovalDecision,
    },
    /// A parent thread spawned a subagent (ADR-039).
    ///
    /// Durable: appended to the **parent's** log so a reconnecting client can
    /// rebuild the delegation tree even if the spawn response was lost.
    SubagentSpawned {
        /// Thread that delegated the work.
        parent_thread_id: ThreadId,
        /// The new child.
        subagent: SubagentView,
    },
    /// A subagent changed state or progress.
    ///
    /// Transient like [`Event::McpServerStatus`]: budget counters and orb state
    /// change without a client turn, so this carries the current durable
    /// sequence instead of consuming one.
    SubagentUpdated {
        /// Thread that delegated the work.
        parent_thread_id: ThreadId,
        /// Current view.
        subagent: SubagentView,
    },
    /// A subagent reached a terminal state.
    ///
    /// Durable, like [`Event::SubagentSpawned`], and written to the parent's
    /// log with the child thread id in the view.
    SubagentFinished {
        /// Thread that delegated the work.
        parent_thread_id: ThreadId,
        /// Final view, including the result.
        subagent: SubagentView,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::TerminalExit;

    #[test]
    fn event_serialises_with_internal_type_and_stable_names() {
        let event = Event::AgentStateChanged {
            thread_id: ThreadId::new("th_1"),
            state: AgentState::AwaitingApproval,
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["type"], "agent_state_changed");
        assert_eq!(value["state"], "awaiting_approval");
        assert_eq!(value["thread_id"], "th_1");
    }

    #[test]
    fn server_frame_tags_are_stable() {
        let frame = ServerFrame::Event {
            seq: 4,
            event: Event::UsageLimitHit {
                thread_id: ThreadId::new("th_1"),
                reset_at: None,
                auto_resume: true,
            },
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains("\"kind\":\"event\""));
        assert!(json.contains("\"type\":\"usage_limit_hit\""));
    }

    #[test]
    fn terminal_frames_have_stable_tags() {
        let frame = ServerFrame::TerminalEvent {
            event: TerminalExitNotice {
                terminal_id: crate::TerminalId::new("term_1"),
                thread_id: None,
                exit: TerminalExit {
                    status: 0,
                    signal: None,
                    at: Utc::now(),
                },
            },
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains("\"kind\":\"terminal_event\""), "{json}");
        assert!(json.contains("\"terminal_id\":\"term_1\""), "{json}");

        let response = ResponseBody::TerminalAttached {
            terminal: TerminalInfo {
                terminal_id: crate::TerminalId::new("term_1"),
                stream_id: 1,
                thread_id: None,
                host: crate::HostId::new("host_atlas"),
                cwd: "/tmp".into(),
                shell: "/bin/sh".into(),
                cols: 80,
                rows: 24,
                alive: true,
                viewers: 1,
                exit: None,
            },
            snapshot: "hello".into(),
            start_seq: 3,
        };
        let value = serde_json::to_value(ServerFrame::Response {
            id: 1,
            result: Ok(response),
        })
        .unwrap();
        assert_eq!(value["result"]["Ok"]["type"], "terminal_attached");
        assert_eq!(value["result"]["Ok"]["start_seq"], 3);
    }

    #[test]
    fn hello_carries_the_daemons_machine_and_tolerates_an_older_daemon() {
        let value = serde_json::json!({
            "type": "hello",
            "protocol_version": 1,
            "server_version": "1.0.1",
            "device_id": "dev_1",
            "daemon_machine_id": "m_atlas",
            "local_host": "host_atlas",
            "hosts": [],
            "daemon_timezone": "Europe/Berlin",
        });
        match serde_json::from_value::<ResponseBody>(value.clone()).unwrap() {
            ResponseBody::Hello {
                daemon_machine_id,
                daemon_timezone,
                ..
            } => {
                assert_eq!(daemon_machine_id.as_deref(), Some("m_atlas"));
                assert_eq!(daemon_timezone, "Europe/Berlin");
            }
            other => panic!("unexpected body: {other:?}"),
        }

        // A daemon older than these fields answers without them. Their absence
        // must not fail the handshake; the client falls back to the local host
        // name and its own zone.
        let mut older = value;
        older.as_object_mut().unwrap().remove("daemon_machine_id");
        older.as_object_mut().unwrap().remove("daemon_timezone");
        match serde_json::from_value::<ResponseBody>(older).unwrap() {
            ResponseBody::Hello {
                daemon_machine_id,
                daemon_timezone,
                ..
            } => {
                assert_eq!(daemon_machine_id, None);
                assert_eq!(daemon_timezone, "");
            }
            other => panic!("unexpected body: {other:?}"),
        }
    }

    #[test]
    fn response_errors_keep_the_hint() {
        let error = ProtoError::new(
            ProtoErrorCode::Unsupported,
            "terminal is not implemented yet",
            "Upgrade the daemon when phase 2 ships",
        );
        assert_eq!(error.code, ProtoErrorCode::Unsupported);
        assert!(error.hint.unwrap().contains("phase 2"));
    }
}
