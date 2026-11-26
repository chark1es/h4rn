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

use std::collections::{BTreeMap, HashSet};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use h4rn_core::{EventStore, SqliteEventStore, StoreError, ThreadEvent};
use h4rn_protocol::{
    Event, ForgeKind, ForgeRepoRef, FsTurnChanges, HarnessKind, HostId, Item, RuntimeMode,
    ScheduleDestination, ScheduleRun, ScheduleRunOutcome, ScheduleSpec, ScheduleTask,
    ScheduleTaskId, SubagentId, ThreadId, TurnId, Usage, WorktreeRequest,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{Row, SqlitePool};

use crate::issue_links::IssueLinkRecord;
use crate::pr_links::{PrLinkRecord, StoredPrWatch};

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

/// The git registration a thread's linked worktree had when it was created.
///
/// A rewind verifies this at restore time: a moved worktree whose old path now
/// holds a different checkout must never receive its files.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeIdentity {
    /// The checkout's own git dir (`rev-parse --git-dir`).
    pub git_dir: String,
    /// The repository's common git dir (`rev-parse --git-common-dir`).
    pub common_dir: String,
}

/// The provider's own id for one delivered prompt.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderTurnRecord {
    /// Codex's provider turn id.
    #[serde(default)]
    pub turn_id: Option<String>,
    /// Claude Code's uuid for the assistant message before the prompt.
    #[serde(default)]
    pub last_message_id: Option<String>,
    /// The provider session that recorded this boundary.
    ///
    /// A boundary only truncates the history of the session that produced it.
    /// A record from an older spelling of this row has none, and is read as
    /// "not native": a handoff is safer than pairing the id with a session
    /// that never saw it.
    #[serde(default)]
    pub provider_session: Option<String>,
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
    /// Model ids the thread's last session advertised.
    ///
    /// Codex publishes its `model/list` at session start. Keeping the list on
    /// the thread row means a daemon restart does not fall back to the
    /// adapter's static guess: the picker and a new thread on that host start
    /// from what the installed CLI really offers. A new session that reports a
    /// catalogue replaces it; a harness switch clears it.
    pub advertised_models: Vec<String>,
    /// Whether each model the thread's sessions described offers a fast tier.
    ///
    /// The fast tier belongs to the model, not the harness, so the answer is
    /// kept per model id beside `advertised_models`: a session's catalogue
    /// can describe every model it lists, and a restart keeps those answers.
    /// A model with no entry is unknown — never "no tier" and never the
    /// harness's answer for someone else's model.
    pub model_fast: BTreeMap<String, bool>,
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
    /// A provider-history rewind the next session start must apply.
    ///
    /// The thread's transcript is already truncated when this is written; it
    /// survives a restart so the provider conversation is truncated too, even
    /// if the daemon stops before the next prompt.
    pub pending_rewind: Option<h4rn_protocol::PendingRewind>,
    /// The git registration this thread's linked worktree had at creation.
    pub worktree_identity: Option<WorktreeIdentity>,
    /// The provider ids of each delivered prompt, keyed by the thread's turn.
    pub provider_turns: std::collections::BTreeMap<String, ProviderTurnRecord>,

    /// The switch this thread's durable row last committed to.
    ///
    /// A late write from a session the thread has already left (clearing the
    /// handoff flag, say) names the generation it saw. The row only accepts a
    /// change that still matches, so an old write can never undo a later
    /// switch's metadata.
    pub switch_generation: u64,
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
    /// Whether the next turn must open with a bounded handoff of the journaled
    /// history.
    ///
    /// A harness switch clears the old resume id, so the first turn on the new
    /// harness has no native context to resume. It sends the history instead,
    /// using the same builder and budget as an import fallback. Cleared once the
    /// session accepts the prompt that carried the handoff, so a failed start
    /// does not lose the history a later message needs.
    pub handoff_pending: bool,
}

impl ThreadMeta {
    /// The fast answer for the row's current model, when a session described it.
    ///
    /// `None` means genuinely unknown — no session has described this model —
    /// so the client hides the row instead of borrowing the harness's answer
    /// for a model it never ran.
    pub fn fast_available(&self) -> Option<bool> {
        self.model_fast.get(&self.model).copied()
    }

    /// Records the fast answers one session's catalogue reported.
    ///
    /// `current` is the model the session really runs; its own answer is
    /// recorded under that id, so a model picked while the session was
    /// starting never inherits the answer for the model it replaced.
    pub fn record_fast_answers(
        &mut self,
        current: &str,
        fast: Option<bool>,
        fast_by_model: &BTreeMap<String, bool>,
    ) {
        self.model_fast.extend(
            fast_by_model
                .iter()
                .map(|(model, available)| (model.clone(), *available)),
        );
        if let Some(fast) = fast
            && !current.trim().is_empty()
        {
            self.model_fast.insert(current.to_string(), fast);
        }
    }
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
    /// The transcript item id this message must be journaled under, if any.
    ///
    /// A guest Send names its request's id. The row keeps it, so a prompt that
    /// waits in the queue and is promoted later is journaled under the same id
    /// and recovery can still answer the guest from the journal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
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
    /// Whether the message waited in the thread's queue before it was claimed.
    ///
    /// An ordinary send to an idle thread also writes a claim, but its prompt is
    /// already the turn's journaled user item: a `claimed` one (the adapter was
    /// never called) is dropped at startup instead of being queued again. A
    /// `sending` one still becomes the visible unsure row, because the adapter
    /// may have taken it and the transcript cannot say.
    ///
    /// An older daemon wrote no such field, and every promoted prompt it wrote
    /// a claim for came from the queue, so a missing one must read back as a
    /// promotion: treating it as an ordinary send would drop the only copy.
    #[serde(default = "claim_from_queue_default")]
    pub from_queue: bool,
}

