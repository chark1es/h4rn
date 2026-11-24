//! The h4rn wire contract.
//!
//! `h4rn-protocol` owns every type that crosses the WebSocket between a client
//! (desktop, mobile or web) and the daemon. JSON frames carry commands, events
//! and control messages; binary frames carry PTY output and blob chunks
//! (ADR-001, ADR-002, ADR-007).
//!
//! TypeScript bindings are generated from these types by `ts-rs` and checked
//! into `apps/ui/src/gen`. The `export_typescript_bindings` test is the single
//! writer of that directory; `scripts/check-bindings.sh` fails when it drifts.
//!
//! Conventions:
//! - JSON enums are internally tagged (`type` or `kind`) with snake_case
//!   discriminants so the generated TypeScript is a stable union.
//! - Timestamps are RFC 3339 strings (`chrono::DateTime<Utc>`).
//! - 64-bit counters are serialised as JSON numbers and typed as `number` in
//!   TypeScript. Every value h4rn counts (tokens, cost, sequence) is far below
//!   `Number.MAX_SAFE_INTEGER`.
//!
//! # Resume sequence (ADR-003)
//!
//! [`ServerFrame::Event::seq`] is a durable, daemon-global, monotonically
//! increasing sequence number shared by every thread. It is allocated by the
//! SQLite journal and survives a daemon restart. Clients persist the last value
//! they applied and send it back as [`ClientRequest::Subscribe::since_seq`]
//! (global replay) or [`ClientRequest::GetThread::since_seq`] (one thread), so a
//! reconnect can never skip an event.

#![deny(missing_docs)]

mod acp_agent;
mod binary;
mod blob;
mod client;
mod common;
pub mod connect;
pub mod forge;
mod fs;
mod git_actions;
mod guest;
mod harness;
mod ids;
pub mod import;
pub mod issues;
pub mod mcp;
pub mod projects;
mod proxy;
mod schedules;
mod server;
pub mod skills;
mod subagents;
mod terminal;
mod text_generation;
mod transport;
mod usage;
mod worktrees;

