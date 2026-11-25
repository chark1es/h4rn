//! Durable daemon journal.
//!
//! `h4rn-core` owns the domain event log (lifecycle events) and the per-thread
//! sequence numbers of ADR-003. The daemon additionally stores the **projected
//! wire events** so a reconnecting client can replay exactly what it missed,
//! including streamed `MessageDelta` frames that are not part of the lifecycle
//! state machine. Both tables live in the same SQLite file and therefore in the
//! same cache the relay will eventually mirror.
//!
//! Sequence numbers here are daemon-global and strictly increasing, which is
//! what a client subscribed to several threads needs. `GetThread` replays by
//! that global sequence and filters one thread.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use h4rn_core::{EventStore, SqliteEventStore, StoreError, ThreadEvent};
use h4rn_protocol::{
    Event, ForgeKind, ForgeRepoRef, FsTurnChanges, HarnessKind, HostId, Item, RuntimeMode,
    ScheduleDestination, ScheduleRun, ScheduleRunOutcome, ScheduleSpec, ScheduleTask,
    ScheduleTaskId, SubagentId, ThreadId, Usage, WorktreeRequest,
};
use serde_json::json;
use sqlx::{Row, SqlitePool};

use crate::issue_links::IssueLinkRecord;
use crate::pr_links::PrLinkRecord;

/// Where a thread's current title came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TitleSource {
    /// The first prompt line, or the workspace folder name.
    #[default]
    Prompt,
    /// Written by the configured title model.
    Generated,
    /// Typed by the person; never replaced automatically.
    Renamed,
}

impl TitleSource {
    /// The stored spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::Generated => "generated",
            Self::Renamed => "renamed",
        }
    }

    /// Reads a stored spelling, defaulting to the prompt-derived title.
    pub fn parse(value: &str) -> Self {
        match value {
            "generated" => Self::Generated,
            "renamed" => Self::Renamed,
            _ => Self::Prompt,
        }
    }
}

/// Everything the daemon needs to rebuild a thread row without replaying it.
#[derive(Clone, Debug, PartialEq)]
pub struct ThreadMeta {
    /// Thread id.
    pub thread_id: ThreadId,
    /// Thread that delegated this one, when this is a subagent child.
    pub parent_thread_id: Option<ThreadId>,
    /// Subagent record this child belongs to, when it is one.
    pub subagent_id: Option<SubagentId>,
    /// Host running the harness.
    pub host: HostId,
    /// Harness adapter.
    pub harness: HarnessKind,
    /// Configured ACP agent instance, when the harness is generic ACP.
    pub agent_id: Option<String>,
    /// Human title, derived from the first prompt.
    pub title: String,
    /// Where the current title came from.
    ///
    /// A generated title only replaces a prompt-derived one, never a name the
    /// person typed, and the origin survives a daemon restart.
    pub title_source: TitleSource,
    /// Absolute workspace path.
    pub workspace_path: String,
    /// Model slug.
    pub model: String,
    /// Permission autonomy chosen at creation.
    pub runtime_mode: RuntimeMode,
    /// Reasoning effort in the harness's own vocabulary, when one was chosen.
    ///
    /// Absent in rows written before thread controls; those threads keep the
    /// harness default.
    pub reasoning_effort: Option<String>,
    /// Whether the thread asks for the harness's fast tier.
    pub fast: bool,
    /// Worktree intent recorded at creation.
    pub worktree: Option<WorktreeRequest>,
    /// Creation time, used as the initial lifecycle clock.
    pub created_at: DateTime<Utc>,
    /// Cumulative usage.
    pub usage: Usage,
    /// Harness-native session id for resume.
    pub resume_id: Option<String>,
    /// Fingerprint of the configured ACP command line `resume_id` belongs to.
    ///
    /// A different value means the stored session id describes a command the
    /// thread no longer runs, so the next start opens a fresh session.
    pub agent_fingerprint: Option<String>,
    /// A prompt a paused thread still has to deliver.
    ///
    /// A failed write puts its prompt back and the typed limit pauses the
    /// thread; the schedule is what delivers it. Without this record a daemon
    /// restart would forget the prompt and fire the policy's resume prompt
    /// instead, so the person's message would silently turn into "continue".
    pub parked: Option<ParkedPrompt>,
    /// Messages waiting for the next turn, oldest first.
    ///
    /// Stored with the row so the queue survives a daemon restart in its order,
    /// and so does the ceiling or outside grant each message carries.
    pub queued: Vec<StoredQueuedMessage>,
    /// Messages taken from the queue for a steer into a running turn.
    ///
    /// A claim keeps its payload until delivery is known, so a crash before the
    /// adapter took it, or a refused message whose restore write failed, is
    /// never lost. A delivered claim is only remembered to prove it must not be
    /// sent again.
    pub claimed: Vec<QueuedClaim>,
    /// The saved conversation this thread was imported from, when it was.
    ///
    /// An imported thread starts with no live session. The kept messages, with
    /// their timestamps, are what its first new turn hands to a fresh session
    /// (or a native resume, when the harness can take one).
    pub imported: Option<ImportedOrigin>,
    /// The thread and turn this thread was forked from, when it was.
    ///
    /// A fork is an ordinary thread with its own session, queue, approvals and
    /// schedules; only the lineage is recorded here. Nothing cascades from the
    /// source, so deleting or archiving it never affects the fork.
    pub forked_from: Option<ForkOrigin>,
}

/// The source of a forked thread.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ForkOrigin {
    /// Thread the transcript was copied from.
    pub thread_id: ThreadId,
    /// Last turn copied, inclusive.
    pub turn_id: h4rn_protocol::TurnId,
    /// The source's sidebar grouping, when it differed from its workspace.
    #[serde(default)]
    pub project_path: Option<String>,
    /// Whether the fork still has one native fork to try on its first start.
    ///
    /// Cleared when a session reports `SessionStarted`, so a later restart
    /// never forks the source again (which would lose turns made in the fork).
    pub native_fork: bool,
    /// The provider's own turn id the fork copies through, inclusive.
    ///
    /// Set only where the adapter can bound a native fork by turn (Codex's
    /// `thread/fork` with `lastTurnId`). It is what keeps the fork at the chosen
    /// turn when the source has moved on since the fork was made.
    #[serde(default)]
    pub provider_turn_id: Option<String>,
}

/// One message waiting for a thread's next turn, with the identity of the work
/// it does.
///
/// The queue outlives the request that filled it, so this stores the sender's
/// permission ceiling and, for an outside grant, the token family that asked.
/// The family is daemon-internal: a client only ever sees the
/// [`h4rn_protocol::QueuedMessage`] half, which the flatten keeps stored rows
/// shaped like the wire message.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StoredQueuedMessage {
    /// The message as a client sees it.
    #[serde(flatten)]
    pub message: h4rn_protocol::QueuedMessage,
    /// The permission ceiling the sender attached to this message, if any.
    ///
    /// It survives a restart with the message, so a supervised prompt is not
    /// promoted at a more permissive mode just because the daemon restarted.
    #[serde(default)]
    pub ceiling: Option<h4rn_protocol::RuntimeMode>,
    /// The outside grant whose work this message does, when one sent it.
    ///
    /// Re-checked when the message is delivered, so work asked for under a
    /// family that was revoked while it waited never runs.
    #[serde(default)]
    pub grant: Option<h4rn_protocol::WorkGrant>,
}

/// One message taken from the queue to steer into a running turn.
///
/// The claim is written before the adapter sees the text and removed only once
/// the outcome is known: a message that never reached the turn goes back to the
/// queue at startup, one the adapter definitely refused is restored, and one it
/// took (or may have taken) is never sent a second time.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QueuedClaim {
    /// The message exactly as it waited in the queue, with its ceiling and grant.
    pub message: StoredQueuedMessage,
    /// The queue position it waited in, so a restore puts it back where it was.
    pub index: u32,
    /// The turn it was claimed for.
    pub turn_id: h4rn_protocol::TurnId,
    /// What is known about its delivery.
    pub state: QueuedClaimState,
    /// Whether this claim is the turn's own promoted prompt, not a steer.
    ///
    /// A promoted prompt's claim is what recovery uses when the daemon stops
    /// between removing the queue row and handing the text to the adapter. A
    /// steer's claim is kept for its message alone, so the two are told apart:
    /// settling a turn drops its promoted claim but never an unclear steer's.
    #[serde(default)]
    pub promoted: bool,
}

/// What a [`QueuedClaim`] knows about its message's delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueuedClaimState {
    /// Written down, but the adapter has not been called yet.
    ///
    /// On startup this is known not to have reached the turn, so it goes back
    /// to the queue rather than being dropped.
    Claimed,
    /// The adapter call has started, so delivery is unknown.
    ///
    /// A restart keeps the payload and shows it as a row that may have been
    /// delivered. It is never sent automatically again, and the person can
    /// resend or discard it.
    Sending,
    /// The adapter took the message.
    ///
    /// A restart never delivers it again.
    Delivered,
    /// The adapter definitely refused it and its queue row still has to be put
    /// back. A startup restores it.
    Refused,
}

/// The saved conversation a thread was imported from.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ImportedOrigin {
    /// Which harness saved it.
    pub source: h4rn_protocol::ImportSource,
    /// Transcript file it came from (read-only).
    pub source_path: String,
    /// Harness-native session id, when the transcript records one.
    pub session_id: Option<String>,
    /// The checkout the conversation belongs to, when it differed from the
    /// working directory it ran in (a linked worktree or a subdirectory).
    ///
    /// The working directory stays what a native resume needs; this is only
    /// the sidebar's grouping, saved so it survives a restart.
    #[serde(default)]
    pub project_path: Option<String>,
    /// Whether the thread's one native resume has not been spent yet.
    ///
    /// Cleared when the first session starts: after that the thread has a live
    /// conversation of its own, and its handoff must not replay the imported
    /// messages over the turns made in h4rn.
    pub native_resume: bool,
}

/// One message of a thread's visible history.
///
/// Read back from the journaled items when a handoff needs it, so the imported
/// origin does not hold a second copy of the transcript.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ImportedMessage {
    /// Who authored it.
    pub role: h4rn_protocol::MessageRole,
    /// Text, already redacted before it reached the journal.
    pub text: String,
    /// When it was written in the source conversation.
    pub at: DateTime<Utc>,
}

/// One visible record of a journaled turn, in the order it was shown.
///
/// The transcript is not only messages: a tool call, a plan, an error row and
/// a usage-limit row are all durable records a fork must carry, and an
/// assistant reply that was still streaming when the daemon stopped exists
/// only as deltas.
#[derive(Clone, Debug, PartialEq)]
pub enum JournaledItem {
    /// A completed stream item: a message with its attachments, a tool call or
    /// a delegated row.
    Item {
        /// The item exactly as the transcript showed it.
        item: Item,
        /// When it was authored in the conversation it came from.
        at: DateTime<Utc>,
    },
    /// A tool call's own update, when the harness journaled it that way.
    ToolCall {
        /// The call's latest state at that point.
        tool_call: h4rn_protocol::ToolCall,
    },
    /// The turn's plan, as last published.
    Plan {
        /// Every step, in the agent's order.
        entries: Vec<h4rn_protocol::PlanStep>,
    },
    /// A failure the turn showed as its own row.
    Error {
        /// What happened.
        message: String,
        /// What to do about it.
        hint: Option<String>,
        /// Whether a retry is safe.
        retryable: bool,
        /// When it was shown.
        at: DateTime<Utc>,
    },
    /// A usage-limit row.
    Limit {
        /// When the provider resets, if the row knew.
        reset_at: Option<DateTime<Utc>>,
        /// Whether auto-resume was armed.
        auto_resume: bool,
        /// When it was shown.
        at: DateTime<Utc>,
    },
}

/// One turn as the journal recorded it, in first-seen order.
#[derive(Clone, Debug, PartialEq)]
pub struct JournaledTurn {
    /// The turn's id.
    pub turn_id: h4rn_protocol::TurnId,
    /// When the turn started, when the journal recorded it.
    pub started_at: Option<DateTime<Utc>>,
    /// Every visible record, oldest first.
    pub items: Vec<JournaledItem>,
    /// The journal sequence of each entry in `items`, in the same order.
    ///
    /// A queued prompt's row is written when the message arrives, long before
    /// the turn it belongs to starts, so the sequence — not the turn list — is
    /// what keeps a copy in the order the source showed.
    pub item_seqs: Vec<u64>,
    /// The journal sequence of the turn's own `TurnStarted`, when recorded.
    pub started_seq: Option<u64>,
    /// The journal sequence of its `TurnCompleted`, when recorded.
    pub completed_seq: Option<u64>,
    /// Whether an older journal's queued prompt was shown before this turn and
    /// never became a turn of its own.
    ///
    /// Such a prompt cannot be copied with any turn, so a fork at this turn or
    /// any later one is refused rather than shown a transcript the source did
    /// not have.
    pub lost_prompt: bool,
    /// How the turn finished, when it reported a completion.
    ///
    /// An imported transcript's history has none: it was already finished when
    /// it was written.
    pub status: Option<h4rn_protocol::TurnStatus>,
    /// The usage its completion reported, when it did.
    pub usage: Option<Usage>,
    /// The files it changed, when its completion reported them.
    pub changes: Option<FsTurnChanges>,
    /// Whether these records were copied from a finished conversation.
    ///
    /// An imported or forked turn carries explicit times on its messages and
    /// has no completion row of its own; a live turn's records are journaled as
    /// they happen. It is what tells a host-lost live turn from history when a
    /// fork has to name a status for a turn that never completed.
    pub copied: bool,
    /// The provider's own turn id, when the adapter reported one.
    ///
    /// A native fork that can copy through a named turn uses it.
    pub native_turn_id: Option<String>,
}

impl JournaledTurn {
    /// Whether the journal recorded the turn's end.
    pub fn completed(&self) -> bool {
        self.status.is_some()
    }

    /// Whether a fork may copy this turn.
    ///
    /// Copied history was already finished when it was written. A live turn
    /// needs evidence of its own both ways: the journal saw it start and saw it
    /// end (interrupted counts). A prompt whose turn never started — a queued
    /// message, or one an interruption lost — is not a turn with a transcript
    /// to show.
    pub fn forkable(&self) -> bool {
        self.copied || (self.started_at.is_some() && self.status.is_some())
    }

    /// Appends one record and its journal sequence together.
    fn push_item(&mut self, seq: u64, item: JournaledItem) {
        self.item_seqs.push(seq);
        self.items.push(item);
    }

    /// Inserts one record and its journal sequence together.
    fn insert_item(&mut self, position: usize, seq: u64, item: JournaledItem) {
        self.item_seqs.insert(position, seq);
        self.items.insert(position, item);
    }

    /// Removes the record at `position` and its sequence.
    fn remove_item(&mut self, position: usize) {
        self.item_seqs.remove(position);
        self.items.remove(position);
    }

    /// Every item with its journal sequence, oldest first by that sequence.
    ///
    /// The turn's own records keep their relative order even when a later
    /// queued prompt was written first.
    pub fn items_with_seqs(&self) -> Vec<(u64, &JournaledItem)> {
        let mut pairs: Vec<(u64, &JournaledItem)> = self
            .item_seqs
            .iter()
            .copied()
            .zip(self.items.iter())
            .collect();
        pairs.sort_by_key(|(seq, _)| *seq);
        pairs
    }