/// The value `from_queue` had before the field was recorded.
fn claim_from_queue_default() -> bool {
    true
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
    /// The item ids in `items` that are steers written into the running turn.
    ///
    /// A fork keeps this mark for its own copy, or a later fork would read the
    /// steered row as an older daemon's queued prompt and drop it (ADR-047).
    pub steers: Vec<String>,
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
    /// Prompt text exactly as the person sent it, without any handoff block.
    pub text: String,
    /// The journaled id of the user message this prompt was recorded as.
    ///
    /// A parked prompt is also still in the transcript; keeping its item id
    /// lets the next handoff leave the prompt out of its own history.
    #[serde(default)]
    pub item_id: Option<String>,
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

/// One row the page walk yielded.
///
/// The walk visits the page plus the first row that did not fit. That last row
/// is only the `more` flag: its stored event text is not selected, so the page
/// never fetches the body of an event it is going to drop.
struct PageRow {
    /// Stored sequence number.
    seq: u64,
    /// The stored event text, absent on the lookahead row.
    event_json: Option<String>,
    /// Whether this row is past the page.
    stopped: bool,
}

// A write a test can land between the page statement and its answer.
//
// The page and its `more` flag come from one statement, so in production there
// is no window between them. The hook exists to prove that: with the old
// two-statement probe, an append placed here turns an empty page into
// `more=true` at an unchanged cursor.
#[cfg(test)]
pub(crate) mod page_hook {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    use h4rn_protocol::{Event, ThreadId};

    use super::SqliteJournal;

    /// The writer and the event it will append.
    pub(crate) struct Between {
        /// The journal instance whose next page read fires the hook.
        pub(crate) reader: u64,
        /// A separate connection, standing in for another writer.
        pub(crate) journal: SqliteJournal,
        /// Thread the event belongs to.
        pub(crate) thread_id: ThreadId,
        /// Event to append.
        pub(crate) events: Vec<Event>,
    }

    static BETWEEN: Mutex<Option<Between>> = Mutex::new(None);
    static NEXT_INSTANCE: AtomicU64 = AtomicU64::new(1);

    /// A new journal instance's identity, allocated when it opens.
    pub(crate) fn next_instance() -> u64 {
        NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed)
    }

    /// Arms the next matching page read to append just before it answers.
    pub(crate) fn set(between: Between) {
        *BETWEEN
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(between);
    }

    /// Takes the armed write when it belongs to this exact read.
    ///
    /// A page read from another journal instance or another thread leaves the
    /// arm in place: a test running in parallel cannot consume it, so its own
    /// snapshot test always finds the append it armed.
    pub(crate) fn take(reader: u64, thread_id: &ThreadId) -> Option<Between> {
        let mut armed = BETWEEN
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match armed.as_ref() {
            Some(between) if between.reader == reader && between.thread_id == *thread_id => {
                armed.take()
            }
            _ => None,
        }
    }
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
    /// A one-time upgrade cannot be applied without losing or merging data.
    #[error("journal upgrade refused: {0}")]
    Migration(String),
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

    /// Appends a sent prompt's events and its undelivered marker in one commit.
    ///
    /// A Send is one fact: the message the person wrote is in the log *and* a
    /// provider has not taken it yet. Committing the two separately lets a
    /// failed write keep the message row while the marker is gone (a later
    /// handoff silently drops the request) or keep the marker while the event
    /// is gone (an undelivered item nothing can deliver). Either way the thread
    /// is left describing work that does not exist.
    async fn append_sent(
        &self,
        meta: &ThreadMeta,
        domain: &[ThreadEvent],
        wire: &[Event],
        item_id: &str,
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
    /// stored text would pass `max_bytes`, so a long replay never loads the
    /// whole thread to hand back its first page. The page and its `more` flag
    /// come from one statement, so they are one snapshot. The first event is
    /// always included even when it alone is larger than the budget: an
    /// oversized event is carried whole in its own page rather than dropped or
    /// cut.
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

    /// Whether one thread's journal already holds `request_id`'s answer.
    ///
    /// Send, Dismiss and every sweep retry ask this before appending, so a
    /// whole-thread read would make each answer cost the whole conversation.
    /// The SQL journal filters on the request id inside the stored event; a
    /// journal without SQL falls back to the full read.
    async fn guest_resolution_exists(
        &self,
        thread_id: &ThreadId,
        request_id: &str,
    ) -> Result<bool, JournalError> {
        Ok(self
            .read_wire_since(thread_id, 0)
            .await?
            .into_iter()
            .any(|entry| {
                matches!(
                    entry.event,
                    Event::GuestRequestResolved { request_id: id, .. } if id == request_id
                )
            }))
    }

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

    /// Appends a steer's transcript row and marks it as part of its turn.
    ///
    /// A steer is written into the turn that was running, next to its own
    /// prompt. The mark is what lets a replay tell it from a prompt an older
    /// journal filed under the running turn, which belongs to the next turn
    /// that starts instead (ADR-047).
    async fn append_steer(
        &self,
        meta: &ThreadMeta,
        domain: &[ThreadEvent],
        wire: &[Event],
        steer_item: &str,
    ) -> Result<Vec<u64>, JournalError>;

    /// Appends a fork's copied transcript with the steers it keeps.
    ///
    /// A copy reproduces the source's rows, including the steers written into
    /// a running turn. Those rows need their own marks under the fork's thread
    /// id in the same append, or a replay of the fork cannot tell them from a
    /// queued prompt and silently drops them from a later fork.
    async fn append_fork(
        &self,
        meta: &ThreadMeta,
        domain: &[ThreadEvent],
        wire: &[Event],
        steer_items: &[&str],
    ) -> Result<Vec<u64>, JournalError>;

    /// Clears the handoff flag only when the row is still on `switch_generation`.
    ///
    /// A session that accepted a handoff may report it after the thread has
    /// switched again. This changes that one flag and nothing else, and only
    /// while the switch the session belongs to is still the durable one, so a
    /// late write can never restore an old harness, model or resume id.
    async fn clear_handoff_pending(
        &self,
        thread_id: &ThreadId,
        switch_generation: u64,
    ) -> Result<bool, JournalError>;

    /// Records that a dead imported resume fell back to a handoff.
    ///
    /// Only the fallback's own fields change (the resume id is cleared, the
    /// handoff flag is set and the import's native resume is spent), and only
    /// while the row still belongs to the switch the fallback saw. A fallback
    /// that finishes after the thread moved again therefore cannot restore the
    /// old harness's metadata or spend the new one's one native resume.
    async fn fall_back_from_native_resume(
        &self,
        thread_id: &ThreadId,
        switch_generation: u64,
        rewind_fallback: bool,
    ) -> Result<bool, JournalError>;

    /// Forgets a pending item once a provider accepted its prompt.
    async fn mark_item_accepted(
        &self,
        thread_id: &ThreadId,
        item_id: &str,
    ) -> Result<(), JournalError>;

    /// Every journaled user item no provider has accepted, oldest row first.
    async fn pending_item_ids(&self, thread_id: &ThreadId) -> Result<Vec<String>, JournalError>;

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

    /// Replaces every persisted PR watch with `watches`.
    ///
    /// One snapshot, written in one transaction, so a watcher that ended by
    /// any path cannot be left behind and restored after a restart. The grant
    /// travels with the watcher.
    async fn replace_pr_watches(&self, watches: &[StoredPrWatch]) -> Result<(), JournalError> {
        let _ = watches;
        Ok(())
    }

    /// Every persisted PR watch, oldest first.
    async fn pr_watches(&self) -> Result<Vec<StoredPrWatch>, JournalError> {
        Ok(Vec::new())
    }

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
    /// Identifies this journal in tests, so a page hook fires only for its own
    /// read.
    #[cfg(test)]
    instance: u64,
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
        let journal = Self {
            store,
            pool,
            #[cfg(test)]
            instance: page_hook::next_instance(),
        };
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
                model_fast_json TEXT NOT NULL DEFAULT '{}',
                advertised_models_json TEXT NOT NULL DEFAULT '[]',
                worktree_json  TEXT,
                parent_thread_id TEXT,
                subagent_id    TEXT,
                agent_id       TEXT,
                agent_fingerprint TEXT,
                parked_json    TEXT,
                imported_json  TEXT,
                rewind_json    TEXT,
                worktree_identity_json TEXT,
                provider_turns_json TEXT,
                forked_json    TEXT,
                handoff_pending INTEGER NOT NULL DEFAULT 0,
                switch_generation INTEGER NOT NULL DEFAULT 0,
                queued_json    TEXT NOT NULL DEFAULT '[]',
                claimed_json   TEXT NOT NULL DEFAULT '[]'
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
        self.ensure_column("daemon_threads", "rewind_json", "TEXT")
            .await?;
        self.ensure_column("daemon_threads", "worktree_identity_json", "TEXT")
            .await?;
        self.ensure_column("daemon_threads", "provider_turns_json", "TEXT")
            .await?;
        self.ensure_column("daemon_threads", "forked_json", "TEXT")
            .await?;
        self.ensure_column(
            "daemon_threads",
            "handoff_pending",
            "INTEGER NOT NULL DEFAULT 0",
        )
        .await?;
        self.ensure_column(
            "daemon_threads",
            "switch_generation",
            "INTEGER NOT NULL DEFAULT 0",
        )
        .await?;
        self.ensure_column("daemon_threads", "reasoning_effort", "TEXT")
            .await?;
        self.ensure_column("daemon_threads", "fast", "INTEGER NOT NULL DEFAULT 0")
            .await?;
        self.ensure_column(
            "daemon_threads",
            "model_fast_json",
            "TEXT NOT NULL DEFAULT '{}'",
        )
        .await?;
        self.ensure_column(
            "daemon_threads",
            "advertised_models_json",
            "TEXT NOT NULL DEFAULT '[]'",
        )
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

        // A guest answer is looked up by request id before every Send, Dismiss
        // and sweep retry. The partial expression index keeps that a lookup on
        // the request id instead of a walk of every event the thread holds.
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_daemon_wire_events_guest_resolution
             ON daemon_wire_events (json_extract(event_json, '$.request_id'))
             WHERE json_extract(event_json, '$.type') = 'guest_request_resolved'",
        )
        .execute(&self.pool)
        .await?;

        // A person's message is journaled before a provider accepts it. The
        // queue lives in memory across turns but not across a restart, so the
        // ids of messages no provider has accepted are kept durably. A later
        // handoff leaves those items out instead of reading a prompt that was
        // waiting as if the old provider had already seen it.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS daemon_pending_items (
                thread_id TEXT NOT NULL,
                item_id   TEXT NOT NULL,
                PRIMARY KEY (thread_id, item_id)
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS daemon_steer_items (
                thread_id TEXT NOT NULL,
                item_id   TEXT NOT NULL,
                PRIMARY KEY (thread_id, item_id)
            )",
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

        // A PR watch is work that outlives the daemon, so it and its outside
        // grant are stored here. A restart restores the watcher instead of
        // dropping it, and never without the grant it was created under.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS daemon_pr_watches (
                thread_id  TEXT NOT NULL,
                account_id TEXT NOT NULL,
                forge      TEXT NOT NULL,
                base_url   TEXT NOT NULL,
                owner      TEXT NOT NULL,
                repo       TEXT NOT NULL,
                number     INTEGER NOT NULL,
                client_id  TEXT,
                family     TEXT,
                ceiling    TEXT,
                watched_at TEXT NOT NULL,
                PRIMARY KEY (thread_id, forge, base_url, owner, repo, number)
            )",
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
                identifier TEXT,
                title      TEXT,
                linked_at  TEXT NOT NULL,
                PRIMARY KEY (thread_id, scope_kind, scope_key, issue_id)
            )",
        )
        .execute(&self.pool)
        .await?;
        // A database written before the link stored its human identity still
        // opens: the two columns are added empty, and the UI falls back to a
        // generic label for those links.
        self.ensure_column("daemon_issue_links", "identifier", "TEXT")
            .await?;
        self.ensure_column("daemon_issue_links", "title", "TEXT")
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

        // A claim used to spell its kind only in the id that followed, so a
        // session id that looked like a path key was read as one. Store the
        // kind on rows written before that, once, in one write transaction.
        self.migrate_import_claims().await?;

        Ok(())
    }

    /// Gives every claim written before kinds were stored its stored identity.
    ///
    /// The whole table is read before anything is written: each row keeps its
    /// own thread and path, and the new keys are checked to be distinct. Two
    /// rows can never collapse into one claim, and a rename cannot land on a
    /// row that has not been renamed yet.
    async fn migrate_import_claims(&self) -> Result<(), JournalError> {
        const MIGRATION: &str = "import_claim_kinds";
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS daemon_migrations (
                name       TEXT PRIMARY KEY,
                applied_at TEXT NOT NULL
            )",
        )
        .execute(&mut *tx)
        .await?;
        let applied: Option<String> =
            sqlx::query_scalar("SELECT name FROM daemon_migrations WHERE name = ?")
                .bind(MIGRATION)
                .fetch_optional(&mut *tx)
                .await?;
        if applied.is_none() {
            // The thread's stored import metadata names the conversation's own
            // session id, which is the only trustworthy way to tell a legacy
            // session claim whose id looks like a path key from a path claim.
            let rows = sqlx::query(
                "SELECT claim.source_key, claim.thread_id, claim.path, claim.imported_at,
                        thread.imported_json
                   FROM daemon_imported_sources AS claim
                   LEFT JOIN daemon_threads AS thread
                     ON thread.thread_id = claim.thread_id",
            )
            .fetch_all(&mut *tx)
            .await?;
            let mut claims: Vec<(String, String, String, String)> = Vec::with_capacity(rows.len());
            let mut seen: HashSet<String> = HashSet::with_capacity(rows.len());
            for row in rows {
                let key: String = row.try_get("source_key")?;
                let imported_json: Option<String> = row.try_get("imported_json")?;
                let migrated = migrated_claim_key(&key, imported_json.as_deref());
                if !seen.insert(migrated.clone()) {
                    return Err(JournalError::Migration(format!(
                        "two import claims would become {migrated}"
                    )));
                }
                claims.push((
                    migrated,
                    row.try_get("thread_id")?,
                    row.try_get("path")?,
                    row.try_get("imported_at")?,
                ));
            }
            // Every row is written back under its own new key. Deleting first
            // means no rename can replace a row that is still waiting.
            sqlx::query("DELETE FROM daemon_imported_sources")
                .execute(&mut *tx)
                .await?;
            for (key, thread_id, path, imported_at) in claims {
                sqlx::query(
                    "INSERT INTO daemon_imported_sources
                        (source_key, thread_id, path, imported_at)
                     VALUES (?, ?, ?, ?)",
                )
                .bind(key)
                .bind(thread_id)
                .bind(path)
                .bind(imported_at)
                .execute(&mut *tx)
                .await?;
            }
            sqlx::query("INSERT INTO daemon_migrations (name, applied_at) VALUES (?, ?)")
                .bind(MIGRATION)
                .bind(Utc::now().to_rfc3339())
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn ensure_column(
        &self,
        table: &str,
        column: &str,
        definition: &str,
    ) -> Result<(), JournalError> {
        // Two daemons may open the same journal at once. The check and the
        // `ALTER TABLE` run inside one write transaction (`BEGIN IMMEDIATE`),
        // so the second process can only look after the first committed and
        // sees the column it added, instead of both seeing it missing and
        // racing to add it.
        let mut conn = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
        match add_column(&mut conn, table, column, definition).await {
            Ok(()) => {
                sqlx::query("COMMIT").execute(&mut *conn).await?;
                Ok(())
            }
            Err(error) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                Err(error)
            }
        }
    }
}