pub use acp_agent::{
    AcpAgent, AcpAgentMode, AcpAgentModel, AcpAgentProbe, AcpAgentSpec, AcpWorkingDir,
};
pub use binary::{
    BINARY_HEADER_LEN, BINARY_MAGIC, BINARY_VERSION, BinaryFrame, BinaryFrameError, BinaryKind,
    PTY_INPUT_ORDINAL_BITS, PTY_INPUT_ORDINAL_MAX, PTY_INPUT_ORDINAL_SHIFT,
    PTY_INPUT_POSITION_BITS, PTY_INPUT_POSITION_MAX, PTY_INPUT_POSITION_SHIFT, PTY_INPUT_REPLY,
    pty_input_reply_ordinal, pty_input_reply_position, pty_input_reply_seq,
};
pub use blob::{Attachment, BlobDownload, BlobInfo, BlobRange, UploadState, UploadTicket};
pub use client::{ClientFrame, ClientRequest};
pub use common::{
    AgentState, ApprovalDecision, ApprovalKind, ApprovalOption, DeviceInfo, HarnessKind,
    HostSummary, Item, MatchUnit, MessageRole, PlanStep, RuntimeMode, ThreadFilter, ThreadSummary,
    ThreadTab, ToolCall, ToolCallStatus, ToolStats, Topic, TurnStatus, Usage, WorktreeRequest,
};
pub use connect::*;
pub use forge::*;
pub use fs::{
    DiffAgainst, FileEntry, FileEntryKind, FsBranchList, FsChangeKind, FsCreateKind,
    FsDeleteResult, FsDiff, FsDiffCommit, FsDiffCommitList, FsDiffFile, FsDiffFiles, FsFileContent,
    FsListing, FsOmitted, FsSearchBatch, FsSearchMatch, FsTurnChanges, FsTurnFile, FsWriteConflict,
    FsWriteResult, GitFileStatus,
};
pub use git_actions::{
    GitChangeSet, GitCommitResult, GitOperation, GitPushResult, GitThreadStatus, MAX_GIT_PATHS,
};
pub use guest::{
    GUEST_CHUNK_BYTES, GUEST_COOKIE, GUEST_LINK_TTL_HOURS, GUEST_MAX_EXCHANGE_BYTES,
    GUEST_MAX_FILES, GUEST_MAX_MESSAGE_BYTES, GUEST_MAX_PENDING_BYTES, GUEST_MAX_PENDING_REQUESTS,
    GUEST_MAX_TEXT_BYTES, GUEST_PAGE_HTML, GuestAttachment, GuestAttachmentInput, GuestLink,
    GuestLinkState, GuestMessageInput, GuestRequest, GuestSettings, GuestSharing, GuestSnapshot,
    GuestThreadView, guest_page_csp, guest_page_html, redact_guest_path,
};
pub use harness::{
    HarnessOutputStream, HarnessUpdateEvent, HarnessUpdateMethod, HarnessVersionInfo,
};
pub use ids::{
    AttachmentId, AttachmentIdError, DeviceId, HostId, ScheduleTaskId, SubagentId, TerminalId,
    ThreadId, TurnId,
};
pub use import::{
    ImportEvent, ImportProgress, ImportProject, ImportScan, ImportSource, ImportedSource,
    path_source_key, source_key,
};
pub use issues::*;
pub use mcp::{
    AgentToolSettings, McpCaller, McpConflictResolution, McpEnabledMatrix, McpHarnessToggle,
    McpImportCandidate, McpImportConflict, McpImportPreview, McpMachineToggle, McpRequest,
    McpSecretInput, McpServerInput, McpServerStatusKind, McpServerView, McpShareApproval,
    McpSharePolicy, McpShareView, McpTestCallResult, McpToolInfo, McpTransportInput,
    McpTransportView, OutsideAgentLevel, OutsideAgentSettings, OutsideAgentSignIn,
    OutsideAgentView,
};
pub use projects::{DirectoryEntry, DirectoryListing, Project, ProjectSource};
pub use proxy::*;
pub use schedules::{
    IntervalUnit, MAX_WEBHOOK_BODY_BYTES, MAX_WEBHOOK_PLACEHOLDERS, ScheduleDestination,
    ScheduleDraft, ScheduleEvent, ScheduleRun, ScheduleRunOutcome, ScheduleSpec, ScheduleTask,
    WEBHOOK_RATE_LIMIT_PER_MINUTE, parse_once_date, parse_schedule_time,
};
pub use server::{
    DirectDevice, Event, ProtoError, ProtoErrorCode, ResponseBody, SeqEvent, ServerFrame,
};
pub use skills::{
    SkillBundleFile, SkillConflict, SkillConflictResolution, SkillDetail, SkillDiff, SkillDiffFile,
    SkillFile, SkillFileChange, SkillHarnessAvailability, SkillImportCandidate, SkillImportPreview,
    SkillInstallState, SkillInventory, SkillManifest, SkillSource, SkillSummary, SkillSyncAck,
    SkillSyncConflict, SkillSyncEntry, SkillSyncPull, SkillSyncPullResponse, SkillSyncPush,
    SkillSyncPushResponse, SkillSyncState, SkillSyncStatus, SkillVersionVector,
};
pub use subagents::{
    DEFAULT_MAX_DEPTH, DEFAULT_MAX_TOKENS, DEFAULT_MAX_TURNS, DEFAULT_MAX_WALL_TIME_SECS,
    SpawnSubagentRequest, SubagentBudget, SubagentContext, SubagentContextKind, SubagentCredential,
    SubagentFileChange, SubagentIsolation, SubagentProgress, SubagentRequest, SubagentResult,
    SubagentState, SubagentView,
};
pub use terminal::{
    CREDIT_MAX_BYTES, CREDIT_MAX_CHUNKS, TerminalExit, TerminalExitNotice, TerminalInfo,
};
pub use text_generation::{
    MAX_SOURCE_CONTROL_INSTRUCTIONS, SourceControlModelMode, SourceControlModelSetting,
    TextGenerationSettings, TextGenerationSettingsUpdate, TextGenerator, TextGeneratorDraft,
    TextGeneratorId, TextGeneratorKind, TextGeneratorPatch, TextGeneratorTestReport,
    TitleModelMode, TitleModelSetting,
};
pub use transport::{
    TransportPathKind, TransportPathState, TransportPathStatus, TransportSessionState,
    TransportStatus,
};
pub use usage::*;
pub use worktrees::{
    DEFAULT_BRANCH_PREFIX, SkippedWorktree, TrackedWorktree, WorktreeAccess, WorktreeOrigin,
    WorktreeSettings, WorktreeStatus,
};

/// Version of the wire protocol implemented by this crate.
///
/// The client sends this in [`ClientRequest::Hello`]; a daemon rejects an
/// incompatible major version with [`ProtoErrorCode::Unsupported`].
pub const PROTOCOL_VERSION: u32 = 1;