    /// The turn's messages, oldest first.
    pub fn messages(&self) -> Vec<(&h4rn_protocol::MessageRole, &str, DateTime<Utc>)> {
        self.items
            .iter()
            .filter_map(|item| match item {
                JournaledItem::Item {
                    item: Item::Message { role, text, .. },
                    at,
                } => Some((role, text.as_str(), *at)),
                _ => None,
            })
            .collect()
    }
}

/// One source conversation already imported, keyed by its identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedSourceRecord {
    /// Source key: harness plus session id, or the transcript path.
    pub key: String,
    /// Thread the conversation became.
    pub thread_id: ThreadId,
    /// Transcript path.
    pub path: String,
    /// When it was imported.
    pub imported_at: DateTime<Utc>,
}

/// A prompt a paused thread still has to deliver, persisted with the thread.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ParkedPrompt {
    /// The turn the prompt belongs to, already recorded in the journal.
    pub turn_id: h4rn_protocol::TurnId,
    /// Prompt text exactly as the person sent it.
    pub text: String,
    /// Attachments to deliver with it.
    pub attachments: Vec<ParkedAttachment>,
    /// The permission ceiling the sender attached to this prompt, if any.
    ///
    /// It survives a restart with the prompt: a supervised agent's message must
    /// not come back at a more permissive mode just because the daemon paused.
    #[serde(default)]
    pub mode_ceiling: Option<h4rn_protocol::RuntimeMode>,
    /// The outside grant that asked for this prompt, when one did.
    ///
    /// It survives a restart with the prompt: the resumed turn must re-check
    /// that the exact token family is still live, and refuse to run if the grant
    /// was revoked while the daemon was down.
    #[serde(default)]
    pub grant: Option<h4rn_protocol::WorkGrant>,
}

/// One attachment of a [`ParkedPrompt`].
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ParkedAttachment {
    /// Content-addressed blob id.
    pub id: h4rn_protocol::AttachmentId,
    /// Absolute path of the blob on the daemon host.
    pub path: String,
    /// Media type, when known.
    pub media_type: String,
}

/// One ordered, projected event as stored.
#[derive(Clone, Debug, PartialEq)]
pub struct WireEvent {
    /// Daemon-global monotonic sequence.
    pub seq: u64,
    /// Owning thread.
    pub thread_id: ThreadId,
    /// The event sent to clients.
    pub event: Event,
}

/// At most one bounded page of a thread's projected events.
#[derive(Clone, Debug, PartialEq)]
pub struct WirePage {
    /// The page, in sequence order.
    pub events: Vec<WireEvent>,
    /// Whether a later event exists beyond the page.
    pub more: bool,
}