/// Test-only pause between a column's check and its `ALTER`.
///
/// A test that wants two daemons to contend must keep the first one inside
/// that window, otherwise the two finishes arrive one after the other and the
/// race never happens. Production builds have no gate.
#[cfg(test)]
mod migration_gate {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    static ARMED: AtomicBool = AtomicBool::new(false);
    /// How often the duplicate-column fallback ran. A properly locked upgrade
    /// never needs it, so a test can prove the lock did the work.
    static FALLBACKS: AtomicUsize = AtomicUsize::new(0);

    pub(super) fn arm() {
        ARMED.store(true, Ordering::SeqCst);
    }

    pub(super) fn disarm() {
        ARMED.store(false, Ordering::SeqCst);
    }

    pub(super) async fn pause() {
        if ARMED.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    pub(super) fn record_fallback() {
        FALLBACKS.fetch_add(1, Ordering::SeqCst);
    }

    pub(super) fn reset_fallbacks() {
        FALLBACKS.store(0, Ordering::SeqCst);
    }

    pub(super) fn fallbacks() -> usize {
        FALLBACKS.load(Ordering::SeqCst)
    }
}

/// Adds one column when it is missing, on the caller's locked connection.
async fn add_column(
    conn: &mut sqlx::SqliteConnection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<(), JournalError> {
    let rows = sqlx::query(&format!("PRAGMA table_info({table})"))
        .fetch_all(&mut *conn)
        .await?;
    let exists = rows.iter().any(|row| {
        row.try_get::<String, _>("name")
            .is_ok_and(|name| name == column)
    });
    if exists {
        return Ok(());
    }
    #[cfg(test)]
    migration_gate::pause().await;
    alter_add_column(conn, table, column, definition).await
}

/// Runs the `ALTER`, treating a column another writer already added as done.
async fn alter_add_column(
    conn: &mut sqlx::SqliteConnection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<(), JournalError> {
    match sqlx::query(&format!(
        "ALTER TABLE {table} ADD COLUMN {column} {definition}"
    ))
    .execute(&mut *conn)
    .await
    {
        Ok(_) => Ok(()),
        // A writer that never took the lock (an older build, or another
        // tool) may have added the column after the check. That is exactly
        // the state this migration wants, so it is already applied.
        Err(sqlx::Error::Database(error)) if error.message().contains("duplicate column name") => {
            #[cfg(test)]
            migration_gate::record_fallback();
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

/// The key a legacy import claim should become.
///
/// When the claim's thread still records the conversation it came from, that
/// metadata names the original session id, and a non-empty id makes the claim a
/// session claim even when the id itself is spelled like a path key. Without
/// metadata the old spelling rules decide, which is all an older database can
/// offer.
fn migrated_claim_key(key: &str, imported_json: Option<&str>) -> String {
    if let Some(origin) =
        imported_json.and_then(|json| serde_json::from_str::<ImportedOrigin>(json).ok())
    {
        return match origin
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            Some(id) => h4rn_protocol::source_key(origin.source, Some(id), &origin.source_path),
            // The thread's metadata names no session, so the claim is about a
            // file version and already carries its path, size and fingerprint.
            None => key.to_owned(),
        };
    }
    match h4rn_protocol::migrated_claim_key(key) {
        Some(migrated) => migrated,
        // A path claim already pins its file.
        None => key.to_owned(),
    }
}

/// Marks a journaled user item undelivered on any connection.
async fn mark_item_pending_on(
    conn: &mut sqlx::SqliteConnection,
    thread_id: &ThreadId,
    item_id: &str,
) -> Result<(), JournalError> {
    sqlx::query(
        "INSERT INTO daemon_pending_items (thread_id, item_id) VALUES (?, ?)
         ON CONFLICT(thread_id, item_id) DO NOTHING",
    )
    .bind(thread_id.as_str())
    .bind(item_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Marks one transcript row as a steer written into the running turn.
async fn mark_steer_item_on(
    conn: &mut sqlx::SqliteConnection,
    thread_id: &ThreadId,
    item_id: &str,
) -> Result<(), JournalError> {
    sqlx::query(
        "INSERT INTO daemon_steer_items (thread_id, item_id) VALUES (?, ?)
         ON CONFLICT(thread_id, item_id) DO NOTHING",
    )
    .bind(thread_id.as_str())
    .bind(item_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
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
    let rewind_json = serde_json::to_string(&meta.pending_rewind)?;
    let worktree_identity_json = serde_json::to_string(&meta.worktree_identity)?;
    let provider_turns_json = serde_json::to_string(&meta.provider_turns)?;
    let forked_json = serde_json::to_string(&meta.forked_from)?;
    let advertised_models_json = serde_json::to_string(&meta.advertised_models)?;
    let model_fast_json = serde_json::to_string(&meta.model_fast)?;
    sqlx::query(
        "INSERT INTO daemon_threads
            (thread_id, host, harness, title, title_source, workspace_path, model, created_at, usage_json, resume_id, runtime_mode, reasoning_effort, fast, model_fast_json, advertised_models_json, worktree_json, parent_thread_id, subagent_id, agent_id, agent_fingerprint, parked_json, queued_json, claimed_json, imported_json, rewind_json, worktree_identity_json, provider_turns_json, forked_json, handoff_pending, switch_generation)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
            model_fast_json = excluded.model_fast_json,
            advertised_models_json = excluded.advertised_models_json,
            worktree_json = excluded.worktree_json,
            parent_thread_id = excluded.parent_thread_id,
            subagent_id = excluded.subagent_id,
            agent_id = excluded.agent_id,
            agent_fingerprint = excluded.agent_fingerprint,
            parked_json = excluded.parked_json,
            imported_json = excluded.imported_json,
            rewind_json = excluded.rewind_json,
            worktree_identity_json = excluded.worktree_identity_json,
            provider_turns_json = excluded.provider_turns_json,
            queued_json = excluded.queued_json,
            claimed_json = excluded.claimed_json,
            forked_json = excluded.forked_json,
            handoff_pending = excluded.handoff_pending,
            switch_generation = excluded.switch_generation",
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
    .bind(&model_fast_json)
    .bind(advertised_models_json)
    .bind(worktree_json)
    .bind(meta.parent_thread_id.as_ref().map(ThreadId::as_str))
    .bind(meta.subagent_id.as_ref().map(SubagentId::as_str))
    .bind(meta.agent_id.as_deref())
    .bind(meta.agent_fingerprint.as_deref())
    .bind(parked_json)
    .bind(queued_json)
    .bind(claimed_json)
    .bind(imported_json)
    .bind(rewind_json)
    .bind(worktree_identity_json)
    .bind(provider_turns_json)
    .bind(forked_json)
    .bind(meta.handoff_pending as i64)
    .bind(meta.switch_generation as i64)
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
                steers: Vec::new(),
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
            "SELECT thread_id, host, harness, title, title_source, workspace_path, model, created_at, usage_json, resume_id, runtime_mode, reasoning_effort, fast, model_fast_json, advertised_models_json, worktree_json, parent_thread_id, subagent_id, agent_id, agent_fingerprint, parked_json, queued_json, claimed_json, imported_json, rewind_json, worktree_identity_json, provider_turns_json, forked_json, handoff_pending, switch_generation
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
                model_fast: row
                    .try_get::<Option<String>, _>("model_fast_json")?
                    .and_then(|json| serde_json::from_str(&json).ok())
                    .unwrap_or_default(),
                advertised_models: row
                    .try_get::<Option<String>, _>("advertised_models_json")?
                    .and_then(|json| serde_json::from_str(&json).ok())
                    .unwrap_or_default(),
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
                pending_rewind: row
                    .try_get::<Option<String>, _>("rewind_json")?
                    .and_then(|json| serde_json::from_str(&json).ok()),
                worktree_identity: row
                    .try_get::<Option<String>, _>("worktree_identity_json")?
                    .and_then(|json| serde_json::from_str(&json).ok()),
                provider_turns: row
                    .try_get::<Option<String>, _>("provider_turns_json")?
                    .and_then(|json| serde_json::from_str(&json).ok())
                    .unwrap_or_default(),
                forked_from: row
                    .try_get::<Option<String>, _>("forked_json")?
                    .and_then(|json| serde_json::from_str(&json).ok()),
                handoff_pending: row.try_get::<i64, _>("handoff_pending")? != 0,
                switch_generation: row.try_get::<i64, _>("switch_generation").unwrap_or(0) as u64,
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
        self.append_with_marks(meta, domain, wire, None, &[]).await
    }

    async fn append_sent(
        &self,
        meta: &ThreadMeta,
        domain: &[ThreadEvent],
        wire: &[Event],
        item_id: &str,
    ) -> Result<Vec<u64>, JournalError> {
        self.append_with_marks(meta, domain, wire, Some(item_id), &[])
            .await
    }

    async fn append_steer(
        &self,
        meta: &ThreadMeta,
        domain: &[ThreadEvent],
        wire: &[Event],
        steer_item: &str,
    ) -> Result<Vec<u64>, JournalError> {
        self.append_with_marks(meta, domain, wire, None, &[steer_item])
            .await
    }

    async fn append_fork(
        &self,
        meta: &ThreadMeta,
        domain: &[ThreadEvent],
        wire: &[Event],
        steer_items: &[&str],
    ) -> Result<Vec<u64>, JournalError> {
        self.append_with_marks(meta, domain, wire, None, steer_items)
            .await
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
        let rows = self
            .page_rows(thread_id, after, max_events, max_bytes)
            .await?;

        let mut more = false;
        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            if row.stopped {
                more = true;
                continue;
            }
            let Some(event_json) = row.event_json else {
                continue;
            };
            events.push(WireEvent {
                seq: row.seq,
                thread_id: thread_id.clone(),
                event: serde_json::from_str(&event_json)?,
            });
        }
        // A test's append lands where the old two-statement pager would probe
        // for `more`; the page above is already one snapshot and must not
        // change because of it.
        #[cfg(test)]
        if let Some(between) = page_hook::take(self.instance, thread_id) {
            between
                .journal
                .append_wire(&between.thread_id, &between.events)
                .await?;
        }
        Ok(WirePage { events, more })
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

    async fn guest_resolution_exists(
        &self,
        thread_id: &ThreadId,
        request_id: &str,
    ) -> Result<bool, JournalError> {
        // The partial index on the request id makes this a lookup rather than a
        // walk of the thread's events; the type check keeps an unrelated event
        // that happens to carry the same id string out of the answer.
        let found: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM daemon_wire_events
             WHERE thread_id = ?
               AND json_extract(event_json, '$.type') = 'guest_request_resolved'
               AND json_extract(event_json, '$.request_id') = ?
             LIMIT 1",
        )
        .bind(thread_id.as_str())
        .bind(request_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(found.is_some())
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
        let steer_items: std::collections::HashSet<String> =
            sqlx::query("SELECT item_id FROM daemon_steer_items WHERE thread_id = ?")
                .bind(thread_id.as_str())
                .fetch_all(&self.pool)
                .await?
                .into_iter()
                .filter_map(|row| row.try_get::<String, _>("item_id").ok())
                .collect();

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
        // The turns a rewind removed. The cut leaves the transcript for good,
        // and a late row that still names one of them must not put it back: the
        // text the person edited away may be a secret.
        let mut removed_turns: HashSet<TurnId> = HashSet::new();
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
                    if removed_turns.contains(&turn_id) {
                        continue;
                    }
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
                    if removed_turns.contains(&turn_id) {
                        continue;
                    }
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
                    // the next turn that starts, never to this one. A steer is
                    // the exception: it was written into this running turn on
                    // purpose, and the journal marked its row as one (ADR-047).
                    let steered = matches!(
                        &item,
                        Item::Message { item_id, .. } if steer_items.contains(item_id)
                    );
                    if steered && let Item::Message { item_id, .. } = &item {
                        turns[index].steers.push(item_id.clone());
                    }
                    if is_user_message(&item)
                        && !steered
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
                    if removed_turns.contains(&turn_id) {
                        continue;
                    }
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
                    if removed_turns.contains(&turn_id) {
                        continue;
                    }
                    let index = turn_index(&mut turns, &turn_id);
                    turns[index].push_item(seq, JournaledItem::ToolCall { tool_call });
                }
                Event::Plan {
                    turn_id, entries, ..
                } => {
                    if removed_turns.contains(&turn_id) {
                        continue;
                    }
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
                    if removed_turns.contains(&turn_id) {
                        continue;
                    }
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
                Event::ThreadRewound { before_item_id, .. } => {
                    // The same cut `rewind::live_prompts` makes: the named
                    // prompt and everything after it left the conversation. A
                    // fork must never copy them back, so they leave this reader
                    // too, and any later row that still names one is ignored.
                    let named = |item: &JournaledItem| {
                        matches!(
                            item,
                            JournaledItem::Item {
                                item: Item::Message {
                                    item_id,
                                    role: h4rn_protocol::MessageRole::User,
                                    ..
                                },
                                ..
                            } if item_id.as_str() == before_item_id.as_str()
                        )
                    };
                    let Some(cut) = turns.iter().position(|turn| turn.items.iter().any(named))
                    else {
                        continue;
                    };
                    let cut_seq = turns[cut]
                        .items
                        .iter()
                        .position(named)
                        .and_then(|position| turns[cut].item_seqs.get(position).copied());
                    for turn in turns.drain(cut..) {
                        removed_turns.insert(turn.turn_id);
                    }
                    // A prompt an older journal held without a turn of its own
                    // stays held when it was shown before the cut item, and
                    // leaves with the cut when it was shown after it.
                    held_prompts.retain(|(seq, _)| cut_seq.is_some_and(|cut_seq| *seq < cut_seq));
                    // Streamed text still refers to turn indexes the cut dropped.
                    streaming.retain(|_, (turn, _)| *turn < cut);
                    open_turn = turns
                        .iter()
                        .rposition(|turn| turn.started_at.is_some() && turn.status.is_none());
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
    async fn clear_handoff_pending(
        &self,
        thread_id: &ThreadId,
        switch_generation: u64,
    ) -> Result<bool, JournalError> {
        let result = sqlx::query(
            "UPDATE daemon_threads SET handoff_pending = 0
             WHERE thread_id = ? AND switch_generation = ?",
        )
        .bind(thread_id.as_str())
        .bind(switch_generation as i64)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn fall_back_from_native_resume(
        &self,
        thread_id: &ThreadId,
        switch_generation: u64,
        rewind_fallback: bool,
    ) -> Result<bool, JournalError> {
        // One statement, one field: the resume id this fallback could not use is
        // cleared, the history is owed, and the import's one native resume is
        // spent. A fallback that replaces a refused native rewind also records
        // that the next session starts from a handoff. The generation guard
        // means a fallback that lost its thread to a later switch changes
        // nothing at all.
        let rewind_json = rewind_fallback
            .then(|| serde_json::to_string(&h4rn_protocol::PendingRewind::Handoff))
            .transpose()?;
        let result = sqlx::query(
            "UPDATE daemon_threads
                SET resume_id = NULL,
                    handoff_pending = 1,
                    rewind_json = COALESCE(?, rewind_json),
                    imported_json = CASE
                        WHEN json_valid(imported_json)
                            THEN json_set(imported_json, '$.native_resume', json('false'))
                        ELSE imported_json
                    END,
                    forked_json = CASE
                        WHEN json_valid(forked_json)
                            THEN json_set(forked_json, '$.native_fork', json('false'))
                        ELSE forked_json
                    END
              WHERE thread_id = ? AND switch_generation = ?",
        )
        .bind(rewind_json.as_deref())
        .bind(thread_id.as_str())
        .bind(switch_generation as i64)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn mark_item_accepted(
        &self,
        thread_id: &ThreadId,
        item_id: &str,
    ) -> Result<(), JournalError> {
        sqlx::query("DELETE FROM daemon_pending_items WHERE thread_id = ? AND item_id = ?")
            .bind(thread_id.as_str())
            .bind(item_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn pending_item_ids(&self, thread_id: &ThreadId) -> Result<Vec<String>, JournalError> {
        let rows = sqlx::query(
            "SELECT item_id FROM daemon_pending_items WHERE thread_id = ? ORDER BY rowid ASC",
        )
        .bind(thread_id.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                row.try_get::<String, _>("item_id")
                    .map_err(JournalError::from)
            })
            .collect()
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

    async fn replace_pr_watches(&self, watches: &[StoredPrWatch]) -> Result<(), JournalError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query("DELETE FROM daemon_pr_watches")
            .execute(&mut *transaction)
            .await?;
        for watch in watches {
            let ceiling = match watch.grant.as_ref().and_then(|grant| grant.ceiling) {
                Some(ceiling) => Some(serde_json::to_string(&ceiling).map_err(|error| {
                    JournalError::Database(sqlx::Error::Protocol(error.to_string()))
                })?),
                None => None,
            };
            sqlx::query(
                "INSERT INTO daemon_pr_watches
                    (thread_id, account_id, forge, base_url, owner, repo, number,
                     client_id, family, ceiling, watched_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(watch.thread_id.as_str())
            .bind(&watch.account_id)
            .bind(watch.repo.forge.slug())
            .bind(&watch.repo.base_url)
            .bind(&watch.repo.owner)
            .bind(&watch.repo.repo)
            .bind(i64::from(watch.number))
            .bind(watch.grant.as_ref().map(|grant| grant.client_id.as_str()))
            .bind(watch.grant.as_ref().map(|grant| grant.family.as_str()))
            .bind(ceiling)
            .bind(watch.watched_at.to_rfc3339())
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    async fn pr_watches(&self) -> Result<Vec<StoredPrWatch>, JournalError> {
        let rows = sqlx::query(
            "SELECT thread_id, account_id, forge, base_url, owner, repo, number,
                    client_id, family, ceiling, watched_at
             FROM daemon_pr_watches ORDER BY watched_at ASC, rowid ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(pr_watch_from_row).collect()
    }

    async fn upsert_issue_link(&self, link: &IssueLinkRecord) -> Result<(), JournalError> {
        let (kind, key) = issue_scope_parts(&link.scope);
        let scope_json = serde_json::to_string(&link.scope)
            .map_err(|error| JournalError::Database(sqlx::Error::Protocol(error.to_string())))?;
        sqlx::query(
            "INSERT INTO daemon_issue_links
                (thread_id, account_id, scope_kind, scope_key, scope_json, issue_id,
                 identifier, title, linked_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(thread_id, scope_kind, scope_key, issue_id) DO UPDATE SET
                account_id = excluded.account_id,
                identifier = excluded.identifier,
                title = excluded.title",
        )
        .bind(link.thread_id.as_str())
        .bind(&link.account_id)
        .bind(kind)
        .bind(key)
        .bind(scope_json)
        .bind(&link.issue_id)
        .bind(&link.identifier)
        .bind(&link.title)
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
            "SELECT thread_id, account_id, scope_json, issue_id, identifier, title, linked_at
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

impl SqliteJournal {
    /// The rows the page walk visited: the page plus one lookahead row.
    ///
    /// The recursive walk takes one row at a time over the (thread_id, seq)
    /// index and stops as soon as the page would pass its byte budget or its
    /// row cap, so the database reads one page instead of the whole replay.
    /// The first row is taken unconditionally, which keeps an oversized event
    /// whole.
    ///
    /// The walk also yields the first row that did not fit, marked `stopped`.
    /// Only that flag comes out of the walk: neither its text nor its length is
    /// carried, and the outer select reads a body only for the rows the page
    /// accepted. A rejected event is therefore never fetched, however large it
    /// is. Its presence is the page's `more` flag, and the answer comes from
    /// inside the same statement as the page, inside one implicit read
    /// transaction, so an append cannot land between them and turn an empty
    /// page into `more=true` with an unchanged cursor. The tests check this
    /// through `page_hook` and measure the query's SQLite VM steps and the
    /// largest allocation it makes, so a walk that scanned the thread only to
    /// trim its output fails them.
    ///
    /// The budget counts each row's stored `event_json` bytes, not the event
    /// re-serialized by this build. `octet_length` keeps SQLite from loading a
    /// row's text just to measure it. A page boundary for history written by an
    /// older protocol version can therefore land elsewhere than a
    /// re-serialized count would put it; nothing is lost either way, because
    /// the next page starts at the following seq, and the outbound frame of a
    /// paired session is chunked below the transport's frame limit regardless
    /// of its size.
    async fn page_rows(
        &self,
        thread_id: &ThreadId,
        after: u64,
        max_events: usize,
        max_bytes: usize,
    ) -> Result<Vec<PageRow>, JournalError> {
        let rows = sqlx::query(
            "WITH RECURSIVE page(seq, used, n, stopped) AS (
                 SELECT seq, octet_length(event_json), 1, 0
                   FROM daemon_wire_events
                  WHERE thread_id = ?1
                    AND seq = (SELECT MIN(seq) FROM daemon_wire_events
                                WHERE thread_id = ?1 AND seq > ?2)
                 UNION ALL
                 SELECT next.seq,
                        page.used + octet_length(next.event_json),
                        page.n + 1,
                        CASE WHEN page.n >= ?4
                                  OR page.used + octet_length(next.event_json) > ?3
                             THEN 1 ELSE 0 END
                   FROM page
                   JOIN daemon_wire_events AS next
                     ON next.thread_id = ?1
                    AND next.seq = (SELECT MIN(seq) FROM daemon_wire_events
                                     WHERE thread_id = ?1 AND seq > page.seq)
                  WHERE page.stopped = 0
             )
             SELECT page.seq, events.event_json, page.stopped
               FROM page
               LEFT JOIN daemon_wire_events AS events
                 ON events.thread_id = ?1
                AND events.seq = page.seq
                AND page.stopped = 0
              ORDER BY page.seq",
        )
        .bind(thread_id.as_str())
        .bind(after as i64)
        .bind(max_bytes as i64)
        .bind(max_events as i64)
        .fetch_all(&self.pool)
        .await?;

        let mut page = Vec::with_capacity(rows.len());
        for row in rows {
            page.push(PageRow {
                seq: row.try_get::<i64, _>("seq")? as u64,
                event_json: row.try_get("event_json")?,
                stopped: row.try_get::<i64, _>("stopped")? != 0,
            });
        }
        Ok(page)
    }

    /// One transaction for a thread row, its events and the marks that go with
    /// them: a prompt no provider has taken yet, and the steers that belong to
    /// the running turn.
    async fn append_with_marks(
        &self,
        meta: &ThreadMeta,
        domain: &[ThreadEvent],
        wire: &[Event],
        pending_item: Option<&str>,
        steer_items: &[&str],
    ) -> Result<Vec<u64>, JournalError> {
        // One transaction: the thread row, the domain log and the wire log
        // commit together or not at all, so a crash or a failed insert cannot
        // leave a domain event without its wire event. A sent prompt's
        // undelivered marker belongs to the same commit for the same reason.
        let mut tx = self.pool.begin().await?;
        upsert_thread_on(&mut tx, meta).await?;
        if !domain.is_empty() {
            SqliteEventStore::append_in(&mut tx, &meta.thread_id, domain).await?;
        }
        if let Some(item_id) = pending_item {
            mark_item_pending_on(&mut tx, &meta.thread_id, item_id).await?;
        }
        for item_id in steer_items {
            mark_steer_item_on(&mut tx, &meta.thread_id, item_id).await?;
        }
        let seqs = if wire.is_empty() {
            Vec::new()
        } else {
            append_wire_on(&mut tx, &meta.thread_id, wire).await?
        };
        tx.commit().await?;
        Ok(seqs)
    }
    /// The identity a test hook matches against.
    #[cfg(test)]
    pub(crate) fn test_instance(&self) -> u64 {
        self.instance
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

fn pr_watch_from_row(row: sqlx::sqlite::SqliteRow) -> Result<StoredPrWatch, JournalError> {
    let forge: String = row.try_get("forge")?;
    let number: i64 = row.try_get("number")?;
    let client_id: Option<String> = row.try_get("client_id")?;
    let family: Option<String> = row.try_get("family")?;
    let ceiling: Option<String> = row.try_get("ceiling")?;
    let grant = match (client_id, family) {
        (Some(client_id), Some(family)) => Some(crate::pr_links::WatchGrant {
            client_id,
            family,
            ceiling: ceiling
                .map(|value| {
                    serde_json::from_str(&value).map_err(|error| {
                        JournalError::Database(sqlx::Error::Protocol(format!(
                            "unparsable PR watch ceiling {value:?}: {error}"
                        )))
                    })
                })
                .transpose()?,
        }),
        _ => None,
    };
    let watched_at: String = row.try_get("watched_at")?;
    Ok(StoredPrWatch {
        thread_id: ThreadId::new(row.try_get::<String, _>("thread_id")?),
        account_id: row.try_get("account_id")?,
        repo: ForgeRepoRef {
            forge: ForgeKind::parse(&forge).ok_or_else(|| {
                JournalError::Database(sqlx::Error::Protocol(format!(
                    "unknown forge slug {forge:?} in daemon_pr_watches"
                )))
            })?,
            base_url: row.try_get("base_url")?,
            owner: row.try_get("owner")?,
            repo: row.try_get("repo")?,
        },
        number: u32::try_from(number).map_err(|_| {
            JournalError::Database(sqlx::Error::Protocol(format!(
                "PR number {number} does not fit in daemon_pr_watches"
            )))
        })?,
        grant,
        watched_at: DateTime::parse_from_rfc3339(&watched_at)
            .map(|value| value.with_timezone(&Utc))
            .map_err(|error| {
                JournalError::Database(sqlx::Error::Protocol(format!(
                    "unparsable watched_at {watched_at:?} in daemon_pr_watches: {error}"
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
        identifier: row.try_get("identifier")?,
        title: row.try_get("title")?,
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
        "pending_rewind": meta.pending_rewind,
        "worktree_identity": meta.worktree_identity,
        "provider_turns": meta.provider_turns,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use h4rn_protocol::{AgentState, ThreadSummary};

    /// SQLite virtual-machine instructions run by the measured page query.
    static PAGE_VM_STEPS: AtomicUsize = AtomicUsize::new(0);
    /// Serialises measurements: the progress handler is process-wide here.
    static PAGE_VM_STEPS_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
            advertised_models: Vec::new(),
            model_fast: Default::default(),
            worktree: None,
            created_at: Utc::now(),
            usage: Usage::default(),
            resume_id: None,
            agent_fingerprint: None,
            parked: None,
            pending_rewind: None,
            worktree_identity: None,
            provider_turns: Default::default(),
            queued: Vec::new(),
            claimed: Vec::new(),
            imported: None,
            forked_from: None,
            handoff_pending: false,
            switch_generation: 0,
        }
    }

    fn summary() -> ThreadSummary {
        ThreadSummary {
            thread_id: ThreadId::new("th_one"),
            parent_thread_id: None,
            host: HostId::new("host_test"),
            harness: HarnessKind::ClaudeCode,
            imported_from: None,
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
            fast_models: None,
            fast_available: None,
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
            rewind: Default::default(),
            file_rewind: false,
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
            item_id: Some("item_user_one".into()),
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

    /// The issue-link table as daemons wrote it before the identity columns,
    /// with one row that must survive the upgrade.
    async fn old_issue_links_schema(path: &std::path::Path) {
        let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .expect("open old database");
        sqlx::query(
            "CREATE TABLE daemon_issue_links (
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
        .execute(&pool)
        .await
        .expect("old schema");
        sqlx::query(
            "INSERT INTO daemon_issue_links
                (thread_id, account_id, scope_kind, scope_key, scope_json, issue_id, linked_at)
             VALUES ('th_1', 'lin_1', 'linear', 'team_1',
                     '{\"tracker\":\"linear\",\"team_id\":\"team_1\"}', 'issue_uuid_1',
                     '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .expect("old row");
        pool.close().await;
    }

    #[tokio::test]
    async fn an_issue_link_written_before_the_identity_was_stored_migrates_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("old-links.db");
        old_issue_links_schema(&path).await;

        let journal = SqliteJournal::connect(&path).await.expect("open journal");
        let links = journal
            .issue_links_for_thread(&ThreadId::new("th_1"))
            .await
            .expect("list");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].issue_id, "issue_uuid_1");
        assert_eq!(links[0].identifier, None);
        assert_eq!(links[0].title, None);
    }

    /// The fallback counter is process-wide, so the tests that read it take
    /// turns even though cargo runs tests in parallel.
    fn fallback_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    /// Arms the contention gate and disarms it on drop, so a failing
    /// assertion cannot leave every later test pausing at its migration.
    struct ArmedGate;

    impl ArmedGate {
        fn arm() -> Self {
            migration_gate::arm();
            Self
        }
    }

    impl Drop for ArmedGate {
        fn drop(&mut self) {
            migration_gate::disarm();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_daemons_open_one_old_journal_without_a_race() {
        let _counting = fallback_lock().lock().await;
        // Hold every daemon at its column check long enough for the other one
        // to reach the same point, so the two upgrades really contend.
        let _gate = ArmedGate::arm();
        for attempt in 0..3 {
            migration_gate::reset_fallbacks();
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("old-links-race.db");
            // The core schema is applied once, so the only upgrade left is
            // the daemon's own, which both processes run at startup.
            drop(SqliteEventStore::connect(&path).await.expect("core schema"));
            old_issue_links_schema(&path).await;

            let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
            let mut handles = Vec::new();
            for _ in 0..2 {
                let path = path.clone();
                let barrier = barrier.clone();
                handles.push(tokio::spawn(async move {
                    barrier.wait().await;
                    SqliteJournal::connect(&path).await
                }));
            }
            let mut opened = Vec::new();
            for handle in handles {
                let journal = handle
                    .await
                    .expect("the daemon task ran")
                    .unwrap_or_else(|error| {
                        panic!(
                            "both daemons must open the same journal (attempt {attempt}): {error}"
                        )
                    });
                opened.push(journal);
            }
            let links = opened[0]
                .issue_links_for_thread(&ThreadId::new("th_1"))
                .await
                .expect("list");
            assert_eq!(links.len(), 1, "the row survives the upgrade");
            assert_eq!(links[0].issue_id, "issue_uuid_1");
            assert_eq!(links[0].identifier, None);
            // The lock, not the duplicate-column fallback, is what makes the
            // second opener see the first one's column.
            assert_eq!(
                migration_gate::fallbacks(),
                0,
                "a locked upgrade never reached the duplicate-column fallback (attempt {attempt})"
            );
        }
    }

    #[tokio::test]
    async fn a_column_another_writer_already_added_counts_as_applied() {
        let _counting = fallback_lock().lock().await;
        let journal = SqliteJournal::in_memory().await.unwrap();
        let mut conn = journal.pool.acquire().await.expect("connection");
        sqlx::query("CREATE TABLE probe (id TEXT)")
            .execute(&mut *conn)
            .await
            .expect("probe table");
        migration_gate::reset_fallbacks();
        add_column(&mut conn, "probe", "note", "TEXT")
            .await
            .expect("add the column");
        // A writer that did not check first (an older build) reaches its own
        // `ALTER` after ours: the column is already there.
        alter_add_column(&mut conn, "probe", "note", "TEXT")
            .await
            .expect("a duplicate column is already applied");
        assert_eq!(migration_gate::fallbacks(), 1, "the fallback ran once");
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
                        harness: None,
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
                        harness: None,
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

    /// The fallback from a dead native resume changes only its own fields, and
    /// only on the switch it saw.
    ///
    /// It spends both native origins: an import's saved conversation and a
    /// fork's copy, each of which must never be tried again after the fallback.
    #[tokio::test]
    async fn a_fallback_write_is_generation_checked_and_field_limited() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        let mut one = meta("th_one");
        one.resume_id = Some("saved-session".into());
        one.title = "keep me".into();
        one.model = "keep-the-model".into();
        one.imported = Some(ImportedOrigin {
            source: h4rn_protocol::ImportSource::ClaudeCode,
            source_path: "/tmp/work/saved.jsonl".into(),
            session_id: Some("saved-session".into()),
            project_path: None,
            native_resume: true,
        });
        one.forked_from = Some(ForkOrigin {
            thread_id: ThreadId::new("th_source"),
            turn_id: h4rn_protocol::TurnId::new("turn_source"),
            project_path: None,
            native_fork: true,
            provider_turn_id: None,
        });
        one.switch_generation = 4;
        journal.upsert_thread(&one).await.unwrap();

        assert!(
            !journal
                .fall_back_from_native_resume(&one.thread_id, 3, false)
                .await
                .unwrap(),
            "an older switch's write was accepted"
        );
        let row = journal.list_threads().await.unwrap().remove(0);
        assert_eq!(row.resume_id.as_deref(), Some("saved-session"));
        assert!(
            row.forked_from
                .as_ref()
                .is_some_and(|forked| forked.native_fork),
            "a refused fallback spent the fork's native attempt"
        );

        assert!(
            journal
                .fall_back_from_native_resume(&one.thread_id, 4, false)
                .await
                .unwrap(),
            "the fallback on the current switch was refused"
        );
        let row = journal.list_threads().await.unwrap().remove(0);
        assert!(row.resume_id.is_none(), "the dead resume id survived");
        assert!(row.handoff_pending, "the history is not owed");
        assert!(
            row.imported
                .as_ref()
                .is_some_and(|imported| !imported.native_resume),
            "the import's native resume was not spent"
        );
        assert!(
            row.forked_from
                .as_ref()
                .is_some_and(|forked| !forked.native_fork),
            "the fork's native attempt was not spent"
        );
        // Everything the fallback does not own is left exactly as it was.
        assert_eq!(row.title, "keep me");
        assert_eq!(row.model, "keep-the-model");
        assert_eq!(row.switch_generation, 4);
        assert!(
            row.pending_rewind.is_none(),
            "a plain fallback invented a rewind"
        );
    }

    /// A fallback that replaces a refused native boundary records the handoff,
    /// in the same generation-checked statement.
    #[tokio::test]
    async fn a_rewind_fallback_records_the_handoff_it_replaces_it_with() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        let mut one = meta("th_one");
        one.resume_id = Some("saved-session".into());
        one.pending_rewind = Some(h4rn_protocol::PendingRewind::Native {
            session: h4rn_protocol::SessionRewind::ResumeAtMessage {
                message: "msg_1".into(),
            },
        });
        one.switch_generation = 2;
        journal.upsert_thread(&one).await.unwrap();

        assert!(
            journal
                .fall_back_from_native_resume(&one.thread_id, 2, true)
                .await
                .unwrap(),
            "the fallback on the current switch was refused"
        );
        let row = journal.list_threads().await.unwrap().remove(0);
        assert_eq!(
            row.pending_rewind,
            Some(h4rn_protocol::PendingRewind::Handoff),
            "the refused native boundary was not replaced by the handoff"
        );
        assert!(row.handoff_pending);
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
    async fn a_guest_answer_is_looked_up_by_its_request_id() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        let one = meta("th_one");
        let wire = vec![
            Event::GuestRequestResolved {
                thread_id: one.thread_id.clone(),
                request_id: "greq_one".into(),
                sent: true,
            },
            Event::GuestRequestResolved {
                thread_id: one.thread_id.clone(),
                request_id: "greq_two".into(),
                sent: false,
            },
        ];
        journal.append(&one, &[], &wire).await.unwrap();

        assert!(
            journal
                .guest_resolution_exists(&one.thread_id, "greq_one")
                .await
                .unwrap()
        );
        assert!(
            journal
                .guest_resolution_exists(&one.thread_id, "greq_two")
                .await
                .unwrap()
        );
        assert!(
            !journal
                .guest_resolution_exists(&one.thread_id, "greq_three")
                .await
                .unwrap(),
            "another request's answer was taken for this one"
        );
        let other = meta("th_two");
        assert!(
            !journal
                .guest_resolution_exists(&other.thread_id, "greq_one")
                .await
                .unwrap(),
            "another thread's answer was taken for this one"
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

    /// Reads one page and counts the SQLite VM steps it ran.
    ///
    /// The journal's pool holds a single connection, so a progress handler set
    /// on it is the connection the page query runs on. `num_ops = 1` invokes
    /// the handler for every virtual-machine instruction; counting the calls
    /// counts the work, not the rows the statement happened to return.
    async fn page_with_steps(
        journal: &SqliteJournal,
        thread_id: &ThreadId,
        after: u64,
        max_events: usize,
        max_bytes: usize,
    ) -> (WirePage, usize) {
        let guard = PAGE_VM_STEPS_GUARD.lock().await;
        PAGE_VM_STEPS.store(0, Ordering::SeqCst);
        {
            let mut connection = journal.pool.acquire().await.expect("connection");
            let mut handle = connection.lock_handle().await.expect("handle");
            handle.set_progress_handler(1, || {
                PAGE_VM_STEPS.fetch_add(1, Ordering::Relaxed);
                // `true` continues the statement; `false` would interrupt it.
                true
            });
        }
        let page = journal
            .read_wire_page(thread_id, after, max_events, max_bytes)
            .await
            .expect("page");
        let steps = PAGE_VM_STEPS.load(Ordering::SeqCst);
        {
            let mut connection = journal.pool.acquire().await.expect("connection");
            let mut handle = connection.lock_handle().await.expect("handle");
            handle.remove_progress_handler();
        }
        drop(guard);
        (page, steps)
    }

    /// The largest single allocation SQLite has made since the last reset.
    ///
    /// `SQLITE_STATUS_MALLOC_SIZE` is 5. Reading a row's text needs a buffer as
    /// large as the row; a walk that only measures it with `octet_length` does
    /// not. The counter is global, so the caller holds `PAGE_VM_STEPS_GUARD`
    /// while measuring and no other test is inserting a large event.
    fn sqlite_largest_allocation(reset: bool) -> i64 {
        unsafe extern "C" {
            fn sqlite3_status64(
                op: std::os::raw::c_int,
                current: *mut i64,
                highwater: *mut i64,
                reset: std::os::raw::c_int,
            ) -> std::os::raw::c_int;
        }
        const SQLITE_STATUS_MALLOC_SIZE: std::os::raw::c_int = 5;
        let mut current = 0;
        let mut highwater = 0;
        unsafe {
            sqlite3_status64(
                SQLITE_STATUS_MALLOC_SIZE,
                &mut current,
                &mut highwater,
                i32::from(reset),
            );
        }
        highwater
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

        let (page, page_steps) = page_with_steps(&journal, &thread_id, 0, 4096, 8 * 1024).await;
        assert!(
            page.events.len() < 200,
            "the page is a small prefix, not the whole thread"
        );
        assert!(page.more);

        // The whole thread in one page, as the measure of a full walk. The
        // paged query must cost a small fraction of it: a walk that scanned
        // the thread and trimmed with an outer LIMIT costs nearly as much.
        let (all, all_steps) = page_with_steps(&journal, &thread_id, 0, 4096, 1024 * 1024).await;
        assert_eq!(all.events.len(), 200);
        assert!(
            page_steps.saturating_mul(5) < all_steps,
            "{page_steps} VM steps for a {} event page, {all_steps} for all 200",
            page.events.len()
        );
    }

    #[tokio::test]
    async fn a_page_that_ends_at_the_last_event_has_nothing_more() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        let thread_id = ThreadId::new("th_last");
        journal.upsert_thread(&meta("th_last")).await.unwrap();
        journal
            .append_wire(
                &thread_id,
                &[
                    page_event(&thread_id, 0, 1024),
                    page_event(&thread_id, 1, 1024),
                ],
            )
            .await
            .unwrap();

        // A budget that carries both events: the walk ends with the thread, so
        // no lookahead row and no `more`.
        let page = journal
            .read_wire_page(&thread_id, 0, 4096, 4 * 1024)
            .await
            .unwrap();
        assert_eq!(page.events.len(), 2);
        assert!(!page.more, "nothing follows the last event");

        // The same two events split by a one-event budget: the page always has
        // its first event and `more` is true, never an empty page with more.
        let page = journal
            .read_wire_page(&thread_id, 0, 4096, 1)
            .await
            .unwrap();
        assert_eq!(page.events.len(), 1, "the oversized event is carried whole");
        assert!(page.more, "the second event follows");
    }

    #[tokio::test]
    async fn the_row_cap_stops_the_walk_instead_of_trimming_it() {
        const EVENTS: usize = 5_000;
        let journal = SqliteJournal::in_memory().await.unwrap();
        let thread_id = ThreadId::new("th_cap");
        journal.upsert_thread(&meta("th_cap")).await.unwrap();
        let mut wire = Vec::new();
        for index in 0..EVENTS {
            wire.push(page_event(&thread_id, index, 8));
        }
        journal.append_wire(&thread_id, &wire).await.unwrap();

        let (page, page_steps) = page_with_steps(&journal, &thread_id, 0, 100, 256 * 1024).await;
        assert_eq!(page.events.len(), 100, "the cap fills the page");
        assert!(page.more, "the rest follows");

        let (all, all_steps) =
            page_with_steps(&journal, &thread_id, 0, EVENTS, 16 * 1024 * 1024).await;
        assert_eq!(all.events.len(), EVENTS);
        assert!(
            page_steps.saturating_mul(5) < all_steps,
            "{page_steps} VM steps to page 100 of {EVENTS}, {all_steps} for all {EVENTS}"
        );
    }

    #[tokio::test]
    async fn a_lookahead_row_returns_only_its_flag() {
        const BIG: usize = 32 * 1024 * 1024;
        // The allocation counter is process-wide, and this is the only test
        // that writes an event this large.
        let guard = PAGE_VM_STEPS_GUARD.lock().await;
        let journal = SqliteJournal::in_memory().await.unwrap();
        let thread_id = ThreadId::new("th_lookahead");
        journal.upsert_thread(&meta("th_lookahead")).await.unwrap();
        journal
            .append_wire(
                &thread_id,
                &[
                    page_event(&thread_id, 0, 16),
                    page_event(&thread_id, 1, BIG),
                ],
            )
            .await
            .unwrap();

        sqlite_largest_allocation(true);
        let rows = journal
            .page_rows(&thread_id, 0, 4096, 256 * 1024)
            .await
            .unwrap();
        let largest_allocation = sqlite_largest_allocation(false);
        assert_eq!(rows.len(), 2, "the page and the row that ended it");
        assert!(!rows[0].stopped);
        assert!(rows[1].stopped);
        assert_eq!(
            rows[1].event_json, None,
            "the rejected row is only a flag, not its 32 MiB body"
        );
        let fetched: usize = rows
            .iter()
            .filter_map(|row| row.event_json.as_ref())
            .map(String::len)
            .sum();
        assert!(
            fetched < 1024 * 1024,
            "the page fetched {fetched} bytes: the 32 MiB lookahead body was read into the answer"
        );
        assert!(
            largest_allocation < 4 * 1024 * 1024,
            "the walk made a {largest_allocation}-byte allocation: it read the 32 MiB lookahead body"
        );
        drop(guard);
    }

    #[tokio::test]
    async fn claims_written_before_kinds_were_stored_are_classified_on_upgrade() {
        let dir = tempfile::tempdir().expect("dir");
        let database = dir.path().join("old.db");
        let pool = SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&database)
                .create_if_missing(true),
        )
        .await
        .expect("old database");
        sqlx::query(
            "CREATE TABLE daemon_imported_sources (
                source_key  TEXT PRIMARY KEY,
                thread_id   TEXT NOT NULL,
                path        TEXT NOT NULL,
                imported_at TEXT NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .expect("old claims table");
        let path_key = format!("codex:path:/tmp/a.jsonl:42:{}", "a".repeat(64));
        let claims = [
            ("claude_code:abc", "/tmp/abc.jsonl"),
            ("claude_code:path:/tmp/legacy.jsonl", "/tmp/legacy.jsonl"),
            // An old session id that itself began with `session:`.
            ("claude_code:session:old-id", "/tmp/old.jsonl"),
            (path_key.as_str(), "/tmp/a.jsonl"),
        ];
        for (key, path) in claims {
            sqlx::query(
                "INSERT INTO daemon_imported_sources
                    (source_key, thread_id, path, imported_at)
                 VALUES (?, 'th_old', ?, '2026-11-20T10:00:00Z')",
            )
            .bind(key)
            .bind(path)
            .execute(&pool)
            .await
            .expect("old claim");
        }
        pool.close().await;

        let journal = SqliteJournal::connect(&database).await.expect("upgrade");
        let mut keys: Vec<String> = journal
            .list_imported_sources()
            .await
            .unwrap()
            .into_iter()
            .map(|record| record.key)
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "claude_code:session:abc".to_owned(),
                "claude_code:session:path:/tmp/legacy.jsonl".to_owned(),
                "claude_code:session:session:old-id".to_owned(),
                path_key.clone(),
            ]
        );

        // The upgrade is marked, so opening the database again classifies
        // nothing a second time.
        drop(journal);
        let journal = SqliteJournal::connect(&database).await.expect("reopen");
        let mut again: Vec<String> = journal
            .list_imported_sources()
            .await
            .unwrap()
            .into_iter()
            .map(|record| record.key)
            .collect();
        again.sort();
        assert_eq!(again, keys);
    }

    #[tokio::test]
    async fn an_append_between_the_page_and_its_answer_keeps_the_snapshot() {
        let dir = tempfile::tempdir().expect("dir");

        let database = dir.path().join("events.db");
        let journal = SqliteJournal::connect(&database).await.unwrap();
        let thread_id = ThreadId::new("th_between");
        journal.upsert_thread(&meta("th_between")).await.unwrap();
        journal
            .append_wire(&thread_id, &[page_event(&thread_id, 0, 8)])
            .await
            .unwrap();
        let cursor = journal
            .read_wire_since(&thread_id, 0)
            .await
            .unwrap()
            .last()
            .expect("first event")
            .seq;
        // Another thread on the same journal, so a read of it must leave the
        // hook armed too.
        let other = ThreadId::new("th_between_other");
        journal
            .upsert_thread(&meta("th_between_other"))
            .await
            .unwrap();
        journal
            .append_wire(&other, &[page_event(&other, 0, 8)])
            .await
            .unwrap();

        // A second connection writes just before the read answers, where the
        // old two-statement pager ran its `more` probe.
        let writer = SqliteJournal::connect(&database).await.unwrap();
        page_hook::set(page_hook::Between {
            reader: journal.test_instance(),
            journal: writer.clone(),
            thread_id: thread_id.clone(),
            events: vec![page_event(&thread_id, 1, 8)],
        });

        // A read on another journal leaves the arm in place.
        writer
            .read_wire_page(&thread_id, cursor, 4096, 256 * 1024)
            .await
            .unwrap();
        assert_eq!(
            journal.read_wire_since(&thread_id, 0).await.unwrap().len(),
            1,
            "another journal's read fired the hook"
        );

        // A read of another thread on the intended journal leaves it in place.
        journal
            .read_wire_page(&other, 0, 4096, 256 * 1024)
            .await
            .unwrap();
        assert_eq!(
            journal.read_wire_since(&thread_id, 0).await.unwrap().len(),
            1,
            "another thread's read fired the hook"
        );

        let page = journal
            .read_wire_page(&thread_id, cursor, 4096, 256 * 1024)
            .await
            .unwrap();
        assert!(
            page.events.is_empty(),
            "the page was taken before the append"
        );
        assert!(
            !page.more,
            "an empty page must not promise more at an unchanged cursor"
        );

        // The appended event is still there, in the next page.
        let next = journal
            .read_wire_page(&thread_id, cursor, 4096, 256 * 1024)
            .await
            .unwrap();
        assert_eq!(next.events.len(), 1, "the append is not lost");
        assert!(!next.more);
    }

    /// A page of rows written before `at` existed.
    ///
    /// Their stored text omits `at`, so re-serializing them adds `"at":null`.
    /// That makes the page boundary land elsewhere than the pre-SQL pager's
    /// did, but every event still arrives exactly once and in order.
    #[tokio::test]
    async fn a_legacy_page_may_differ_but_delivers_every_event_once() {
        const BUDGET: usize = 256 * 1024;
        let journal = SqliteJournal::in_memory().await.unwrap();
        let thread_id = ThreadId::new("th_legacy");

        // Two rows whose stored text exactly fills the budget between them.
        let mut seqs = Vec::new();
        for (index, target) in [BUDGET / 2, BUDGET / 2].into_iter().enumerate() {
            let mut value = serde_json::json!({
                "type": "item_completed",
                "thread_id": thread_id.as_str(),
                "turn_id": "turn_page",
                "item": {
                    "kind": "message",
                    "item_id": format!("legacy-{index}"),
                    "role": "user",
                    "text": "",
                    "attachments": [],
                },
            });
            let empty = value.to_string().len();
            value["item"]["text"] = serde_json::Value::String("l".repeat(target - empty));
            let text = value.to_string();
            assert_eq!(text.len(), target, "the row is exactly the target size");
            let seq: i64 = sqlx::query_scalar(
                "INSERT INTO daemon_wire_events (thread_id, event_json, recorded_at)
                 VALUES (?, ?, '2026-11-20T10:00:00Z') RETURNING seq",
            )
            .bind(thread_id.as_str())
            .bind(text)
            .fetch_one(&journal.pool)
            .await
            .unwrap();
            seqs.push(seq as u64);
        }

        let all = journal.read_wire_since(&thread_id, 0).await.unwrap();
        let before = old_pages(&all, BUDGET);
        assert_eq!(before.len(), 2, "the old pager split the two rows");
        assert_eq!(before[0].0, vec![seqs[0]], "one event in the first page");
        assert!(before[0].1, "the old pager had more to send");

        let page = journal
            .read_wire_page(&thread_id, 0, 4096, BUDGET)
            .await
            .unwrap();
        assert_eq!(
            page.events
                .iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            seqs,
            "the stored-byte budget carries both rows in one page"
        );
        assert!(!page.more, "nothing is left after the page");
        let serialized: usize = page
            .events
            .iter()
            .map(|event| serde_json::to_string(&event.event).unwrap().len())
            .sum();
        assert!(
            serialized > BUDGET,
            "the re-serialized page ({serialized}) passes the stored-byte budget"
        );

        // Walking a smaller budget delivers every event exactly once, in
        // order, with an exact `more` each time.
        let mut after = 0;
        let mut walked = Vec::new();
        loop {
            let page = journal
                .read_wire_page(&thread_id, after, 4096, BUDGET / 2)
                .await
                .unwrap();
            assert!(!page.events.is_empty(), "a page with more is never empty");
            walked.extend(page.events.iter().map(|event| event.seq));
            after = *walked.last().unwrap();
            if !page.more {
                break;
            }
        }
        assert_eq!(walked, seqs, "every legacy event arrives once, in order");
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
                        harness: None,
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
                        harness: None,
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
                        harness: None,
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
                        harness: None,
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
            complete: false,
            note: None,
        };
        journal
            .append(
                &one,
                &[],
                &[
                    Event::TurnStarted {
                        harness: None,
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

    /// A claim written before `from_queue` existed reads as a queue promotion.
    ///
    /// The older daemon wrote a claim only for prompts it promoted from the
    /// queue, so a missing field must restore the message instead of dropping
    /// it as an ordinary send's already journaled prompt.
    #[test]
    fn a_claim_from_an_older_daemon_reads_back_as_a_queue_promotion() {
        let legacy = serde_json::json!({
            "message": {
                "message_id": "qm_legacy",
                "text": "written by an older daemon",
                "attachments": [],
                "queued_at": "2026-08-24T11:59:00Z",
                "steerable": false,
                "delivery_unknown": false,
                "ceiling": null,
                "grant": null
            },
            "index": 0,
            "turn_id": "turn_legacy",
            "state": "claimed",
            "promoted": true
        });
        let claim: QueuedClaim = serde_json::from_value(legacy).expect("the legacy claim shape");
        assert!(
            claim.from_queue,
            "a claim without from_queue is not a promotion"
        );
        assert!(claim.promoted, "the legacy claim is still a turn's prompt");
        assert_eq!(claim.state, QueuedClaimState::Claimed);
    }
}