// Rows a test reader actually fetched, for the one-page assertion.
//
// Thread-local so tests running in parallel cannot add to each other's count.
#[cfg(test)]
thread_local! {
    pub(crate) static WIRE_PAGE_ROWS_FETCHED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Journal failures.
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    /// The core event store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A daemon-owned SQLite query failed.
    #[error("journal database error: {0}")]
    Database(#[from] sqlx::Error),
    /// The data directory could not be prepared.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A projected event could not be serialised.
    #[error("journal serialisation error: {0}")]
    Serialisation(#[from] serde_json::Error),
}

/// Durable log of domain and projected events.
#[async_trait]
pub trait Journal: Send + Sync {
    /// Inserts or updates metadata for one thread.
    async fn upsert_thread(&self, meta: &ThreadMeta) -> Result<(), JournalError>;

    /// All thread metadata, oldest first.
    async fn list_threads(&self) -> Result<Vec<ThreadMeta>, JournalError>;

    /// Appends domain events plus their projected wire events in one call.
    ///
    /// The metadata upsert happens before the append so resume ids and usage
    /// survive a crash alongside the event that changed them.
    async fn append(
        &self,
        meta: &ThreadMeta,
        domain: &[ThreadEvent],
        wire: &[Event],
    ) -> Result<Vec<u64>, JournalError>;

    /// Appends projected events that are derived (for example a tab change
    /// produced by the background ticker) without a lifecycle event.
    async fn append_wire(
        &self,
        thread_id: &ThreadId,
        wire: &[Event],
    ) -> Result<Vec<u64>, JournalError>;

    /// Projected events for one thread with `seq > after`, in order.
    async fn read_wire_since(
        &self,
        thread_id: &ThreadId,
        after: u64,
    ) -> Result<Vec<WireEvent>, JournalError>;

    /// At most one page of a thread's projected events with `seq > after`.
    ///
    /// The database stops after `max_events` rows or as soon as the page's
    /// serialized text would pass `max_bytes`, so a long replay never loads
    /// the whole thread to hand back its first page. The first event is always
    /// included even when it alone is larger than the budget: an oversized
    /// event is carried whole in its own page rather than dropped or cut.
    async fn read_wire_page(
        &self,
        thread_id: &ThreadId,
        after: u64,
        max_events: usize,
        max_bytes: usize,
    ) -> Result<WirePage, JournalError>;

    /// Projected events across every thread with `seq > after`, in order.
    ///
    /// This is the global replay a reconnecting subscriber asks for; the
    /// caller holds the append lock so the result is a gap-free prefix.
    async fn read_wire_since_global(&self, after: u64) -> Result<Vec<WireEvent>, JournalError>;

    /// The highest sequence number the journal has allocated, or 0 when empty.
    ///
    /// Read while the append lock is held, this is the boundary between the
    /// subscribe replay and live events.
    async fn head(&self) -> Result<u64, JournalError>;

    /// The domain log for one thread, used to rebuild lifecycle state.
    async fn read_domain(&self, thread_id: &ThreadId) -> Result<Vec<ThreadEvent>, JournalError>;

    /// The visible turns of one thread, in the order they were journaled.
    ///
    /// Forking reads this to copy exactly the transcript through a turn,
    /// keeping each message's own time.
    async fn read_journaled_turns(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<JournaledTurn>, JournalError>;

    /// Records the provider's own id for one turn.
    ///
    /// A native fork that can copy through a named turn names this id, so the
    /// mapping has to outlive the daemon run that saw it. Writing the same turn
    /// twice keeps the first id: a provider that renames its turn is not a case
    /// any adapter reports, and the first id is the one the source really ran.
    async fn note_native_turn(
        &self,
        thread_id: &ThreadId,
        turn_id: &h4rn_protocol::TurnId,
        native_turn_id: &str,
    ) -> Result<(), JournalError>;

    /// Inserts or refreshes one thread → pull request link.
    async fn upsert_pr_link(&self, link: &PrLinkRecord) -> Result<(), JournalError>;

    /// Removes one thread → pull request link.
    async fn remove_pr_link(
        &self,
        thread_id: &ThreadId,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<(), JournalError>;

    /// Every PR link one thread holds, oldest link first.
    async fn pr_links_for_thread(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<PrLinkRecord>, JournalError>;

    /// Every thread that links one PR, oldest link first.
    async fn pr_links_for_pr(
        &self,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<Vec<PrLinkRecord>, JournalError>;

    /// The SQLite pool backing this journal, when it has one.
    ///
    /// Guest links live in the same database file as the thread journal
    /// (ADR-044), so the daemon needs the pool rather than a second writer.
    fn sql_pool(&self) -> Option<&SqlitePool> {
        None
    }

    /// Inserts or refreshes one thread → issue link.
    async fn upsert_issue_link(&self, link: &IssueLinkRecord) -> Result<(), JournalError>;

    /// Removes one thread → issue link.
    async fn remove_issue_link(
        &self,
        thread_id: &ThreadId,
        scope: &h4rn_protocol::IssueScope,
        issue_id: &str,
    ) -> Result<(), JournalError>;

    /// Every issue link one thread holds, oldest link first.
    async fn issue_links_for_thread(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<IssueLinkRecord>, JournalError>;

    /// Every scheduled task, oldest first, with its most recent run.
    async fn list_schedules(&self) -> Result<Vec<ScheduleTask>, JournalError>;

    /// One scheduled task, when it exists.
    async fn schedule(
        &self,
        task_id: &ScheduleTaskId,
    ) -> Result<Option<ScheduleTask>, JournalError>;

    /// Inserts or replaces one scheduled task.
    async fn upsert_schedule(&self, task: &ScheduleTask) -> Result<(), JournalError>;

    /// Deletes one task and its run history, returning whether the task existed.
    async fn delete_schedule(&self, task_id: &ScheduleTaskId) -> Result<bool, JournalError>;

    /// Appends one run, fills in its durable id, and trims the task's history
    /// to the newest `keep` entries.
    async fn append_schedule_run(
        &self,
        run: &mut ScheduleRun,
        keep: usize,
    ) -> Result<(), JournalError>;

    /// Appends one run and replaces the task's scheduling state in one
    /// transaction.
    ///
    /// A run and the advancement it describes land together or not at all, so
    /// a crash cannot leave a slot either double-run or lost.
    async fn commit_schedule_run(
        &self,
        run: &mut ScheduleRun,
        task: &ScheduleTask,
        keep: usize,
    ) -> Result<(), JournalError>;

    /// A task's runs, newest first, at most `limit`.
    async fn schedule_runs(
        &self,
        task_id: &ScheduleTaskId,
        limit: usize,
    ) -> Result<Vec<ScheduleRun>, JournalError>;

    /// Every source conversation already imported, oldest first.
    async fn list_imported_sources(&self) -> Result<Vec<ImportedSourceRecord>, JournalError>;

    /// Remembers that a source conversation became `thread_id`.
    async fn record_imported_source(
        &self,
        record: &ImportedSourceRecord,
    ) -> Result<(), JournalError>;

    /// Claims a source conversation and writes its thread in one transaction.
    ///
    /// Returns the new thread's wire sequence numbers, or `None` when another
    /// run had already claimed the source. A crash between the claim and the
    /// thread write rolls both back, so a retry imports the conversation once.
    async fn commit_thread_import(
        &self,
        claim: &ImportedSourceRecord,
        meta: &ThreadMeta,
        domain: &[ThreadEvent],
        wire: &[Event],
    ) -> Result<Option<Vec<u64>>, JournalError>;
}

/// SQLite-backed journal.
#[derive(Clone, Debug)]
pub struct SqliteJournal {
    store: SqliteEventStore,
    pool: SqlitePool,
}

impl SqliteJournal {
    /// Opens (or creates) a database file and applies both schemas.
    pub async fn connect(path: impl AsRef<std::path::Path>) -> Result<Self, JournalError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            crate::config::create_private_dir(parent)
                .map_err(|error| JournalError::Io(std::io::Error::other(error.to_string())))?;
        }
        let store = SqliteEventStore::connect(path).await?;
        Self::from_store(store).await
    }

    /// Opens an in-memory journal for tests.
    pub async fn in_memory() -> Result<Self, JournalError> {
        let store = SqliteEventStore::in_memory().await?;
        Self::from_store(store).await
    }

    /// Opens (or creates) a journal file.
    pub async fn open(path: impl AsRef<std::path::Path>) -> Result<Self, JournalError> {
        let store = SqliteEventStore::connect(path).await?;
        Self::from_store(store).await
    }

    async fn from_store(store: SqliteEventStore) -> Result<Self, JournalError> {
        let pool = store.pool().clone();
        let journal = Self { store, pool };
        journal.ensure_schema().await?;
        Ok(journal)
    }

    async fn ensure_schema(&self) -> Result<(), JournalError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS daemon_threads (
                thread_id      TEXT PRIMARY KEY,
                host           TEXT NOT NULL,
                harness        TEXT NOT NULL,
                title          TEXT NOT NULL,
                title_source   TEXT NOT NULL DEFAULT 'prompt',
                workspace_path TEXT NOT NULL,
                model          TEXT NOT NULL,
                created_at     TEXT NOT NULL,
                usage_json     TEXT NOT NULL,
                resume_id      TEXT,
                runtime_mode   TEXT NOT NULL DEFAULT 'approval_required',
                reasoning_effort TEXT,
                fast           INTEGER NOT NULL DEFAULT 0,
                worktree_json  TEXT,
                parent_thread_id TEXT,
                subagent_id    TEXT,
                agent_id       TEXT,
                agent_fingerprint TEXT,
                parked_json    TEXT,
                queued_json    TEXT NOT NULL DEFAULT '[]',
                claimed_json   TEXT NOT NULL DEFAULT '[]',
                imported_json  TEXT,
                forked_json    TEXT
            )",
        )
        .execute(&self.pool)
        .await?;

        // `CREATE TABLE IF NOT EXISTS` cannot add columns to a database written
        // by an older daemon, so migrate the two Phase 1c fields explicitly.
        self.ensure_column(
            "daemon_threads",
            "runtime_mode",
            "TEXT NOT NULL DEFAULT 'approval_required'",
        )
        .await?;
        self.ensure_column("daemon_threads", "worktree_json", "TEXT")
            .await?;
        self.ensure_column("daemon_threads", "parent_thread_id", "TEXT")
            .await?;
        self.ensure_column("daemon_threads", "subagent_id", "TEXT")
            .await?;
        self.ensure_column("daemon_threads", "agent_id", "TEXT")
            .await?;
        self.ensure_column("daemon_threads", "agent_fingerprint", "TEXT")
            .await?;
        self.ensure_column("daemon_threads", "parked_json", "TEXT")
            .await?;
        self.ensure_column("daemon_threads", "imported_json", "TEXT")
            .await?;
        self.ensure_column("daemon_threads", "forked_json", "TEXT")
            .await?;
        self.ensure_column("daemon_threads", "reasoning_effort", "TEXT")
            .await?;
        self.ensure_column("daemon_threads", "fast", "INTEGER NOT NULL DEFAULT 0")
            .await?;
        self.ensure_column(
            "daemon_threads",
            "title_source",
            "TEXT NOT NULL DEFAULT 'prompt'",
        )
        .await?;
        self.ensure_column(
            "daemon_threads",
            "queued_json",
            "TEXT NOT NULL DEFAULT '[]'",
        )
        .await?;
        self.ensure_column(
            "daemon_threads",
            "claimed_json",
            "TEXT NOT NULL DEFAULT '[]'",
        )
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS daemon_wire_events (
                seq         INTEGER PRIMARY KEY AUTOINCREMENT,
                thread_id   TEXT NOT NULL,
                event_json  TEXT NOT NULL,
                recorded_at TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;

        // The provider's own id for an h4rn turn, when its adapter reported
        // one. A native fork names it as the turn to copy through, so it has to
        // outlive the daemon run that produced it.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS daemon_turn_native_ids (
                thread_id      TEXT NOT NULL,
                turn_id        TEXT NOT NULL,
                native_turn_id TEXT NOT NULL,
                PRIMARY KEY (thread_id, turn_id)
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_daemon_wire_events_thread_seq
             ON daemon_wire_events (thread_id, seq)",
        )
        .execute(&self.pool)
        .await?;

        // Thread ↔ pull request links live beside the thread rows so they
        // survive a restart. The PR's state is not stored: it is read from the
        // forge whenever it is shown.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS daemon_pr_links (
                thread_id  TEXT NOT NULL,
                account_id TEXT NOT NULL,
                forge      TEXT NOT NULL,
                base_url   TEXT NOT NULL,
                owner      TEXT NOT NULL,
                repo       TEXT NOT NULL,
                number     INTEGER NOT NULL,
                linked_at  TEXT NOT NULL,
                PRIMARY KEY (thread_id, forge, base_url, owner, repo, number)
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_daemon_pr_links_pr
             ON daemon_pr_links (forge, base_url, owner, repo, number)",
        )
        .execute(&self.pool)
        .await?;

        // Issue links keep the scope as JSON so a future tracker does not need
        // a schema change; `scope_key` is the stable identity for conflicts.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS daemon_issue_links (
                thread_id  TEXT NOT NULL,
                account_id TEXT NOT NULL,
                scope_kind TEXT NOT NULL,
                scope_key  TEXT NOT NULL,
                scope_json TEXT NOT NULL,
                issue_id   TEXT NOT NULL,
                linked_at  TEXT NOT NULL,
                PRIMARY KEY (thread_id, scope_kind, scope_key, issue_id)
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_daemon_issue_links_thread
             ON daemon_issue_links (thread_id)",
        )
        .execute(&self.pool)
        .await?;

        // Scheduled tasks live in the same journal file as threads so a
        // restart restores the timer, the webhook secret and the run history.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS daemon_schedules (
                task_id        TEXT PRIMARY KEY,
                name           TEXT NOT NULL,
                host           TEXT NOT NULL,
                workspace_path TEXT NOT NULL,
                harness        TEXT NOT NULL,
                agent_id       TEXT,
                model          TEXT NOT NULL,
                runtime_mode   TEXT NOT NULL,
                prompt         TEXT NOT NULL,
                schedule_json  TEXT NOT NULL,
                timezone       TEXT NOT NULL,
                destination    TEXT NOT NULL,
                enabled        INTEGER NOT NULL,
                thread_id      TEXT,
                webhook_secret TEXT,
                once_consumed  INTEGER NOT NULL DEFAULT 0,
                once_slot      TEXT,
                next_run_at    TEXT,
                created_at     TEXT NOT NULL,
                updated_at     TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;

        // A database written before the once-consumed marker exists needs the
        // column added; the default keeps every stored task re-runnable.
        self.ensure_column(
            "daemon_schedules",
            "once_consumed",
            "INTEGER NOT NULL DEFAULT 0",
        )
        .await?;
        // The spent marker now carries the slot's identity; a database from
        // before that column derives it from the stored schedule on read.
        self.ensure_column("daemon_schedules", "once_slot", "TEXT")
            .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS daemon_schedule_runs (
                run_id        INTEGER PRIMARY KEY AUTOINCREMENT,
                task_id       TEXT NOT NULL,
                scheduled_for TEXT,
                started_at    TEXT NOT NULL,
                thread_id     TEXT,
                outcome       TEXT NOT NULL,
                detail        TEXT
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_daemon_schedule_runs_task
             ON daemon_schedule_runs (task_id, run_id)",
        )
        .execute(&self.pool)
        .await?;

        // One row per source conversation that became a thread, so a second
        // import run never duplicates it.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS daemon_imported_sources (
                source_key  TEXT PRIMARY KEY,
                thread_id   TEXT NOT NULL,
                path        TEXT NOT NULL,
                imported_at TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn ensure_column(
        &self,
        table: &str,
        column: &str,
        definition: &str,
    ) -> Result<(), JournalError> {
        let rows = sqlx::query(&format!("PRAGMA table_info({table})"))
            .fetch_all(&self.pool)
            .await?;
        let exists = rows.iter().any(|row| {
            row.try_get::<String, _>("name")
                .is_ok_and(|name| name == column)
        });
        if !exists {
            sqlx::query(&format!(
                "ALTER TABLE {table} ADD COLUMN {column} {definition}"
            ))
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }
}

/// Upserts a thread row on any connection (a pooled one or a transaction's).
async fn upsert_thread_on(
    conn: &mut sqlx::SqliteConnection,
    meta: &ThreadMeta,
) -> Result<(), JournalError> {
    let usage_json = serde_json::to_string(&meta.usage)?;
    let harness = serde_json::to_string(&meta.harness)?;
    let harness = harness.trim_matches('"').to_owned();
    let runtime_mode = serde_json::to_string(&meta.runtime_mode)?;
    let runtime_mode = runtime_mode.trim_matches('"').to_owned();
    let worktree_json = serde_json::to_string(&meta.worktree)?;
    let parked_json = serde_json::to_string(&meta.parked)?;
    let queued_json = serde_json::to_string(&meta.queued)?;
    let claimed_json = serde_json::to_string(&meta.claimed)?;
    let imported_json = serde_json::to_string(&meta.imported)?;
    let forked_json = serde_json::to_string(&meta.forked_from)?;
    sqlx::query(
        "INSERT INTO daemon_threads
            (thread_id, host, harness, title, title_source, workspace_path, model, created_at, usage_json, resume_id, runtime_mode, reasoning_effort, fast, worktree_json, parent_thread_id, subagent_id, agent_id, agent_fingerprint, parked_json, queued_json, claimed_json, imported_json, forked_json)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(thread_id) DO UPDATE SET
            host = excluded.host,
            harness = excluded.harness,
            title = excluded.title,
            title_source = excluded.title_source,
            workspace_path = excluded.workspace_path,
            model = excluded.model,
            usage_json = excluded.usage_json,
            resume_id = excluded.resume_id,
            runtime_mode = excluded.runtime_mode,
            reasoning_effort = excluded.reasoning_effort,
            fast = excluded.fast,
            worktree_json = excluded.worktree_json,
            parent_thread_id = excluded.parent_thread_id,
            subagent_id = excluded.subagent_id,
            agent_id = excluded.agent_id,
            agent_fingerprint = excluded.agent_fingerprint,
            parked_json = excluded.parked_json,
            queued_json = excluded.queued_json,
            claimed_json = excluded.claimed_json,
            imported_json = excluded.imported_json,
            forked_json = excluded.forked_json",
    )
    .bind(meta.thread_id.as_str())
    .bind(meta.host.as_str())
    .bind(harness)
    .bind(&meta.title)
    .bind(meta.title_source.as_str())
    .bind(&meta.workspace_path)
    .bind(&meta.model)
    .bind(meta.created_at.to_rfc3339())
    .bind(usage_json)
    .bind(meta.resume_id.as_deref())
    .bind(runtime_mode)
    .bind(meta.reasoning_effort.as_deref())
    .bind(meta.fast as i64)
    .bind(worktree_json)
    .bind(meta.parent_thread_id.as_ref().map(ThreadId::as_str))
    .bind(meta.subagent_id.as_ref().map(SubagentId::as_str))
    .bind(meta.agent_id.as_deref())
    .bind(meta.agent_fingerprint.as_deref())
    .bind(parked_json)
    .bind(queued_json)
    .bind(claimed_json)
    .bind(imported_json)
    .bind(forked_json)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Inserts wire events on any connection and returns their global sequences.
async fn append_wire_on(
    conn: &mut sqlx::SqliteConnection,
    thread_id: &ThreadId,
    wire: &[Event],
) -> Result<Vec<u64>, JournalError> {
    let recorded_at = Utc::now().to_rfc3339();
    let mut seqs = Vec::with_capacity(wire.len());
    for event in wire {
        let event_json = serde_json::to_string(event)?;
        let result = sqlx::query(
            "INSERT INTO daemon_wire_events (thread_id, event_json, recorded_at) VALUES (?, ?, ?)",
        )
        .bind(thread_id.as_str())
        .bind(event_json)
        .bind(&recorded_at)
        .execute(&mut *conn)
        .await?;
        seqs.push(result.last_insert_rowid() as u64);
    }
    Ok(seqs)
}

/// Whether one record is a message the person wrote.
fn is_user_prompt(item: &JournaledItem) -> bool {
    matches!(
        item,
        JournaledItem::Item {
            item,
            ..
        } if is_user_message(item)
    )
}

/// Whether one streamed item is a message the person wrote.
fn is_user_message(item: &Item) -> bool {
    matches!(
        item,
        Item::Message {
            role: h4rn_protocol::MessageRole::User,
            ..
        }
    )
}

/// The turn a thread-only row belongs to: the open turn when there is one,
/// otherwise the newest turn the journal holds.
fn attach_turn(turns: &mut [JournaledTurn], open: Option<usize>) -> Option<&mut JournaledTurn> {
    match open {
        Some(index) => turns.get_mut(index),
        None => turns.last_mut(),
    }
}

/// Folds a subagent lifecycle event into the delegated row it describes.
///
/// The spawn row is copied from `Item::Delegated` as it was when the child
/// started; the later events carry the child's real final state. A fork must
/// show that final state, not a child that is forever "thinking".
fn fold_subagent(turns: &mut [JournaledTurn], view: &h4rn_protocol::SubagentView) {
    for turn in turns.iter_mut().rev() {
        for item in turn.items.iter_mut().rev() {
            if let JournaledItem::Item { item, .. } = item
                && let Item::Delegated {
                    subagent_id,
                    thread_id,
                    state,
                    agent_state,
                    ..
                } = item
                && *subagent_id == view.subagent_id
            {
                *state = view.state;
                *agent_state = view.agent_state;
                if view.child_thread_id.is_some() {
                    *thread_id = view.child_thread_id.clone();
                }
                return;
            }
        }
    }
}

/// The index of `turn_id` in `turns`, appending an empty turn when this event
/// is the first one the journal holds for it.
fn turn_index(turns: &mut Vec<JournaledTurn>, turn_id: &h4rn_protocol::TurnId) -> usize {
    match turns.iter().position(|turn| &turn.turn_id == turn_id) {
        Some(index) => index,
        None => {
            turns.push(JournaledTurn {
                turn_id: turn_id.clone(),
                started_at: None,
                items: Vec::new(),
                item_seqs: Vec::new(),
                started_seq: None,
                completed_seq: None,
                lost_prompt: false,
                status: None,
                usage: None,
                changes: None,
                copied: false,
                native_turn_id: None,
            });
            turns.len() - 1
        }
    }
}

#[async_trait]
impl Journal for SqliteJournal {
    fn sql_pool(&self) -> Option<&SqlitePool> {
        Some(&self.pool)
    }

    async fn upsert_thread(&self, meta: &ThreadMeta) -> Result<(), JournalError> {
        let mut conn = self.pool.acquire().await?;
        upsert_thread_on(&mut conn, meta).await
    }

    async fn list_threads(&self) -> Result<Vec<ThreadMeta>, JournalError> {
        let rows = sqlx::query(
            "SELECT thread_id, host, harness, title, title_source, workspace_path, model, created_at, usage_json, resume_id, runtime_mode, reasoning_effort, fast, worktree_json, parent_thread_id, subagent_id, agent_id, agent_fingerprint, parked_json, queued_json, claimed_json, imported_json, forked_json
             FROM daemon_threads ORDER BY created_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;

        let mut metas = Vec::with_capacity(rows.len());
        for row in rows {
            let thread_id: String = row.try_get("thread_id")?;
            let host: String = row.try_get("host")?;
            let harness: String = row.try_get("harness")?;
            let usage_json: String = row.try_get("usage_json")?;
            let created_at: String = row.try_get("created_at")?;
            let runtime_mode: String = row.try_get("runtime_mode")?;
            let worktree_json: Option<String> = row.try_get("worktree_json")?;
            let created_at = DateTime::parse_from_rfc3339(&created_at)
                .map(|value| value.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            metas.push(ThreadMeta {
                thread_id: ThreadId::new(thread_id),
                parent_thread_id: row
                    .try_get::<Option<String>, _>("parent_thread_id")?
                    .map(ThreadId::new),
                subagent_id: row
                    .try_get::<Option<String>, _>("subagent_id")?
                    .map(SubagentId::new),
                host: HostId::new(host),
                harness: serde_json::from_str(&format!("\"{harness}\""))
                    .unwrap_or(HarnessKind::ClaudeCode),
                agent_id: row.try_get("agent_id")?,
                title: row.try_get("title")?,
                title_source: TitleSource::parse(
                    &row.try_get::<String, _>("title_source")
                        .unwrap_or_else(|_| "prompt".to_owned()),
                ),
                workspace_path: row.try_get("workspace_path")?,
                model: row.try_get("model")?,
                runtime_mode: serde_json::from_str(&format!("\"{runtime_mode}\""))
                    .unwrap_or_default(),
                reasoning_effort: row.try_get("reasoning_effort")?,
                fast: row.try_get::<i64, _>("fast")? != 0,
                worktree: worktree_json
                    .and_then(|json| serde_json::from_str(&json).ok())
                    .flatten(),
                created_at,
                usage: serde_json::from_str(&usage_json).unwrap_or_default(),
                resume_id: row.try_get("resume_id")?,
                agent_fingerprint: row.try_get("agent_fingerprint")?,
                parked: row
                    .try_get::<Option<String>, _>("parked_json")?
                    .and_then(|json| serde_json::from_str(&json).ok()),
                queued: row
                    .try_get::<Option<String>, _>("queued_json")?
                    .and_then(|json| serde_json::from_str(&json).ok())
                    .unwrap_or_default(),
                claimed: row
                    .try_get::<Option<String>, _>("claimed_json")?
                    .and_then(|json| serde_json::from_str(&json).ok())
                    .unwrap_or_default(),
                imported: row
                    .try_get::<Option<String>, _>("imported_json")?
                    .and_then(|json| serde_json::from_str(&json).ok()),
                forked_from: row
                    .try_get::<Option<String>, _>("forked_json")?
                    .and_then(|json| serde_json::from_str(&json).ok()),
            });
        }
        Ok(metas)
    }

    async fn append(
        &self,
        meta: &ThreadMeta,
        domain: &[ThreadEvent],
        wire: &[Event],
    ) -> Result<Vec<u64>, JournalError> {
        // One transaction: the thread row, the domain log and the wire log
        // commit together or not at all, so a crash or a failed insert cannot
        // leave a domain event without its wire event.
        let mut tx = self.pool.begin().await?;
        upsert_thread_on(&mut tx, meta).await?;
        if !domain.is_empty() {
            SqliteEventStore::append_in(&mut tx, &meta.thread_id, domain).await?;
        }
        let seqs = if wire.is_empty() {
            Vec::new()
        } else {
            append_wire_on(&mut tx, &meta.thread_id, wire).await?
        };
        tx.commit().await?;
        Ok(seqs)
    }

    async fn append_wire(
        &self,
        thread_id: &ThreadId,
        wire: &[Event],
    ) -> Result<Vec<u64>, JournalError> {
        if wire.is_empty() {
            return Ok(Vec::new());
        }
        let mut conn = self.pool.acquire().await?;
        append_wire_on(&mut conn, thread_id, wire).await
    }

    async fn read_wire_since(
        &self,
        thread_id: &ThreadId,
        after: u64,
    ) -> Result<Vec<WireEvent>, JournalError> {
        let rows = sqlx::query(
            "SELECT seq, event_json FROM daemon_wire_events
             WHERE thread_id = ? AND seq > ? ORDER BY seq ASC",
        )
        .bind(thread_id.as_str())
        .bind(after as i64)
        .fetch_all(&self.pool)
        .await?;

        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let seq: i64 = row.try_get("seq")?;
            let event_json: String = row.try_get("event_json")?;
            events.push(WireEvent {
                seq: seq as u64,
                thread_id: thread_id.clone(),
                event: serde_json::from_str(&event_json)?,
            });
        }
        Ok(events)
    }

    async fn read_wire_page(
        &self,
        thread_id: &ThreadId,
        after: u64,
        max_events: usize,
        max_bytes: usize,
    ) -> Result<WirePage, JournalError> {
        // The recursive walk takes one row at a time over the (thread_id, seq)
        // index and stops as soon as the page's stored text would pass the
        // budget, so the database reads one page instead of the whole replay.
        // The first row is taken unconditionally, which keeps an oversized
        // event whole.
        //
        // The budget counts each row's stored `event_json` bytes, not the
        // event re-serialized by this build. A page boundary for history
        // written by an older protocol version can therefore land elsewhere
        // than a re-serialized count would put it; nothing is lost either way,
        // because the next page starts at the following seq.
        let rows = sqlx::query(
            "WITH RECURSIVE page(seq, event_json, used) AS (
                 SELECT seq, event_json, length(CAST(event_json AS BLOB))
                   FROM daemon_wire_events
                  WHERE thread_id = ?1
                    AND seq = (SELECT MIN(seq) FROM daemon_wire_events
                                WHERE thread_id = ?1 AND seq > ?2)
                 UNION ALL
                 SELECT next.seq, next.event_json,
                        page.used + length(CAST(next.event_json AS BLOB))
                   FROM page
                   JOIN daemon_wire_events AS next
                     ON next.thread_id = ?1
                    AND next.seq = (SELECT MIN(seq) FROM daemon_wire_events
                                     WHERE thread_id = ?1 AND seq > page.seq)
                  WHERE page.used + length(CAST(next.event_json AS BLOB)) <= ?3
             )
             SELECT seq, event_json FROM page ORDER BY seq LIMIT ?4",
        )
        .bind(thread_id.as_str())
        .bind(after as i64)
        .bind(max_bytes as i64)
        .bind(max_events as i64)
        .fetch_all(&self.pool)
        .await?;

        #[cfg(test)]
        WIRE_PAGE_ROWS_FETCHED.with(|fetched| fetched.set(fetched.get() + rows.len()));

        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let seq: i64 = row.try_get("seq")?;
            let event_json: String = row.try_get("event_json")?;
            events.push(WireEvent {
                seq: seq as u64,
                thread_id: thread_id.clone(),
                event: serde_json::from_str(&event_json)?,
            });
        }
        // One indexed probe says whether anything follows the page; this keeps
        // the `more` flag exact even when the row cap ended the page.
        let last = events.last().map(|event| event.seq).unwrap_or(after);
        let more: i64 = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM daemon_wire_events
                            WHERE thread_id = ? AND seq > ?)",
        )
        .bind(thread_id.as_str())
        .bind(last as i64)
        .fetch_one(&self.pool)
        .await?;
        Ok(WirePage {
            events,
            more: more != 0,
        })
    }

    async fn read_wire_since_global(&self, after: u64) -> Result<Vec<WireEvent>, JournalError> {
        let rows = sqlx::query(
            "SELECT seq, thread_id, event_json FROM daemon_wire_events
             WHERE seq > ? ORDER BY seq ASC",
        )
        .bind(after as i64)
        .fetch_all(&self.pool)
        .await?;

        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let seq: i64 = row.try_get("seq")?;
            let thread_id: String = row.try_get("thread_id")?;
            let event_json: String = row.try_get("event_json")?;
            events.push(WireEvent {
                seq: seq as u64,
                thread_id: ThreadId::new(thread_id),
                event: serde_json::from_str(&event_json)?,
            });
        }
        Ok(events)
    }

    async fn head(&self) -> Result<u64, JournalError> {
        let head: Option<i64> = sqlx::query_scalar("SELECT MAX(seq) FROM daemon_wire_events")
            .fetch_one(&self.pool)
            .await?;
        Ok(head.unwrap_or(0) as u64)
    }

    async fn read_domain(&self, thread_id: &ThreadId) -> Result<Vec<ThreadEvent>, JournalError> {
        Ok(self
            .store
            .read_since(thread_id, 0)
            .await?
            .into_iter()
            .map(|stored| stored.event)
            .collect())
    }

    async fn read_journaled_turns(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<JournaledTurn>, JournalError> {
        let rows = sqlx::query(
            "SELECT seq, event_json, recorded_at FROM daemon_wire_events
             WHERE thread_id = ? ORDER BY seq ASC",
        )
        .bind(thread_id.as_str())
        .fetch_all(&self.pool)
        .await?;
        let native_rows = sqlx::query(
            "SELECT turn_id, native_turn_id FROM daemon_turn_native_ids WHERE thread_id = ?",
        )
        .bind(thread_id.as_str())
        .fetch_all(&self.pool)
        .await?;

        let mut turns: Vec<JournaledTurn> = Vec::new();
        // The turn whose `TurnStarted` has no `TurnCompleted` yet. A queued
        // prompt is journaled under its own turn id while an earlier turn runs,
        // so a row that names only the thread belongs to this turn, not to the
        // newest turn the reader happens to hold.
        let mut open_turn: Option<usize> = None;
        // Prompts an older journal wrote under the running turn instead of the
        // queued turn they belong to, with the sequence they were shown at. Each
        // one is given to the next turn that starts and has no prompt of its
        // own; a turn that already has its own prompt keeps it, and the held
        // prompt never became a turn.
        let mut held_prompts: Vec<(u64, JournaledItem)> = Vec::new();
        // Set once a held prompt could not be recovered. It is a message the
        // source showed and no turn owns, so every turn from here on is an
        // ambiguous fork point: copying one would drop a row the source had.
        let mut lost_prompt = false;
        // The item each streaming assistant reply is filling, by item id. A
        // reply that never finished stays here and is rebuilt from its deltas
        // once every event has been read.
        let mut streaming: std::collections::HashMap<String, (usize, usize)> =
            std::collections::HashMap::new();
        for row in rows {
            let seq: i64 = row.try_get("seq")?;
            let seq = u64::try_from(seq).unwrap_or(0);
            let event_json: String = row.try_get("event_json")?;
            let recorded_at: String = row.try_get("recorded_at")?;
            let recorded_at = DateTime::parse_from_rfc3339(&recorded_at)
                .map(|value| value.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            let event: Event = serde_json::from_str(&event_json)?;
            match event {
                Event::TurnStarted {
                    turn_id,
                    started_at,
                    ..
                } => {
                    let previous_open = open_turn;
                    let index = turn_index(&mut turns, &turn_id);
                    turns[index].started_at = Some(started_at);
                    turns[index].started_seq = Some(seq);
                    // An older journal wrote a queued prompt under the turn it
                    // waited behind. The next turn that starts is the one it
                    // was waiting to become, so hand it the oldest such prompt —
                    // but only when this turn has no prompt of its own. A turn
                    // that already sent one is a later message, and attaching
                    // the held prompt to it would put a message it never sent
                    // into its transcript.
                    if previous_open != Some(index) && !held_prompts.is_empty() {
                        let has_own_prompt = turns[index].items.iter().any(is_user_prompt);
                        if has_own_prompt {
                            lost_prompt = true;
                        } else {
                            let (prompt_seq, prompt) = held_prompts.remove(0);
                            turns[index].insert_item(0, prompt_seq, prompt);
                        }
                    }
                    if lost_prompt {
                        turns[index].lost_prompt = true;
                    }
                    open_turn = Some(index);
                }
                Event::ItemCompleted {
                    item, turn_id, at, ..
                } => {
                    let index = turn_index(&mut turns, &turn_id);
                    // An explicit authoring time marks a copied record: a live
                    // turn's items carry none and fall back to the journal's.
                    if at.is_some() {
                        turns[index].copied = true;
                    }
                    let at = at.unwrap_or(recorded_at);
                    // A user item that arrives while its turn is already open,
                    // after that turn's own prompt, is a prompt an older journal
                    // stored under the turn it was queued behind. It belongs to
                    // the next turn that starts, never to this one.
                    if is_user_message(&item)
                        && open_turn == Some(index)
                        && turns[index].items.iter().any(is_user_prompt)
                    {
                        held_prompts.push((seq, JournaledItem::Item { item, at }));
                        continue;
                    }
                    // The daemon finishes a streamed reply with one item that
                    // carries the same id as its deltas. Keep the one entry the
                    // reader already saw, so the fork shows the reply once.
                    if let Item::Message { item_id, .. } = &item
                        && let Some((turn, position)) = streaming.remove(item_id)
                        && let JournaledItem::Item {
                            item:
                                Item::Message {
                                    text, attachments, ..
                                },
                            at: streamed_at,
                        } = &mut turns[turn].items[position]
                    {
                        if let Item::Message {
                            text: complete_text,
                            attachments: complete_attachments,
                            ..
                        } = item
                        {
                            // The completion is authoritative about the text;
                            // the delta time stays the message's own.
                            *text = complete_text;
                            *attachments = complete_attachments;
                        }
                        if at > *streamed_at {
                            *streamed_at = at;
                        }
                        continue;
                    }
                    turns[index].push_item(seq, JournaledItem::Item { item, at });
                }
                Event::MessageDelta {
                    turn_id,
                    item_id,
                    text,
                    ..
                } => {
                    let index = turn_index(&mut turns, &turn_id);
                    if let Some((turn, position)) = streaming.get(&item_id) {
                        if let JournaledItem::Item {
                            item: Item::Message { text: current, .. },
                            ..
                        } = &mut turns[*turn].items[*position]
                        {
                            current.push_str(&text);
                        }
                    } else {
                        let position = turns[index].items.len();
                        turns[index].push_item(
                            seq,
                            JournaledItem::Item {
                                item: Item::Message {
                                    item_id: item_id.clone(),
                                    role: h4rn_protocol::MessageRole::Assistant,
                                    text,
                                    attachments: Vec::new(),
                                },
                                at: recorded_at,
                            },
                        );
                        streaming.insert(item_id, (index, position));
                    }
                }
                Event::ToolCall {
                    turn_id, tool_call, ..
                } => {
                    let index = turn_index(&mut turns, &turn_id);
                    turns[index].push_item(seq, JournaledItem::ToolCall { tool_call });
                }
                Event::Plan {
                    turn_id, entries, ..
                } => {
                    let index = turn_index(&mut turns, &turn_id);
                    match turns[index]
                        .items
                        .iter_mut()
                        .find(|item| matches!(item, JournaledItem::Plan { .. }))
                    {
                        // A plan replaces the whole list, so only the last one
                        // is part of what the transcript shows.
                        Some(JournaledItem::Plan {
                            entries: current, ..
                        }) => *current = entries,
                        _ => turns[index].push_item(seq, JournaledItem::Plan { entries }),
                    }
                }
                // These name a thread but not a turn: the stream attaches them
                // to the turn that was running, so the reader does the same.
                // The open turn is the one whose start has no end yet; falling
                // back to the newest turn only when nothing is open keeps a
                // queued prompt's turn from taking an earlier turn's row.
                Event::Error {
                    message,
                    hint,
                    retryable,
                    ..
                } => {
                    if let Some(turn) = attach_turn(&mut turns, open_turn) {
                        turn.push_item(
                            seq,
                            JournaledItem::Error {
                                message,
                                hint,
                                retryable,
                                at: recorded_at,
                            },
                        );
                    }
                }
                Event::UsageLimitHit {
                    reset_at,
                    auto_resume,
                    ..
                } => {
                    if let Some(turn) = attach_turn(&mut turns, open_turn) {
                        turn.push_item(
                            seq,
                            JournaledItem::Limit {
                                reset_at,
                                auto_resume,
                                at: recorded_at,
                            },
                        );
                    }
                }
                Event::SubagentSpawned { subagent, .. }
                | Event::SubagentUpdated { subagent, .. }
                | Event::SubagentFinished { subagent, .. } => {
                    fold_subagent(&mut turns, &subagent);
                }
                Event::TurnCompleted {
                    turn_id,
                    status,
                    usage,
                    changes,
                    ..
                } => {
                    let index = turn_index(&mut turns, &turn_id);
                    turns[index].status = Some(status);
                    turns[index].usage = Some(usage);
                    turns[index].changes = changes;
                    turns[index].completed_seq = Some(seq);
                    if open_turn == Some(index) {
                        open_turn = turns
                            .iter()
                            .rposition(|turn| turn.started_at.is_some() && turn.status.is_none());
                    }
                }
                _ => {}
            }
        }

        // A reply that was still streaming when the daemon stopped exists only
        // as deltas. The person saw it, so its rebuilt text belongs in the
        // copied transcript; only a reply that never produced a character is
        // dropped.
        let mut empty: Vec<(usize, usize)> = streaming
            .into_values()
            .filter(|(turn, position)| {
                matches!(
                    &turns[*turn].items[*position],
                    JournaledItem::Item {
                        item: Item::Message { text, .. },
                        ..
                    } if text.is_empty()
                )
            })
            .collect();
        empty.sort_unstable_by(|left, right| right.cmp(left));
        for (turn, position) in empty {
            turns[turn].remove_item(position);
        }

        let mut native: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for row in native_rows {
            let turn_id: String = row.try_get("turn_id")?;
            let native_turn_id: String = row.try_get("native_turn_id")?;
            native.insert(turn_id, native_turn_id);
        }
        for turn in &mut turns {
            turn.native_turn_id = native.get(turn.turn_id.as_str()).cloned();
        }

        // A daemon from before this round recorded a host loss as the domain
        // event and an error row, but never the wire `TurnCompleted` that now
        // ends the turn it lost. Without that row the turn reads as "started,
        // never settled", which refuses a fork at it and at every later turn
        // forever. The domain log still holds the evidence: a turn that had
        // already started when a host loss was recorded, and which never
        // reported its own end, ended with that loss. It is settled as
        // interrupted here, exactly as a restart records it now.
        let host_lost: Vec<DateTime<Utc>> = self
            .read_domain(thread_id)
            .await?
            .into_iter()
            .filter_map(|event| match event {
                ThreadEvent::HostLost { at } => Some(at),
                _ => None,
            })
            .collect();
        if !host_lost.is_empty() {
            for turn in &mut turns {
                if turn.status.is_some() {
                    continue;
                }
                let Some(started) = turn.started_at else {
                    continue;
                };
                if host_lost.iter().any(|at| *at >= started) {
                    turn.status = Some(h4rn_protocol::TurnStatus::Interrupted);
                }
            }
        }
        Ok(turns)
    }

    async fn note_native_turn(
        &self,
        thread_id: &ThreadId,
        turn_id: &h4rn_protocol::TurnId,
        native_turn_id: &str,
    ) -> Result<(), JournalError> {
        sqlx::query(
            "INSERT INTO daemon_turn_native_ids (thread_id, turn_id, native_turn_id)
             VALUES (?, ?, ?)
             ON CONFLICT(thread_id, turn_id) DO NOTHING",
        )
        .bind(thread_id.as_str())
        .bind(turn_id.as_str())
        .bind(native_turn_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn upsert_pr_link(&self, link: &PrLinkRecord) -> Result<(), JournalError> {
        sqlx::query(
            "INSERT INTO daemon_pr_links
                (thread_id, account_id, forge, base_url, owner, repo, number, linked_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(thread_id, forge, base_url, owner, repo, number) DO UPDATE SET
                account_id = excluded.account_id",
        )
        .bind(link.thread_id.as_str())
        .bind(&link.account_id)
        .bind(link.repo.forge.slug())
        .bind(&link.repo.base_url)
        .bind(&link.repo.owner)
        .bind(&link.repo.repo)
        .bind(i64::from(link.number))
        .bind(link.linked_at.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn remove_pr_link(
        &self,
        thread_id: &ThreadId,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<(), JournalError> {
        sqlx::query(
            "DELETE FROM daemon_pr_links
             WHERE thread_id = ? AND forge = ? AND base_url = ? AND owner = ? AND repo = ?
               AND number = ?",
        )
        .bind(thread_id.as_str())
        .bind(repo.forge.slug())
        .bind(&repo.base_url)
        .bind(&repo.owner)
        .bind(&repo.repo)
        .bind(i64::from(number))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn pr_links_for_thread(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<PrLinkRecord>, JournalError> {
        let rows = sqlx::query(
            "SELECT thread_id, account_id, forge, base_url, owner, repo, number, linked_at
             FROM daemon_pr_links WHERE thread_id = ?
             ORDER BY linked_at ASC, rowid ASC",
        )
        .bind(thread_id.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(pr_link_from_row).collect()
    }

    async fn pr_links_for_pr(
        &self,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<Vec<PrLinkRecord>, JournalError> {
        let rows = sqlx::query(
            "SELECT thread_id, account_id, forge, base_url, owner, repo, number, linked_at
             FROM daemon_pr_links
             WHERE forge = ? AND base_url = ? AND owner = ? AND repo = ? AND number = ?
             ORDER BY linked_at ASC, rowid ASC",
        )
        .bind(repo.forge.slug())
        .bind(&repo.base_url)
        .bind(&repo.owner)
        .bind(&repo.repo)
        .bind(i64::from(number))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(pr_link_from_row).collect()
    }

    async fn upsert_issue_link(&self, link: &IssueLinkRecord) -> Result<(), JournalError> {
        let (kind, key) = issue_scope_parts(&link.scope);
        let scope_json = serde_json::to_string(&link.scope)
            .map_err(|error| JournalError::Database(sqlx::Error::Protocol(error.to_string())))?;
        sqlx::query(
            "INSERT INTO daemon_issue_links
                (thread_id, account_id, scope_kind, scope_key, scope_json, issue_id, linked_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(thread_id, scope_kind, scope_key, issue_id) DO UPDATE SET
                account_id = excluded.account_id",
        )
        .bind(link.thread_id.as_str())
        .bind(&link.account_id)
        .bind(kind)
        .bind(key)
        .bind(scope_json)
        .bind(&link.issue_id)
        .bind(link.linked_at.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn remove_issue_link(
        &self,
        thread_id: &ThreadId,
        scope: &h4rn_protocol::IssueScope,
        issue_id: &str,
    ) -> Result<(), JournalError> {
        let (kind, key) = issue_scope_parts(scope);
        sqlx::query(
            "DELETE FROM daemon_issue_links
             WHERE thread_id = ? AND scope_kind = ? AND scope_key = ? AND issue_id = ?",
        )
        .bind(thread_id.as_str())
        .bind(kind)
        .bind(key)
        .bind(issue_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn issue_links_for_thread(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<IssueLinkRecord>, JournalError> {
        let rows = sqlx::query(
            "SELECT thread_id, account_id, scope_json, issue_id, linked_at
             FROM daemon_issue_links WHERE thread_id = ?
             ORDER BY linked_at ASC, rowid ASC",
        )
        .bind(thread_id.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(issue_link_from_row).collect()
    }

    async fn list_schedules(&self) -> Result<Vec<ScheduleTask>, JournalError> {
        let rows = sqlx::query(
            "SELECT task_id, name, host, workspace_path, harness, agent_id, model,
                    runtime_mode, prompt, schedule_json, timezone, destination, enabled,
                    thread_id, webhook_secret, once_consumed, once_slot, next_run_at, created_at, updated_at
             FROM daemon_schedules ORDER BY created_at ASC, rowid ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut tasks = Vec::with_capacity(rows.len());
        for row in rows {
            let mut task = schedule_task_from_row(row)?;
            task.last_run = self.schedule_runs(&task.task_id, 1).await?.pop();
            tasks.push(task);
        }
        Ok(tasks)
    }

    async fn schedule(
        &self,
        task_id: &ScheduleTaskId,
    ) -> Result<Option<ScheduleTask>, JournalError> {
        let row = sqlx::query(
            "SELECT task_id, name, host, workspace_path, harness, agent_id, model,
                    runtime_mode, prompt, schedule_json, timezone, destination, enabled,
                    thread_id, webhook_secret, once_consumed, once_slot, next_run_at, created_at, updated_at
             FROM daemon_schedules WHERE task_id = ?",
        )
        .bind(task_id.as_str())
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let mut task = schedule_task_from_row(row)?;
        task.last_run = self.schedule_runs(task_id, 1).await?.pop();
        Ok(Some(task))
    }

    async fn upsert_schedule(&self, task: &ScheduleTask) -> Result<(), JournalError> {
        let mut tx = self.pool.begin().await?;
        upsert_schedule_row(&mut tx, task).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn delete_schedule(&self, task_id: &ScheduleTaskId) -> Result<bool, JournalError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM daemon_schedule_runs WHERE task_id = ?")
            .bind(task_id.as_str())
            .execute(&mut *tx)
            .await?;
        let deleted = sqlx::query("DELETE FROM daemon_schedules WHERE task_id = ?")
            .bind(task_id.as_str())
            .execute(&mut *tx)
            .await?
            .rows_affected()
            > 0;
        tx.commit().await?;
        Ok(deleted)
    }

    async fn append_schedule_run(
        &self,
        run: &mut ScheduleRun,
        keep: usize,
    ) -> Result<(), JournalError> {
        let mut tx = self.pool.begin().await?;
        insert_schedule_run(&mut tx, run).await?;
        trim_schedule_runs(&mut tx, &run.task_id, keep).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn commit_schedule_run(
        &self,
        run: &mut ScheduleRun,
        task: &ScheduleTask,
        keep: usize,
    ) -> Result<(), JournalError> {
        let mut tx = self.pool.begin().await?;
        insert_schedule_run(&mut tx, run).await?;
        trim_schedule_runs(&mut tx, &run.task_id, keep).await?;
        upsert_schedule_row(&mut tx, task).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn schedule_runs(
        &self,
        task_id: &ScheduleTaskId,
        limit: usize,
    ) -> Result<Vec<ScheduleRun>, JournalError> {
        let limit = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);
        let rows = sqlx::query(
            "SELECT run_id, task_id, scheduled_for, started_at, thread_id, outcome, detail
             FROM daemon_schedule_runs WHERE task_id = ?
             ORDER BY run_id DESC LIMIT ?",
        )
        .bind(task_id.as_str())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(schedule_run_from_row).collect()
    }

    async fn list_imported_sources(&self) -> Result<Vec<ImportedSourceRecord>, JournalError> {
        let rows = sqlx::query(
            "SELECT source_key, thread_id, path, imported_at
             FROM daemon_imported_sources ORDER BY imported_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut records = Vec::with_capacity(rows.len());
        for row in rows {
            let imported_at: String = row.try_get("imported_at")?;
            records.push(ImportedSourceRecord {
                key: row.try_get("source_key")?,
                thread_id: ThreadId::new(row.try_get::<String, _>("thread_id")?),
                path: row.try_get("path")?,
                imported_at: DateTime::parse_from_rfc3339(&imported_at)
                    .map(|value| value.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
            });
        }
        Ok(records)
    }

    async fn record_imported_source(
        &self,
        record: &ImportedSourceRecord,
    ) -> Result<(), JournalError> {
        sqlx::query(
            "INSERT INTO daemon_imported_sources (source_key, thread_id, path, imported_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(source_key) DO UPDATE SET
                thread_id = excluded.thread_id,
                path = excluded.path,
                imported_at = excluded.imported_at",
        )
        .bind(&record.key)
        .bind(record.thread_id.as_str())
        .bind(&record.path)
        .bind(record.imported_at.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn commit_thread_import(
        &self,
        claim: &ImportedSourceRecord,
        meta: &ThreadMeta,
        domain: &[ThreadEvent],
        wire: &[Event],
    ) -> Result<Option<Vec<u64>>, JournalError> {
        // One transaction: the claim, the thread row, the domain log and the
        // wire log commit together or not at all. A concurrent run loses the
        // claim insert and writes nothing; a crash rolls everything back, so
        // the retry is still the conversation's first import.
        let mut tx = self.pool.begin().await?;
        let claimed = sqlx::query(
            "INSERT INTO daemon_imported_sources (source_key, thread_id, path, imported_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(source_key) DO NOTHING",
        )
        .bind(&claim.key)
        .bind(claim.thread_id.as_str())
        .bind(&claim.path)
        .bind(claim.imported_at.to_rfc3339())
        .execute(&mut *tx)
        .await?;
        if claimed.rows_affected() == 0 {
            tx.rollback().await?;
            return Ok(None);
        }
        upsert_thread_on(&mut tx, meta).await?;
        if !domain.is_empty() {
            SqliteEventStore::append_in(&mut tx, &meta.thread_id, domain).await?;
        }
        let seqs = if wire.is_empty() {
            Vec::new()
        } else {
            append_wire_on(&mut tx, &meta.thread_id, wire).await?
        };
        tx.commit().await?;
        Ok(Some(seqs))
    }
}

#[cfg(test)]
impl SqliteJournal {
    /// Stores a raw `next_run_at` so an upgrade test can write a timestamp the
    /// daemon cannot decode.
    pub(crate) async fn set_raw_next_run_at(
        &self,
        task_id: &ScheduleTaskId,
        raw: Option<&str>,
    ) -> Result<(), JournalError> {
        sqlx::query("UPDATE daemon_schedules SET next_run_at = ? WHERE task_id = ?")
            .bind(raw)
            .bind(task_id.as_str())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The stored `next_run_at` exactly as written, undecoded.
    pub(crate) async fn raw_next_run_at(
        &self,
        task_id: &ScheduleTaskId,
    ) -> Result<Option<String>, JournalError> {
        let row = sqlx::query("SELECT next_run_at FROM daemon_schedules WHERE task_id = ?")
            .bind(task_id.as_str())
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let raw: Option<String> = row.try_get("next_run_at")?;
        Ok(raw)
    }
}

/// Inserts one run row inside a transaction and fills in its durable id.
async fn insert_schedule_run(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    run: &mut ScheduleRun,
) -> Result<(), JournalError> {
    let outcome = serde_json::to_string(&run.outcome)?;
    let outcome = outcome.trim_matches('"').to_owned();
    let result = sqlx::query(
        "INSERT INTO daemon_schedule_runs
            (task_id, scheduled_for, started_at, thread_id, outcome, detail)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(run.task_id.as_str())
    .bind(run.scheduled_for.map(|at| at.to_rfc3339()))
    .bind(run.started_at.to_rfc3339())
    .bind(run.thread_id.as_ref().map(ThreadId::as_str))
    .bind(outcome)
    .bind(run.detail.as_deref())
    .execute(&mut **tx)
    .await?;
    run.run_id = result.last_insert_rowid() as u64;
    Ok(())
}

/// Keeps only the newest `keep` runs of one task inside a transaction.
async fn trim_schedule_runs(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    task_id: &ScheduleTaskId,
    keep: usize,
) -> Result<(), JournalError> {
    let keep = i64::try_from(keep.max(1)).unwrap_or(i64::MAX);
    sqlx::query(
        "DELETE FROM daemon_schedule_runs
         WHERE task_id = ? AND run_id NOT IN (
            SELECT run_id FROM daemon_schedule_runs
            WHERE task_id = ? ORDER BY run_id DESC LIMIT ?
         )",
    )
    .bind(task_id.as_str())
    .bind(task_id.as_str())
    .bind(keep)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Inserts or replaces one task row inside a transaction.
async fn upsert_schedule_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    task: &ScheduleTask,
) -> Result<(), JournalError> {
    let harness = serde_json::to_string(&task.harness)?;
    let harness = harness.trim_matches('"').to_owned();
    let runtime_mode = serde_json::to_string(&task.runtime_mode)?;
    let runtime_mode = runtime_mode.trim_matches('"').to_owned();
    let destination = serde_json::to_string(&task.destination)?;
    let destination = destination.trim_matches('"').to_owned();
    let schedule_json = serde_json::to_string(&task.schedule)?;
    sqlx::query(
        "INSERT INTO daemon_schedules
            (task_id, name, host, workspace_path, harness, agent_id, model,
             runtime_mode, prompt, schedule_json, timezone, destination, enabled,
             thread_id, webhook_secret, once_consumed, once_slot, next_run_at, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(task_id) DO UPDATE SET
            name = excluded.name,
            host = excluded.host,
            workspace_path = excluded.workspace_path,
            harness = excluded.harness,
            agent_id = excluded.agent_id,
            model = excluded.model,
            runtime_mode = excluded.runtime_mode,
            prompt = excluded.prompt,
            schedule_json = excluded.schedule_json,
            timezone = excluded.timezone,
            destination = excluded.destination,
            enabled = excluded.enabled,
            thread_id = excluded.thread_id,
            webhook_secret = excluded.webhook_secret,
            once_consumed = excluded.once_consumed,
            once_slot = excluded.once_slot,
            next_run_at = excluded.next_run_at,
            updated_at = excluded.updated_at",
    )
    .bind(task.task_id.as_str())
    .bind(&task.name)
    .bind(task.host.as_str())
    .bind(&task.workspace_path)
    .bind(harness)
    .bind(task.agent_id.as_deref())
    .bind(&task.model)
    .bind(runtime_mode)
    .bind(&task.prompt)
    .bind(schedule_json)
    .bind(&task.timezone)
    .bind(destination)
    .bind(task.enabled as i64)
    .bind(task.thread_id.as_ref().map(ThreadId::as_str))
    .bind(webhook_secret_of(task))
    .bind(task.once_consumed as i64)
    .bind(task.once_consumed_slot.as_deref())
    .bind(task.next_run_at.map(|at| at.to_rfc3339()))
    .bind(task.created_at.to_rfc3339())
    .bind(task.updated_at.to_rfc3339())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The webhook secret embedded in a task's public path.
fn webhook_secret_of(task: &ScheduleTask) -> Option<&str> {
    task.webhook_path
        .as_deref()
        .and_then(|path| path.strip_prefix("/webhooks/"))
        .filter(|secret| !secret.is_empty())
}

fn decode_enum<T: serde::de::DeserializeOwned>(
    value: &str,
    column: &str,
) -> Result<T, JournalError> {
    serde_json::from_str(&format!("\"{value}\"")).map_err(|error| {
        JournalError::Database(sqlx::Error::Protocol(format!(
            "unknown value {value:?} in {column}: {error}"
        )))
    })
}

fn schedule_task_from_row(row: sqlx::sqlite::SqliteRow) -> Result<ScheduleTask, JournalError> {
    let task_id: String = row.try_get("task_id")?;
    let harness: String = row.try_get("harness")?;
    let runtime_mode: String = row.try_get("runtime_mode")?;
    let destination: String = row.try_get("destination")?;
    let schedule_json: String = row.try_get("schedule_json")?;
    let created_at: String = row.try_get("created_at")?;
    let updated_at: String = row.try_get("updated_at")?;
    let next_run_at: Option<String> = row.try_get("next_run_at")?;
    let webhook_secret: Option<String> = row.try_get("webhook_secret")?;
    let webhook_path_set = webhook_secret
        .as_deref()
        .is_some_and(|secret| !secret.is_empty());
    let schedule = serde_json::from_str::<ScheduleSpec>(&schedule_json)?;
    // A row written before the slot identity existed only has the flag; the
    // stored schedule names the slot it spent. The zone joins the identity in
    // `ScheduleService::open`, which migrates this older form.
    let once_slot: Option<String> = row.try_get("once_slot")?;
    let legacy_consumed: i64 = row.try_get("once_consumed")?;
    let once_consumed_slot = once_slot.or_else(|| {
        if legacy_consumed == 0 {
            None
        } else {
            schedule.legacy_once_slot()
        }
    });
    let once_consumed = once_consumed_slot.is_some();
    let timezone: String = row.try_get("timezone")?;
    Ok(ScheduleTask {
        task_id: ScheduleTaskId::new(task_id),
        name: row.try_get("name")?,
        host: HostId::new(row.try_get::<String, _>("host")?),
        workspace_path: row.try_get("workspace_path")?,
        harness: decode_enum(&harness, "daemon_schedules.harness")?,
        agent_id: row.try_get("agent_id")?,
        model: row.try_get("model")?,
        runtime_mode: decode_enum(&runtime_mode, "daemon_schedules.runtime_mode")?,
        prompt: row.try_get("prompt")?,
        schedule,
        timezone,
        destination: serde_json::from_str::<ScheduleDestination>(&format!("\"{destination}\""))
            .map_err(|error| {
                JournalError::Database(sqlx::Error::Protocol(format!(
                    "unknown destination {destination:?}: {error}"
                )))
            })?,
        enabled: row.try_get::<i64, _>("enabled")? != 0,
        thread_id: row
            .try_get::<Option<String>, _>("thread_id")?
            .map(ThreadId::new),
        webhook_path: webhook_secret
            .filter(|secret| !secret.is_empty())
            .map(|secret| format!("/webhooks/{secret}")),
        webhook_path_set,
        once_consumed,
        once_consumed_slot,
        next_run_at: next_run_at.and_then(|at| {
            DateTime::parse_from_rfc3339(&at)
                .ok()
                .map(|value| value.with_timezone(&Utc))
        }),
        last_run: None,
        created_at: DateTime::parse_from_rfc3339(&created_at)
            .map(|value| value.with_timezone(&Utc))
            .map_err(|error| {
                JournalError::Database(sqlx::Error::Protocol(format!(
                    "unparsable created_at {created_at:?} in daemon_schedules: {error}"
                )))
            })?,
        updated_at: DateTime::parse_from_rfc3339(&updated_at)
            .map(|value| value.with_timezone(&Utc))
            .map_err(|error| {
                JournalError::Database(sqlx::Error::Protocol(format!(
                    "unparsable updated_at {updated_at:?} in daemon_schedules: {error}"
                )))
            })?,
    })
}

fn schedule_run_from_row(row: sqlx::sqlite::SqliteRow) -> Result<ScheduleRun, JournalError> {
    let run_id: i64 = row.try_get("run_id")?;
    let task_id: String = row.try_get("task_id")?;
    let scheduled_for: Option<String> = row.try_get("scheduled_for")?;
    let started_at: String = row.try_get("started_at")?;
    let outcome: String = row.try_get("outcome")?;
    Ok(ScheduleRun {
        run_id: u64::try_from(run_id).map_err(|_| {
            JournalError::Database(sqlx::Error::Protocol(format!(
                "run id {run_id} does not fit in daemon_schedule_runs"
            )))
        })?,
        task_id: ScheduleTaskId::new(task_id),
        scheduled_for: scheduled_for.and_then(|at| {
            DateTime::parse_from_rfc3339(&at)
                .ok()
                .map(|value| value.with_timezone(&Utc))
        }),
        started_at: DateTime::parse_from_rfc3339(&started_at)
            .map(|value| value.with_timezone(&Utc))
            .map_err(|error| {
                JournalError::Database(sqlx::Error::Protocol(format!(
                    "unparsable started_at {started_at:?} in daemon_schedule_runs: {error}"
                )))
            })?,
        thread_id: row
            .try_get::<Option<String>, _>("thread_id")?
            .map(ThreadId::new),
        outcome: decode_enum::<ScheduleRunOutcome>(&outcome, "daemon_schedule_runs.outcome")?,
        detail: row.try_get("detail")?,
    })
}

fn pr_link_from_row(row: sqlx::sqlite::SqliteRow) -> Result<PrLinkRecord, JournalError> {
    let forge: String = row.try_get("forge")?;
    let linked_at: String = row.try_get("linked_at")?;
    let number: i64 = row.try_get("number")?;
    Ok(PrLinkRecord {
        thread_id: ThreadId::new(row.try_get::<String, _>("thread_id")?),
        account_id: row.try_get("account_id")?,
        repo: ForgeRepoRef {
            forge: ForgeKind::parse(&forge).ok_or_else(|| {
                JournalError::Database(sqlx::Error::Protocol(format!(
                    "unknown forge slug {forge:?} in daemon_pr_links"
                )))
            })?,
            base_url: row.try_get("base_url")?,
            owner: row.try_get("owner")?,
            repo: row.try_get("repo")?,
        },
        number: u32::try_from(number).map_err(|_| {
            JournalError::Database(sqlx::Error::Protocol(format!(
                "PR number {number} does not fit in daemon_pr_links"
            )))
        })?,
        linked_at: DateTime::parse_from_rfc3339(&linked_at)
            .map(|value| value.with_timezone(&Utc))
            .map_err(|error| {
                JournalError::Database(sqlx::Error::Protocol(format!(
                    "unparsable linked_at {linked_at:?} in daemon_pr_links: {error}"
                )))
            })?,
    })
}

/// The stable, queryable identity of an issue scope.
fn issue_scope_parts(scope: &h4rn_protocol::IssueScope) -> (&'static str, String) {
    match scope {
        h4rn_protocol::IssueScope::Forge { repo } => (
            "forge",
            format!(
                "{}|{}|{}|{}",
                repo.forge.slug(),
                repo.base_url,
                repo.owner,
                repo.repo
            ),
        ),
        h4rn_protocol::IssueScope::Linear { team_id } => ("linear", team_id.clone()),
    }
}

fn issue_link_from_row(row: sqlx::sqlite::SqliteRow) -> Result<IssueLinkRecord, JournalError> {
    let scope_json: String = row.try_get("scope_json")?;
    let linked_at: String = row.try_get("linked_at")?;
    Ok(IssueLinkRecord {
        thread_id: ThreadId::new(row.try_get::<String, _>("thread_id")?),
        account_id: row.try_get("account_id")?,
        scope: serde_json::from_str(&scope_json).map_err(|error| {
            JournalError::Database(sqlx::Error::Protocol(format!(
                "unparsable issue scope in daemon_issue_links: {error}"
            )))
        })?,
        issue_id: row.try_get("issue_id")?,
        linked_at: DateTime::parse_from_rfc3339(&linked_at)
            .map(|value| value.with_timezone(&Utc))
            .map_err(|error| {
                JournalError::Database(sqlx::Error::Protocol(format!(
                    "unparsable linked_at {linked_at:?} in daemon_issue_links: {error}"
                )))
            })?,
    })
}

/// A helper for tests and diagnostics: the JSON shape of a thread row.
pub fn thread_meta_json(meta: &ThreadMeta) -> serde_json::Value {
    json!({
        "thread_id": meta.thread_id,
        "parent_thread_id": meta.parent_thread_id,
        "subagent_id": meta.subagent_id,
        "host": meta.host,
        "harness": meta.harness,
        "agent_id": meta.agent_id,
        "title": meta.title,
        "title_source": meta.title_source.as_str(),
        "workspace_path": meta.workspace_path,
        "model": meta.model,
        "created_at": meta.created_at,
        "runtime_mode": meta.runtime_mode,
        "reasoning_effort": meta.reasoning_effort,
        "fast": meta.fast,
        "worktree": meta.worktree,
        "usage": meta.usage,
        "resume_id": meta.resume_id,
        "agent_fingerprint": meta.agent_fingerprint,
        "parked": meta.parked,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use h4rn_protocol::{AgentState, ThreadSummary};

    fn meta(thread_id: &str) -> ThreadMeta {
        ThreadMeta {
            thread_id: ThreadId::new(thread_id),
            parent_thread_id: None,
            subagent_id: None,
            host: HostId::new("host_test"),
            harness: HarnessKind::ClaudeCode,
            agent_id: None,
            title: "first prompt".into(),
            title_source: TitleSource::Prompt,
            workspace_path: "/tmp/work".into(),
            model: "test-model".into(),
            runtime_mode: RuntimeMode::ApprovalRequired,
            reasoning_effort: None,
            fast: false,
            worktree: None,
            created_at: Utc::now(),
            usage: Usage::default(),
            resume_id: None,
            agent_fingerprint: None,
            parked: None,
            queued: Vec::new(),
            claimed: Vec::new(),
            imported: None,
            forked_from: None,
        }
    }

    fn summary() -> ThreadSummary {
        ThreadSummary {
            thread_id: ThreadId::new("th_one"),
            parent_thread_id: None,
            host: HostId::new("host_test"),
            harness: HarnessKind::ClaudeCode,
            agent_id: None,
            title: "first prompt".into(),
            workspace_path: "/tmp/work".into(),
            project_path: None,
            forked_from: None,
            branch: Some("h4rn/first-prompt".into()),
            model: "test-model".into(),
            runtime_mode: RuntimeMode::ApprovalRequired,
            reasoning_effort: None,
            reasoning_efforts: Vec::new(),
            models: Vec::new(),
            fast: false,
            tab: h4rn_protocol::ThreadTab::Working,
            placed_by_user: false,
            pinned: false,
            snoozed_until: None,
            last_activity: Utc::now(),
            agent_state: AgentState::Thinking,
            usage: Usage::default(),
            ttft_ms: None,
            context_window: None,
            context_fill_percent: None,
            needs_attention: false,
            pending_guest_requests: 0,
            unread: false,
            resume_at: None,
            limit: None,
            queued_count: 0,
            queue_revision: 0,
            active_turn_id: None,
            steer: false,
        }
    }

    #[tokio::test]
    async fn a_parked_prompt_keeps_its_mode_ceiling_across_a_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.sqlite");
        let journal = SqliteJournal::connect(&path).await.expect("journal");
        let mut one = meta("th_parked");
        one.parked = Some(ParkedPrompt {
            turn_id: h4rn_protocol::TurnId::new("turn_one"),
            text: "carry on".into(),
            attachments: Vec::new(),
            mode_ceiling: Some(RuntimeMode::ApprovalRequired),
            grant: Some(h4rn_protocol::WorkGrant {
                client_id: "client-1".into(),
                family: "family-1".into(),
                ceiling: Some(RuntimeMode::ApprovalRequired),
            }),
        });
        journal.upsert_thread(&one).await.unwrap();
        drop(journal);

        let reopened = SqliteJournal::connect(&path).await.expect("reopen");
        let rows = reopened.list_threads().await.unwrap();
        assert_eq!(rows, vec![one]);
        assert_eq!(
            rows[0]
                .parked
                .as_ref()
                .and_then(|parked| parked.mode_ceiling),
            Some(RuntimeMode::ApprovalRequired),
            "the parked prompt lost its ceiling"
        );
    }

    #[tokio::test]
    async fn a_thread_row_written_before_agent_id_migrates_with_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("old.db");
        let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .expect("open old database");
        sqlx::query(
            "CREATE TABLE daemon_threads (
                thread_id      TEXT PRIMARY KEY,
                host           TEXT NOT NULL,
                harness        TEXT NOT NULL,
                title          TEXT NOT NULL,
                workspace_path TEXT NOT NULL,
                model          TEXT NOT NULL,
                created_at     TEXT NOT NULL,
                usage_json     TEXT NOT NULL,
                resume_id      TEXT,
                runtime_mode   TEXT NOT NULL DEFAULT 'approval_required',
                worktree_json  TEXT,
                parent_thread_id TEXT,
                subagent_id    TEXT
            )",
        )
        .execute(&pool)
        .await
        .expect("old schema");
        sqlx::query(
            "INSERT INTO daemon_threads
                (thread_id, host, harness, title, workspace_path, model, created_at, usage_json)
             VALUES ('th_old', 'host_local', 'claude_code', 'old', '/tmp/work', 'm', '2026-01-01T00:00:00Z', '{}')",
        )
        .execute(&pool)
        .await
        .expect("old row");
        pool.close().await;

        let journal = SqliteJournal::connect(&path).await.expect("open journal");
        let threads = journal.list_threads().await.expect("list");
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].thread_id.as_str(), "th_old");
        assert_eq!(threads[0].agent_id, None);
        assert_eq!(threads[0].harness, HarnessKind::ClaudeCode);
    }

    #[tokio::test]
    async fn wire_sequences_are_global_and_monotonic() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        let one = meta("th_one");
        let two = meta("th_two");

        let first = journal
            .append(
                &one,
                &[ThreadEvent::TurnQueued { at: Utc::now() }],
                &[Event::ThreadUpserted(summary())],
            )
            .await
            .unwrap();
        let second = journal
            .append_wire(&two.thread_id, &[Event::ThreadUpserted(summary())])
            .await
            .unwrap();
        assert_eq!(first, vec![1]);
        assert_eq!(second, vec![2], "sequences are daemon-global");
    }

    #[tokio::test]
    async fn metadata_round_trips() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        let one = meta("th_one");
        journal.upsert_thread(&one).await.unwrap();
        let rows = journal.list_threads().await.unwrap();
        assert_eq!(rows, vec![one]);
    }

    /// A fork's lineage is part of its row, so a restart shows where it came
    /// from without any other table.
    #[tokio::test]
    async fn a_forked_row_round_trips_its_lineage() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        let mut one = meta("th_fork");
        one.forked_from = Some(ForkOrigin {
            thread_id: ThreadId::new("th_source"),
            turn_id: h4rn_protocol::TurnId::new("turn_source"),
            project_path: Some("/code/api".into()),
            native_fork: true,
            provider_turn_id: None,
        });
        journal.upsert_thread(&one).await.unwrap();
        assert_eq!(journal.list_threads().await.unwrap(), vec![one]);
    }

    /// The journal reader forks use keeps each message's own time, falls back
    /// to the journal time for a live item, and knows which turns completed.
    #[tokio::test]
    async fn journaled_turns_keep_message_times_and_completion() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        let one = meta("th_turns");
        let thread_id = one.thread_id.clone();
        let first = h4rn_protocol::TurnId::new("turn_1");
        let second = h4rn_protocol::TurnId::new("turn_2");
        let imported_at = DateTime::parse_from_rfc3339("2026-02-03T04:05:06Z")
            .unwrap()
            .with_timezone(&Utc);
        journal
            .append(
                &one,
                &[],
                &[
                    Event::TurnStarted {
                        thread_id: thread_id.clone(),
                        turn_id: first.clone(),
                        started_at: Utc::now(),
                    },
                    Event::ItemCompleted {
                        thread_id: thread_id.clone(),
                        turn_id: first.clone(),
                        item: Item::Message {
                            item_id: "i1".into(),
                            role: h4rn_protocol::MessageRole::User,
                            text: "a live prompt".into(),
                            attachments: Vec::new(),
                        },
                        at: None,
                    },
                    Event::TurnCompleted {
                        thread_id: thread_id.clone(),
                        turn_id: first.clone(),
                        usage: Usage::default(),
                        status: h4rn_protocol::TurnStatus::Completed,
                        changes: None,
                    },
                    Event::ItemCompleted {
                        thread_id: thread_id.clone(),
                        turn_id: second.clone(),
                        item: Item::Message {
                            item_id: "i2".into(),
                            role: h4rn_protocol::MessageRole::Assistant,
                            text: "an imported reply".into(),
                            attachments: Vec::new(),
                        },
                        at: Some(imported_at),
                    },
                ],
            )
            .await
            .unwrap();

        let turns = journal.read_journaled_turns(&thread_id).await.unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].turn_id, first);
        assert!(turns[0].completed());
        assert_eq!(turns[0].messages().len(), 1);
        assert_eq!(turns[0].messages()[0].1, "a live prompt");
        assert!(
            turns[0].messages()[0].2 > imported_at,
            "a live item falls back to when the journal wrote it"
        );
        assert_eq!(turns[1].turn_id, second);
        assert!(!turns[1].completed());
        assert_eq!(turns[1].messages()[0].2, imported_at);
    }

    /// The reader keeps what the transcript showed, not only its messages: a
    /// tool call, the plan, an error row, a limit row, an attachment, and a
    /// reply that only ever streamed.
    #[tokio::test]
    async fn a_journaled_turn_keeps_every_visible_record() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        let thread_id = ThreadId::new("th_items");
        let one = meta("th_items");
        let attachment = h4rn_protocol::Attachment {
            id: h4rn_protocol::AttachmentId::new("att_one"),
            name: "shot.png".into(),
            mime: "image/png".into(),
            size: 4,
            local_path: "/tmp/shot.png".into(),
        };
        let turn_id = h4rn_protocol::TurnId::new("turn_one");
        let started_at = Utc::now() - chrono::Duration::minutes(2);
        let tool_call = h4rn_protocol::ToolCall {
            tool_call_id: "call_one".into(),
            name: "Bash".into(),
            status: h4rn_protocol::ToolCallStatus::Completed,
            summary: "ls".into(),
            input: None,
            output: None,
            started_at,
            completed_at: Some(started_at),
            stats: None,
        };
        let plan = vec![h4rn_protocol::PlanStep {
            content: "look".into(),
            status: "completed".into(),
            priority: "high".into(),
        }];
        journal
            .append(
                &one,
                &[],
                &[
                    Event::TurnStarted {
                        thread_id: thread_id.clone(),
                        turn_id: turn_id.clone(),
                        started_at,
                    },
                    Event::ItemCompleted {
                        thread_id: thread_id.clone(),
                        turn_id: turn_id.clone(),
                        item: Item::Message {
                            item_id: "item_user".into(),
                            role: h4rn_protocol::MessageRole::User,
                            text: "look at this".into(),
                            attachments: vec![attachment.clone()],
                        },
                        at: Some(started_at),
                    },
                    Event::MessageDelta {
                        thread_id: thread_id.clone(),
                        turn_id: turn_id.clone(),
                        item_id: "item_reply".into(),
                        text: "half a ".into(),
                    },
                    Event::ToolCall {
                        thread_id: thread_id.clone(),
                        turn_id: turn_id.clone(),
                        tool_call: tool_call.clone(),
                    },
                    Event::Plan {
                        thread_id: thread_id.clone(),
                        turn_id: turn_id.clone(),
                        entries: plan.clone(),
                    },
                    Event::Error {
                        thread_id: Some(thread_id.clone()),
                        message: "a tool failed".into(),
                        hint: None,
                        retryable: true,
                    },
                    Event::UsageLimitHit {
                        thread_id: thread_id.clone(),
                        reset_at: None,
                        auto_resume: false,
                    },
                    Event::MessageDelta {
                        thread_id: thread_id.clone(),
                        turn_id: turn_id.clone(),
                        item_id: "item_reply".into(),
                        text: "reply".into(),
                    },
                    Event::TurnCompleted {
                        thread_id: thread_id.clone(),
                        turn_id: turn_id.clone(),
                        usage: Usage::default(),
                        status: h4rn_protocol::TurnStatus::Completed,
                        changes: None,
                    },
                ],
            )
            .await
            .unwrap();
        journal
            .note_native_turn(&thread_id, &turn_id, "provider-turn-1")
            .await
            .unwrap();

        let turns = journal.read_journaled_turns(&thread_id).await.unwrap();
        assert_eq!(turns.len(), 1);
        let turn = &turns[0];
        assert!(turn.completed());
        assert_eq!(turn.started_at, Some(started_at));
        assert_eq!(turn.native_turn_id.as_deref(), Some("provider-turn-1"));
        assert_eq!(turn.items.len(), 6, "{:?}", turn.items);
        match &turn.items[0] {
            JournaledItem::Item { item, at } => {
                assert_eq!(*at, started_at);
                assert_eq!(
                    item,
                    &Item::Message {
                        item_id: "item_user".into(),
                        role: h4rn_protocol::MessageRole::User,
                        text: "look at this".into(),
                        attachments: vec![attachment],
                    }
                );
            }
            other => panic!("expected the user message, got {other:?}"),
        }
        match &turn.items[1] {
            JournaledItem::Item { item, .. } => assert_eq!(
                item,
                &Item::Message {
                    item_id: "item_reply".into(),
                    role: h4rn_protocol::MessageRole::Assistant,
                    // A reply that only streamed is rebuilt from its deltas.
                    text: "half a reply".into(),
                    attachments: Vec::new(),
                }
            ),
            other => panic!("expected the streamed reply, got {other:?}"),
        }
        assert_eq!(turn.items[2], JournaledItem::ToolCall { tool_call });
        assert_eq!(turn.items[3], JournaledItem::Plan { entries: plan });
        assert!(matches!(
            &turn.items[4],
            JournaledItem::Error {
                message,
                retryable: true,
                ..
            } if message == "a tool failed"
        ));
        assert!(matches!(
            &turn.items[5],
            JournaledItem::Limit {
                reset_at: None,
                auto_resume: false,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn replay_filters_by_thread_and_sequence() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        let one = meta("th_one");
        let two = meta("th_two");

        journal
            .append(
                &one,
                &[ThreadEvent::TurnQueued { at: Utc::now() }],
                &[
                    Event::ThreadUpserted(summary()),
                    Event::AgentStateChanged {
                        thread_id: ThreadId::new("th_one"),
                        state: AgentState::Thinking,
                    },
                ],
            )
            .await
            .unwrap();
        journal
            .append_wire(&two.thread_id, &[Event::ThreadUpserted(summary())])
            .await
            .unwrap();

        let all = journal.read_wire_since(&one.thread_id, 0).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].seq, 1);
        assert_eq!(all[1].seq, 2);

        let tail = journal.read_wire_since(&one.thread_id, 1).await.unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].seq, 2);
        assert!(
            journal
                .read_wire_since(&one.thread_id, 2)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn head_reports_the_last_allocated_sequence() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        assert_eq!(journal.head().await.unwrap(), 0);

        let one = meta("th_one");
        journal
            .append(
                &one,
                &[ThreadEvent::TurnQueued { at: Utc::now() }],
                &[Event::ThreadUpserted(summary())],
            )
            .await
            .unwrap();
        journal
            .append_wire(&one.thread_id, &[Event::ThreadUpserted(summary())])
            .await
            .unwrap();

        assert_eq!(journal.head().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn global_replay_spans_threads_in_sequence_order() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        let one = meta("th_one");
        let two = meta("th_two");
        journal
            .append(
                &one,
                &[ThreadEvent::TurnQueued { at: Utc::now() }],
                &[Event::ThreadUpserted(summary())],
            )
            .await
            .unwrap();
        journal
            .append_wire(&two.thread_id, &[Event::ThreadUpserted(summary())])
            .await
            .unwrap();

        let replayed = journal.read_wire_since_global(1).await.unwrap();
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].seq, 2);
        assert_eq!(replayed[0].thread_id, two.thread_id);
    }

    #[tokio::test]
    async fn domain_log_is_readable_for_restart_replay() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        let one = meta("th_one");
        journal
            .append(
                &one,
                &[
                    ThreadEvent::TurnStarted { at: Utc::now() },
                    ThreadEvent::TurnCompleted {
                        status: h4rn_protocol::TurnStatus::Completed,
                        at: Utc::now(),
                    },
                ],
                &[],
            )
            .await
            .unwrap();
        let domain = journal.read_domain(&one.thread_id).await.unwrap();
        assert_eq!(domain.len(), 2);
    }

    #[tokio::test]
    async fn append_is_atomic_when_the_wire_insert_fails() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        let one = meta("th_one");
        sqlx::query("DROP TABLE daemon_wire_events")
            .execute(&journal.pool)
            .await
            .unwrap();

        let result = journal
            .append(
                &one,
                &[ThreadEvent::TurnStarted { at: Utc::now() }],
                &[Event::ThreadUpserted(summary())],
            )
            .await;
        assert!(result.is_err(), "the wire insert must fail");

        assert!(
            journal
                .read_domain(&one.thread_id)
                .await
                .unwrap()
                .is_empty(),
            "a failed append must not leave a domain event behind"
        );
        assert!(
            journal.list_threads().await.unwrap().is_empty(),
            "a failed append must not leave the thread row behind"
        );
    }

    #[tokio::test]
    async fn a_claimed_source_commits_its_thread_once() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        let imported_meta = meta("th_imported");
        let claim = ImportedSourceRecord {
            key: "claude_code:session-1".into(),
            thread_id: imported_meta.thread_id.clone(),
            path: "/tmp/session-1.jsonl".into(),
            imported_at: Utc::now(),
        };
        let wire = vec![Event::ThreadUpserted(summary())];

        let first = journal
            .commit_thread_import(&claim, &imported_meta, &[], &wire)
            .await
            .unwrap();
        assert!(first.is_some(), "the first claim writes the thread");
        assert_eq!(journal.list_threads().await.unwrap().len(), 1);

        // A second run with the same source writes nothing, as an overlapping
        // run or a retry after a completed import would.
        let second_meta = meta("th_other");
        let mut second_claim = claim.clone();
        second_claim.thread_id = second_meta.thread_id.clone();
        let second = journal
            .commit_thread_import(&second_claim, &second_meta, &[], &wire)
            .await
            .unwrap();
        assert!(second.is_none(), "an existing claim is not imported again");
        let metas = journal.list_threads().await.unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].thread_id.as_str(), "th_imported");
    }

    #[tokio::test]
    async fn a_failed_import_write_leaves_no_claim_behind() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        let imported_meta = meta("th_imported");
        let claim = ImportedSourceRecord {
            key: "claude_code:session-2".into(),
            thread_id: imported_meta.thread_id.clone(),
            path: "/tmp/session-2.jsonl".into(),
            imported_at: Utc::now(),
        };
        // A wire event that cannot be written rolls the claim back with it.
        let wire = vec![Event::ThreadUpserted(summary())];
        sqlx::query("CREATE TRIGGER fail_wire BEFORE INSERT ON daemon_wire_events BEGIN SELECT RAISE(ABORT, 'injected failure'); END")
            .execute(&journal.pool)
            .await
            .unwrap();
        assert!(
            journal
                .commit_thread_import(&claim, &imported_meta, &[], &wire)
                .await
                .is_err()
        );
        assert!(journal.list_imported_sources().await.unwrap().is_empty());
        assert!(journal.list_threads().await.unwrap().is_empty());
    }

    fn page_event(thread_id: &ThreadId, index: usize, text_bytes: usize) -> Event {
        Event::ItemCompleted {
            thread_id: thread_id.clone(),
            turn_id: h4rn_protocol::TurnId::new("turn_page"),
            item: h4rn_protocol::Item::Message {
                item_id: format!("item-{index}"),
                role: h4rn_protocol::MessageRole::User,
                text: "p".repeat(text_bytes),
                attachments: Vec::new(),
            },
            at: None,
        }
    }

    /// The page boundaries the pre-SQL pager produced, for comparison.
    fn old_pages(events: &[WireEvent], budget: usize) -> Vec<(Vec<u64>, bool)> {
        let mut pages = Vec::new();
        let mut after = 0;
        loop {
            let rest: Vec<&WireEvent> = events.iter().filter(|event| event.seq > after).collect();
            if rest.is_empty() {
                break;
            }
            let mut used: usize = 0;
            let mut cut = rest.len();
            for (index, event) in rest.iter().enumerate() {
                let size = serde_json::to_string(&event.event)
                    .map(|text| text.len())
                    .unwrap_or_default();
                if index > 0 && used.saturating_add(size) > budget {
                    cut = index;
                    break;
                }
                used = used.saturating_add(size);
            }
            let more = cut < rest.len();
            let page: Vec<u64> = rest[..cut].iter().map(|event| event.seq).collect();
            after = *page.last().expect("a page is never empty");
            pages.push((page, more));
            if !more {
                break;
            }
        }
        pages
    }

    #[tokio::test]
    async fn a_thread_with_gaps_pages_where_the_old_pager_did() {
        const PAGE: usize = 64 * 1024;
        let journal = SqliteJournal::in_memory().await.unwrap();
        let first = ThreadId::new("th_gaps_a");
        let second = ThreadId::new("th_gaps_b");
        journal.upsert_thread(&meta("th_gaps_a")).await.unwrap();
        journal.upsert_thread(&meta("th_gaps_b")).await.unwrap();
        // Both threads share one AUTOINCREMENT sequence, as real threads do,
        // so each thread's own seqs have gaps wherever the other wrote.
        for index in 0..60 {
            let thread_id = if index % 2 == 0 { &first } else { &second };
            journal
                .append_wire(thread_id, &[page_event(thread_id, index, 4 * 1024 + index)])
                .await
                .unwrap();
        }
        // A deleted row leaves a second kind of hole, deeper than a single
        // missing neighbour, that `seq + 1` stepping cannot cross.
        let before_delete = journal.read_wire_since(&first, 0).await.unwrap();
        let removed = before_delete[10].seq;
        sqlx::query("DELETE FROM daemon_wire_events WHERE thread_id = ? AND seq = ?")
            .bind(first.as_str())
            .bind(removed as i64)
            .execute(&journal.pool)
            .await
            .unwrap();

        let all = journal.read_wire_since(&first, 0).await.unwrap();
        assert!(
            all.iter().any(|event| event.seq > removed),
            "events continue after the deleted row"
        );
        assert!(
            all.windows(2).any(|pair| pair[1].seq > pair[0].seq + 1),
            "the thread's seqs are not contiguous"
        );
        let expected = old_pages(&all, PAGE);

        let mut after = 0;
        let mut actual = Vec::new();
        loop {
            let page = journal
                .read_wire_page(&first, after, 4096, PAGE)
                .await
                .unwrap();
            assert!(!page.events.is_empty(), "a non-final page is never empty");
            let seqs: Vec<u64> = page.events.iter().map(|event| event.seq).collect();
            after = *seqs.last().unwrap();
            actual.push((seqs, page.more));
            if !page.more {
                break;
            }
        }
        assert_eq!(actual, expected, "pages and cursors match the old pager");
    }

    #[tokio::test]
    async fn a_page_takes_the_event_that_exactly_fills_its_budget() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        let thread_id = ThreadId::new("th_exact");
        journal.upsert_thread(&meta("th_exact")).await.unwrap();
        journal
            .append_wire(
                &thread_id,
                &[
                    page_event(&thread_id, 0, 6 * 1024),
                    page_event(&thread_id, 1, 5 * 1024),
                    page_event(&thread_id, 2, 6 * 1024),
                ],
            )
            .await
            .unwrap();

        // The budget counts the stored text, so read those exact lengths: the
        // first two events have to fit it to the byte.
        let stored: Vec<i64> = sqlx::query_scalar(
            "SELECT length(CAST(event_json AS BLOB)) FROM daemon_wire_events
              WHERE thread_id = ? ORDER BY seq",
        )
        .bind(thread_id.as_str())
        .fetch_all(&journal.pool)
        .await
        .unwrap();
        let budget = (stored[0] + stored[1]) as usize;
        let page = journal
            .read_wire_page(&thread_id, 0, 4096, budget)
            .await
            .unwrap();
        assert_eq!(
            page.events.len(),
            2,
            "an event that exactly fills the remaining budget is included"
        );
        assert!(page.more, "the third event does not fit");
    }

    #[tokio::test]
    async fn a_page_stops_at_its_byte_budget() {
        const PAGE: usize = 25 * 1024;
        let journal = SqliteJournal::in_memory().await.unwrap();
        let thread_id = ThreadId::new("th_budget");
        journal.upsert_thread(&meta("th_budget")).await.unwrap();
        journal
            .append_wire(
                &thread_id,
                &[
                    page_event(&thread_id, 0, 10 * 1024),
                    page_event(&thread_id, 1, 10 * 1024),
                    page_event(&thread_id, 2, 10 * 1024),
                ],
            )
            .await
            .unwrap();

        let page = journal
            .read_wire_page(&thread_id, 0, 4096, PAGE)
            .await
            .unwrap();
        let used: usize = page
            .events
            .iter()
            .map(|event| serde_json::to_string(&event.event).unwrap().len())
            .sum();
        assert_eq!(
            page.events.len(),
            2,
            "the third event would pass the budget"
        );
        assert!(used <= PAGE, "{used} bytes fit in {PAGE}");
        assert!(page.more, "the rest follows in another page");

        // An event larger than the whole budget still arrives whole.
        let oversized = journal
            .read_wire_page(&thread_id, 0, 4096, 1024)
            .await
            .unwrap();
        assert_eq!(oversized.events.len(), 1);
        assert_eq!(oversized.events[0].seq, page.events[0].seq);
        assert!(oversized.more);

        // The budget counts the serialized bytes, not characters: this event
        // is about 25 KiB but only 6,340 characters, so a character budget
        // would wrongly let a second one in.
        let wide = ThreadId::new("th_wide");
        journal.upsert_thread(&meta("th_wide")).await.unwrap();
        journal
            .append_wire(
                &wide,
                &[
                    Event::ItemCompleted {
                        thread_id: wide.clone(),
                        turn_id: h4rn_protocol::TurnId::new("turn_page"),
                        item: h4rn_protocol::Item::Message {
                            item_id: "wide-0".into(),
                            role: h4rn_protocol::MessageRole::User,
                            text: "🙂".repeat(6_340),
                            attachments: Vec::new(),
                        },
                        at: None,
                    },
                    page_event(&wide, 1, 1),
                ],
            )
            .await
            .unwrap();
        let wide_page = journal.read_wire_page(&wide, 0, 4096, PAGE).await.unwrap();
        assert_eq!(
            wide_page.events.len(),
            1,
            "one ~25 KiB event fills the page even though it is few characters"
        );
    }

    #[tokio::test]
    async fn the_page_query_reads_only_one_page() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        let thread_id = ThreadId::new("th_rows");
        journal.upsert_thread(&meta("th_rows")).await.unwrap();
        let mut wire = Vec::new();
        for index in 0..200 {
            wire.push(page_event(&thread_id, index, 1024));
        }
        journal.append_wire(&thread_id, &wire).await.unwrap();

        WIRE_PAGE_ROWS_FETCHED.with(|fetched| fetched.set(0));
        let page = journal
            .read_wire_page(&thread_id, 0, 4096, 8 * 1024)
            .await
            .unwrap();
        let fetched = WIRE_PAGE_ROWS_FETCHED.with(|fetched| fetched.get());
        assert_eq!(
            fetched,
            page.events.len(),
            "the query returned only the page"
        );
        assert!(
            page.events.len() < 200,
            "the page is a small prefix, not the whole thread"
        );
        assert!(page.more);
    }

    #[tokio::test]
    async fn an_unparsable_pr_link_timestamp_is_a_journal_error() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        let thread_id = ThreadId::new("th_one");
        journal
            .upsert_pr_link(&PrLinkRecord {
                thread_id: thread_id.clone(),
                account_id: "acct_1".into(),
                repo: ForgeRepoRef {
                    forge: ForgeKind::GitHub,
                    base_url: "https://github.com".into(),
                    owner: "h4rn".into(),
                    repo: "api".into(),
                },
                number: 7,
                linked_at: Utc::now(),
            })
            .await
            .unwrap();

        // A row written by a damaged file or an older schema: the timestamp is
        // not RFC 3339. The row is rejected, never silently stamped "now".
        sqlx::query("UPDATE daemon_pr_links SET linked_at = 'not-a-time'")
            .execute(&journal.pool)
            .await
            .unwrap();

        let error = journal
            .pr_links_for_thread(&thread_id)
            .await
            .expect_err("an unparsable linked_at must not be replaced with now");
        assert!(
            matches!(
                error,
                JournalError::Database(sqlx::Error::Protocol(ref message))
                    if message.contains("linked_at")
            ),
            "{error:?}"
        );
    }

    fn subagent_view(
        state: h4rn_protocol::SubagentState,
        agent_state: h4rn_protocol::AgentState,
    ) -> h4rn_protocol::SubagentView {
        h4rn_protocol::SubagentView {
            subagent_id: SubagentId::new("sa_one"),
            parent_thread_id: ThreadId::new("th_items"),
            child_thread_id: Some(ThreadId::new("th_child")),
            task: "do the thing".into(),
            harness: HarnessKind::Codex,
            host: HostId::new("host_test"),
            model: "test-model".into(),
            isolation: h4rn_protocol::SubagentIsolation::Shared,
            workspace_path: "/tmp/work".into(),
            branch: None,
            depth: 1,
            state,
            agent_state,
            progress: Default::default(),
            budget: Default::default(),
            context: h4rn_protocol::SubagentContext::None,
            credential: h4rn_protocol::SubagentCredential {
                label: "parent".into(),
                shared_with_parent: true,
            },
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            result: None,
        }
    }

    /// A thread-only error names the open turn, not the prompt a later message
    /// queued under its own id while the turn was still running.
    #[tokio::test]
    async fn an_error_belongs_to_the_open_turn_not_a_queued_prompt() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        let one = meta("th_queue");
        let thread_id = one.thread_id.clone();
        let running = h4rn_protocol::TurnId::new("turn_a");
        let queued = h4rn_protocol::TurnId::new("turn_b");
        journal
            .append(
                &one,
                &[],
                &[
                    Event::TurnStarted {
                        thread_id: thread_id.clone(),
                        turn_id: running.clone(),
                        started_at: Utc::now(),
                    },
                    Event::ItemCompleted {
                        thread_id: thread_id.clone(),
                        turn_id: running.clone(),
                        item: Item::Message {
                            item_id: "item_a".into(),
                            role: h4rn_protocol::MessageRole::User,
                            text: "the first prompt".into(),
                            attachments: Vec::new(),
                        },
                        at: None,
                    },
                    Event::ItemCompleted {
                        thread_id: thread_id.clone(),
                        turn_id: queued.clone(),
                        item: Item::Message {
                            item_id: "item_b".into(),
                            role: h4rn_protocol::MessageRole::User,
                            text: "the queued prompt".into(),
                            attachments: Vec::new(),
                        },
                        at: None,
                    },
                    Event::Error {
                        thread_id: Some(thread_id.clone()),
                        message: "the provider failed".into(),
                        hint: None,
                        retryable: true,
                    },
                ],
            )
            .await
            .unwrap();

        let turns = journal.read_journaled_turns(&thread_id).await.unwrap();
        let a = turns
            .iter()
            .find(|turn| turn.turn_id == running)
            .expect("the running turn");
        assert!(
            a.items.iter().any(|item| matches!(
                item,
                JournaledItem::Error { message, .. } if message == "the provider failed"
            )),
            "{:?}",
            a.items
        );
        let b = turns
            .iter()
            .find(|turn| turn.turn_id == queued)
            .expect("the queued turn");
        assert!(
            !b.items.iter().any(|item| matches!(
                item,
                JournaledItem::Error { .. } | JournaledItem::Limit { .. }
            )),
            "a queued prompt must not take the running turn's row: {:?}",
            b.items
        );
    }

    /// An older journal wrote a queued prompt under the running turn. The
    /// reader hands it to the next turn that starts instead of copying it into
    /// the turn it waited behind.
    #[tokio::test]
    async fn an_old_queued_prompt_moves_to_the_turn_it_runs_as() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        let one = meta("th_legacy");
        let thread_id = one.thread_id.clone();
        let a = h4rn_protocol::TurnId::new("turn_a");
        let b = h4rn_protocol::TurnId::new("turn_b");
        journal
            .append(
                &one,
                &[],
                &[
                    Event::TurnStarted {
                        thread_id: thread_id.clone(),
                        turn_id: a.clone(),
                        started_at: Utc::now(),
                    },
                    Event::ItemCompleted {
                        thread_id: thread_id.clone(),
                        turn_id: a.clone(),
                        item: Item::Message {
                            item_id: "item_a".into(),
                            role: h4rn_protocol::MessageRole::User,
                            text: "A prompt".into(),
                            attachments: Vec::new(),
                        },
                        at: None,
                    },
                    // The old live path filed this under the running turn.
                    Event::ItemCompleted {
                        thread_id: thread_id.clone(),
                        turn_id: a.clone(),
                        item: Item::Message {
                            item_id: "item_b".into(),
                            role: h4rn_protocol::MessageRole::User,
                            text: "B prompt".into(),
                            attachments: Vec::new(),
                        },
                        at: None,
                    },
                    Event::TurnCompleted {
                        thread_id: thread_id.clone(),
                        turn_id: a.clone(),
                        usage: Usage::default(),
                        status: h4rn_protocol::TurnStatus::Completed,
                        changes: None,
                    },
                    Event::TurnStarted {
                        thread_id: thread_id.clone(),
                        turn_id: b.clone(),
                        started_at: Utc::now(),
                    },
                    Event::TurnCompleted {
                        thread_id: thread_id.clone(),
                        turn_id: b.clone(),
                        usage: Usage::default(),
                        status: h4rn_protocol::TurnStatus::Completed,
                        changes: None,
                    },
                ],
            )
            .await
            .unwrap();

        let turns = journal.read_journaled_turns(&thread_id).await.unwrap();
        let first = turns.iter().find(|turn| turn.turn_id == a).expect("A");
        let prompts: Vec<&str> = first
            .messages()
            .into_iter()
            .map(|(_, text, _)| text)
            .collect();
        assert_eq!(prompts, vec!["A prompt"], "A owns only its own prompt");
        let second = turns.iter().find(|turn| turn.turn_id == b).expect("B");
        let prompts: Vec<&str> = second
            .messages()
            .into_iter()
            .map(|(_, text, _)| text)
            .collect();
        assert_eq!(prompts, vec!["B prompt"], "B receives the queued prompt");
    }

    /// A subagent row copied from the spawn event keeps the child's final
    /// state, not a permanent "thinking" snapshot.
    #[tokio::test]
    async fn a_delegated_row_keeps_its_final_state() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        let one = meta("th_items");
        let thread_id = one.thread_id.clone();
        let turn_id = h4rn_protocol::TurnId::new("turn_one");
        let running = subagent_view(
            h4rn_protocol::SubagentState::Running,
            h4rn_protocol::AgentState::Thinking,
        );
        let finished = subagent_view(
            h4rn_protocol::SubagentState::Completed,
            h4rn_protocol::AgentState::Done,
        );
        journal
            .append(
                &one,
                &[],
                &[
                    Event::TurnStarted {
                        thread_id: thread_id.clone(),
                        turn_id: turn_id.clone(),
                        started_at: Utc::now(),
                    },
                    Event::ItemCompleted {
                        thread_id: thread_id.clone(),
                        turn_id: turn_id.clone(),
                        item: Item::Delegated {
                            item_id: "item_delegated_sa_one".into(),
                            subagent_id: SubagentId::new("sa_one"),
                            thread_id: Some(ThreadId::new("th_child")),
                            task: "do the thing".into(),
                            harness: HarnessKind::Codex,
                            host: HostId::new("host_test"),
                            model: "test-model".into(),
                            state: h4rn_protocol::SubagentState::Running,
                            agent_state: h4rn_protocol::AgentState::Thinking,
                        },
                        at: None,
                    },
                    Event::SubagentSpawned {
                        parent_thread_id: thread_id.clone(),
                        subagent: running,
                    },
                    Event::SubagentFinished {
                        parent_thread_id: thread_id.clone(),
                        subagent: finished,
                    },
                ],
            )
            .await
            .unwrap();

        let turns = journal.read_journaled_turns(&thread_id).await.unwrap();
        let row = turns[0]
            .items
            .iter()
            .find_map(|item| match item {
                JournaledItem::Item {
                    item:
                        Item::Delegated {
                            state, agent_state, ..
                        },
                    ..
                } => Some((*state, *agent_state)),
                _ => None,
            })
            .expect("the delegated row");
        assert_eq!(
            row,
            (
                h4rn_protocol::SubagentState::Completed,
                h4rn_protocol::AgentState::Done
            )
        );
    }

    /// A completion's usage, change set and real status are part of the turn a
    /// fork copies, so its receipt and "N files changed" card are not invented.
    #[tokio::test]
    async fn a_completion_keeps_usage_changes_and_status() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        let one = meta("th_items");
        let thread_id = one.thread_id.clone();
        let turn_id = h4rn_protocol::TurnId::new("turn_one");
        let usage = Usage {
            input_tokens: 12,
            output_tokens: 5,
            cached_input_tokens: 3,
            cost_micros: 900,
            duration_ms: 1_500,
            generation_ms: 1_200,
            tps: 4,
        };
        let changes = h4rn_protocol::FsTurnChanges {
            thread_id: thread_id.clone(),
            turn_id: turn_id.clone(),
            files: vec![h4rn_protocol::FsTurnFile {
                path: "src/lib.rs".into(),
                added: 4,
                removed: 1,
                before_etag: None,
                after_etag: Some("etag".into()),
            }],
            note: None,
        };
        journal
            .append(
                &one,
                &[],
                &[
                    Event::TurnStarted {
                        thread_id: thread_id.clone(),
                        turn_id: turn_id.clone(),
                        started_at: Utc::now(),
                    },
                    Event::TurnCompleted {
                        thread_id: thread_id.clone(),
                        turn_id: turn_id.clone(),
                        usage: usage.clone(),
                        status: h4rn_protocol::TurnStatus::Interrupted,
                        changes: Some(changes.clone()),
                    },
                ],
            )
            .await
            .unwrap();

        let turns = journal.read_journaled_turns(&thread_id).await.unwrap();
        assert_eq!(turns[0].usage, Some(usage));
        assert_eq!(turns[0].changes, Some(changes));
        assert_eq!(
            turns[0].status,
            Some(h4rn_protocol::TurnStatus::Interrupted)
        );
    }
}
