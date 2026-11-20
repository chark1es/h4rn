//! Thread lifecycle: the pure state machine of ADR-004.
//!
//! Working / Done / Settled are derived from facts, not from where the user
//! last dragged a row, except for a manual override that sticks until the next
//! turn starts. The functions here are pure so the daemon and the client can
//! share exactly one predicate set, and so clock-skew behaviour is testable.

use chrono::{DateTime, Duration, Utc};
use h4rn_protocol::{AgentState, ThreadTab, TurnStatus};
use serde::{Deserialize, Serialize};

/// How long a thread may sit idle before it settles automatically.
pub const DEFAULT_AUTO_SETTLE: Duration = Duration::hours(24);
/// A turn that just ended stays in Working for this long to avoid a flap.
pub const DEFAULT_WORKING_EXIT_GRACE: Duration = Duration::seconds(2);
/// A client whose clock is this far ahead must not be shown a stale grace.
pub const DEFAULT_STALE_BYPASS: Duration = Duration::seconds(60);

/// Tunables shared by the daemon and the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LifecycleSettings {
    /// Idle period after which a thread auto-settles (default 24 h).
    pub auto_settle_after: Duration,
    /// Time a just-finished turn remains in Working (default 2 s).
    pub working_exit_grace: Duration,
    /// Age after which the exit grace is considered stale (default 60 s).
    pub stale_working_bypass: Duration,
}

impl Default for LifecycleSettings {
    fn default() -> Self {
        Self {
            auto_settle_after: DEFAULT_AUTO_SETTLE,
            working_exit_grace: DEFAULT_WORKING_EXIT_GRACE,
            stale_working_bypass: DEFAULT_STALE_BYPASS,
        }
    }
}

impl LifecycleSettings {
    /// Settings with a different auto-settle window (per-project override).
    pub fn with_auto_settle(mut self, after: Duration) -> Self {
        self.auto_settle_after = after;
        self
    }

    /// Settings with a different post-turn grace.
    pub fn with_working_exit_grace(mut self, grace: Duration) -> Self {
        self.working_exit_grace = grace;
        self
    }

    /// Settings with a different stale bypass.
    pub fn with_stale_bypass(mut self, bypass: Duration) -> Self {
        self.stale_working_bypass = bypass;
        self
    }
}

/// The observable phase of a thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// No turn is active; the thread has no outstanding work.
    Idle,
    /// A turn is waiting behind another run.
    Queued,
    /// A turn is streaming or executing tools.
    Running,
    /// The harness raised an approval hand.
    AwaitingApproval,
    /// The last turn completed (successfully or not).
    Completed {
        /// How it ended.
        status: TurnStatus,
    },
    /// The provider reported a usage limit.
    RateLimited,
}

impl Phase {
    /// Whether a turn is running, queued or blocked on the user.
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Queued | Self::Running | Self::AwaitingApproval)
    }
}

/// A manual placement chosen by the user (ADR-004).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManualOverride {
    /// Destination tab.
    pub tab: ThreadTab,
    /// Whether the row is pinned in Working.
    pub pinned: bool,
    /// When the user made the move.
    pub set_at: DateTime<Utc>,
}

/// Usage-limit state, including whether auto-resume is armed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageLimit {
    /// When the provider resets, if known.
    pub reset_at: Option<DateTime<Utc>>,
    /// Whether the daemon will resume automatically.
    pub auto_resume: bool,
}

/// Everything the placement predicate needs to know about one thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadState {
    /// Current phase.
    pub phase: Phase,
    /// Agent state for the orb/static-glyph mapping.
    pub agent_state: AgentState,
    /// A hand-set placement, cleared when the next turn starts.
    pub manual: Option<ManualOverride>,
    /// Whether the user acknowledged the last finished turn.
    pub acknowledged: bool,
    /// Snooze expiry; a snoozed thread is outside all three tabs.
    pub snoozed_until: Option<DateTime<Utc>>,
    /// When the current snooze was requested.
    pub snoozed_at: Option<DateTime<Utc>>,
    /// Last user-authored message or turn activity.
    pub last_activity: DateTime<Utc>,
    /// Last user-authored message, specifically.
    pub last_user_activity: DateTime<Utc>,
    /// Last event observed for this thread.
    pub last_event_at: DateTime<Utc>,
    /// When the last turn completed.
    pub last_turn_completed_at: Option<DateTime<Utc>>,
    /// Last turn outcome.
    pub last_turn_status: Option<TurnStatus>,
    /// An approval is waiting.
    pub pending_approval: bool,
    /// A question is waiting.
    pub pending_question: bool,
    /// A failure is waiting to be acknowledged.
    pub pending_failure: bool,
    /// The host disappeared while work was active.
    pub host_unreachable: bool,
    /// Usage-limit state.
    pub usage_limit: Option<UsageLimit>,
    /// A linked branch merged or PR closed.
    pub link_closed: bool,
    /// A PR/branch is being watched, which holds settlement.
    pub pr_watch: bool,
    /// Number of turns started in this thread.
    pub turn_count: u32,
}

impl ThreadState {
    /// A brand-new thread that has never run a turn.
    pub fn new(at: DateTime<Utc>) -> Self {
        Self {
            phase: Phase::Idle,
            agent_state: AgentState::Idle,
            manual: None,
            acknowledged: false,
            snoozed_until: None,
            snoozed_at: None,
            last_activity: at,
            last_user_activity: at,
            last_event_at: at,
            last_turn_completed_at: None,
            last_turn_status: None,
            pending_approval: false,
            pending_question: false,
            pending_failure: false,
            host_unreachable: false,
            usage_limit: None,
            link_closed: false,
            pr_watch: false,
            turn_count: 0,
        }
    }

    /// Whether a Working pin is set.
    pub fn pinned(&self) -> bool {
        self.manual
            .as_ref()
            .is_some_and(|manual| manual.tab == ThreadTab::Working && manual.pinned)
    }

    /// The best agent state after a temporarily unreachable host returns.
    fn restored_agent_state(&self) -> AgentState {
        match &self.phase {
            Phase::Queued => AgentState::Queued,
            Phase::Running => AgentState::Thinking,
            Phase::AwaitingApproval => AgentState::AwaitingApproval,
            Phase::Completed { status } => match status {
                TurnStatus::Completed => AgentState::Done,
                TurnStatus::Failed
                | TurnStatus::Interrupted
                | TurnStatus::TokenLimit
                | TurnStatus::TurnLimit => AgentState::Error,
            },
            Phase::RateLimited => {
                if self
                    .usage_limit
                    .as_ref()
                    .is_some_and(|limit| limit.auto_resume)
                {
                    AgentState::Scheduled
                } else {
                    AgentState::RateLimited
                }
            }
            Phase::Idle => AgentState::Idle,
        }
    }

    /// Applies one event.
    pub fn apply(mut self, event: &ThreadEvent) -> Self {
        let at = event.at();
        self.last_event_at = at;
        self.last_activity = self.last_activity.max(at);

        match event {
            ThreadEvent::TurnQueued { at } => {
                self.phase = Phase::Queued;
                self.agent_state = AgentState::Queued;
                self.acknowledged = false;
                self.pending_failure = false;
                self.last_activity = *at;
            }
            ThreadEvent::TurnStarted { at } => {
                self.phase = Phase::Running;
                self.agent_state = AgentState::Thinking;
                self.acknowledged = false;
                self.pending_approval = false;
                self.pending_question = false;
                self.pending_failure = false;
                self.host_unreachable = false;
                self.usage_limit = None;
                self.link_closed = false;
                self.manual = None;
                self.turn_count = self.turn_count.saturating_add(1);
                self.last_activity = *at;
            }
            ThreadEvent::AgentStateChanged { state, .. } => {
                self.agent_state = *state;
            }
            ThreadEvent::ApprovalRequested { at } => {
                self.phase = Phase::AwaitingApproval;
                self.agent_state = AgentState::AwaitingApproval;
                self.pending_approval = true;
                self.acknowledged = false;
                self.last_activity = *at;
            }
            ThreadEvent::ApprovalResolved { .. } => {
                self.pending_approval = false;
                if matches!(self.phase, Phase::AwaitingApproval) {
                    self.phase = Phase::Running;
                    self.agent_state = AgentState::RunningTool;
                }
            }
            ThreadEvent::QuestionAsked { at } => {
                self.phase = Phase::AwaitingApproval;
                self.agent_state = AgentState::AwaitingApproval;
                self.pending_question = true;
                self.acknowledged = false;
                self.last_activity = *at;
            }
            ThreadEvent::QuestionAnswered { .. } => {
                self.pending_question = false;
                if matches!(self.phase, Phase::AwaitingApproval) {
                    self.phase = Phase::Running;
                    self.agent_state = AgentState::Thinking;
                }
            }
            ThreadEvent::TurnCompleted { status, at } => {
                self.phase = Phase::Completed { status: *status };
                self.agent_state = match status {
                    TurnStatus::Completed => AgentState::Done,
                    TurnStatus::Failed
                    | TurnStatus::Interrupted
                    | TurnStatus::TokenLimit
                    | TurnStatus::TurnLimit => AgentState::Error,
                };
                self.acknowledged = false;
                self.pending_approval = false;
                self.pending_question = false;
                self.pending_failure = matches!(
                    status,
                    TurnStatus::Failed | TurnStatus::TokenLimit | TurnStatus::TurnLimit
                );
                self.last_turn_completed_at = Some(*at);
                self.last_turn_status = Some(*status);
                self.last_activity = *at;
            }
            ThreadEvent::UsageLimitHit {
                reset_at,
                auto_resume,
                at,
            } => {
                self.phase = Phase::RateLimited;
                self.agent_state = if *auto_resume {
                    AgentState::Scheduled
                } else {
                    AgentState::RateLimited
                };
                self.usage_limit = Some(UsageLimit {
                    reset_at: *reset_at,
                    auto_resume: *auto_resume,
                });
                self.acknowledged = false;
                self.pending_failure = false;
                self.last_activity = *at;
            }
            ThreadEvent::LimitAutoResumeSet { auto_resume, .. } => {
                if let Some(limit) = &mut self.usage_limit {
                    limit.auto_resume = *auto_resume;
                }
                if matches!(self.phase, Phase::RateLimited) {
                    self.agent_state = if *auto_resume {
                        AgentState::Scheduled
                    } else {
                        AgentState::RateLimited
                    };
                }
            }
            ThreadEvent::HostLost { at } => {
                self.host_unreachable = true;
                self.agent_state = AgentState::Unreachable;
                self.last_activity = *at;
            }
            ThreadEvent::HostRestored { .. } => {
                self.host_unreachable = false;
                self.agent_state = self.restored_agent_state();
            }
            ThreadEvent::FailureRaised { at } => {
                self.pending_failure = true;
                self.agent_state = AgentState::Error;
                self.last_activity = *at;
            }
            ThreadEvent::MessageReceived { source, at } => {
                self.last_activity = *at;
                if *source == MessageSource::User {
                    self.last_user_activity = *at;
                    self.acknowledged = false;
                }
                if *source == MessageSource::Guest {}
            }
            ThreadEvent::ManualMove { tab, pinned, at } => {
                self.manual = Some(ManualOverride {
                    tab: *tab,
                    pinned: *pinned && *tab == ThreadTab::Working,
                    set_at: *at,
                });
                match tab {
                    ThreadTab::Settled => self.acknowledged = true,
                    ThreadTab::Done | ThreadTab::Working => self.acknowledged = false,
                }
                if *tab == ThreadTab::Working {
                    self.pending_failure = false;
                }
            }
            ThreadEvent::Acknowledge { .. } => {
                if !self.phase.is_active() && !self.pending_approval {
                    self.acknowledged = true;
                    self.pending_failure = false;
                    self.pending_question = false;
                    if self
                        .manual
                        .as_ref()
                        .is_some_and(|m| m.tab != ThreadTab::Working)
                    {
                        self.manual = None;
                    }
                }
            }
            ThreadEvent::Reopen { .. } => {
                self.acknowledged = false;
                self.manual = None;
                self.link_closed = false;
                self.pending_failure = false;
            }
            ThreadEvent::Snooze { until, at } => {
                self.snoozed_until = *until;
                self.snoozed_at = until.map(|_| *at);
            }
            ThreadEvent::Wake { .. } => {
                self.snoozed_until = None;
                self.snoozed_at = None;
            }
            ThreadEvent::BranchMerged { at } | ThreadEvent::PrClosed { at } => {
                self.link_closed = true;
                self.pr_watch = false;
                self.acknowledged = true;
                self.last_activity = *at;
            }
            ThreadEvent::PrWatchStarted { .. } => {
                self.pr_watch = true;
            }
            ThreadEvent::PrWatchStopped { .. } => {
                self.pr_watch = false;
            }
        }

        // A raised hand always wins over a stale snooze (ADR-004): run
        // completion deliberately does not wake a snoozed thread, but a new
        // approval, question, subagent request, guest message or failure does.
        if event.wakes_snooze() {
            self.snoozed_until = None;
            self.snoozed_at = None;
        }

        self
    }
}

/// Where a thread is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// One of the three tabs.
    Tab(ThreadTab),
    /// Hidden in the Snoozed section; resumes to this tab when it wakes.
    Snoozed {
        /// Wake time.
        until: DateTime<Utc>,
        /// Tab the thread returns to.
        resumes_to: ThreadTab,
    },
}

impl Placement {
    /// The underlying tab, even when snoozed.
    pub fn tab(self) -> ThreadTab {
        match self {
            Self::Tab(tab) => tab,
            Self::Snoozed { resumes_to, .. } => resumes_to,
        }
    }

    /// Whether this placement hides the thread from the three tabs.
    pub fn is_snoozed(self) -> bool {
        matches!(self, Self::Snoozed { .. })
    }
}

/// The pure placement function of ADR-004.
///
/// Priority order: manual override, linked-branch settlement, auto-settle,
/// active work, limit handling, host loss, post-turn grace, then the terminal
/// Done/Settled split.
pub fn placement(
    state: &ThreadState,
    now: DateTime<Utc>,
    settings: &LifecycleSettings,
) -> Placement {
    let tab = base_tab(state, now, settings);
    if let Some(until) = state.snoozed_until {
        if until > now {
            return Placement::Snoozed {
                until,
                resumes_to: tab,
            };
        }
        // An expired snooze is simply not snoozed; no event is required.
    }
    Placement::Tab(tab)
}

/// Returns the tab a thread belongs in, ignoring snooze visibility.
pub fn tab_for(state: &ThreadState, now: DateTime<Utc>, settings: &LifecycleSettings) -> ThreadTab {
    placement(state, now, settings).tab()
}

/// Whether the thread is currently snoozed.
pub fn is_snoozed(state: &ThreadState, now: DateTime<Utc>) -> bool {
    state.snoozed_until.is_some_and(|until| until > now)
}

/// Whether a thread is raising a hand that the user must answer.
pub fn needs_attention(
    state: &ThreadState,
    now: DateTime<Utc>,
    settings: &LifecycleSettings,
) -> bool {
    if state.pending_approval || state.pending_question || state.pending_failure {
        return true;
    }
    if state.host_unreachable {
        return true;
    }
    if let Some(limit) = &state.usage_limit {
        if matches!(state.phase, Phase::RateLimited) && !limit.auto_resume {
            return true;
        }
    }
    tab_for(state, now, settings) == ThreadTab::Done && !state.acknowledged
}

fn base_tab(state: &ThreadState, now: DateTime<Utc>, settings: &LifecycleSettings) -> ThreadTab {
    if let Some(manual) = &state.manual {
        return manual.tab;
    }
    if state.link_closed {
        return ThreadTab::Settled;
    }
    if !holds_settlement(state, now) && is_auto_settle_due(state, now, settings) {
        return ThreadTab::Settled;
    }
    if state.host_unreachable && (state.phase.is_active() || state.last_turn_completed_at.is_none())
    {
        return ThreadTab::Working;
    }

    match &state.phase {
        Phase::Queued | Phase::Running | Phase::AwaitingApproval => ThreadTab::Working,
        Phase::RateLimited => {
            let auto_resume = state
                .usage_limit
                .as_ref()
                .is_some_and(|limit| limit.auto_resume);
            if auto_resume {
                ThreadTab::Working
            } else {
                ThreadTab::Done
            }
        }
        Phase::Completed { .. } => {
            if state.acknowledged {
                ThreadTab::Settled
            } else if in_working_exit_grace(state, now, settings) {
                ThreadTab::Working
            } else {
                ThreadTab::Done
            }
        }
        Phase::Idle => {
            if state.acknowledged {
                ThreadTab::Settled
            } else {
                ThreadTab::Working
            }
        }
    }
}

fn in_working_exit_grace(
    state: &ThreadState,
    now: DateTime<Utc>,
    settings: &LifecycleSettings,
) -> bool {
    let Some(completed_at) = state.last_turn_completed_at else {
        return false;
    };
    // Clamp negative elapsed values: a client whose clock is behind must not
    // flicker the row out of Working.
    let elapsed = (now - completed_at).max(Duration::zero());
    let stale = (now - state.last_event_at) > settings.stale_working_bypass;
    elapsed <= settings.working_exit_grace && !stale
}

fn is_auto_settle_due(
    state: &ThreadState,
    now: DateTime<Utc>,
    settings: &LifecycleSettings,
) -> bool {
    let elapsed = (now - state.last_activity).max(Duration::zero());
    elapsed >= settings.auto_settle_after
}

fn holds_settlement(state: &ThreadState, now: DateTime<Utc>) -> bool {
    state.phase.is_active()
        || state.pending_approval
        || state.pending_question
        || state.pending_failure
        || state.host_unreachable
        || state.pr_watch
        || is_snoozed(state, now)
        || state.pinned()
        || state
            .usage_limit
            .as_ref()
            .is_some_and(|limit| limit.auto_resume && matches!(state.phase, Phase::RateLimited))
}

/// Where a message came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageSource {
    /// The thread's owner.
    User,
    /// A linked guest or another account.
    Guest,
    /// A child subagent reporting progress.
    Subagent,
}

/// An append to a thread's lifecycle log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThreadEvent {
    /// A turn was queued behind another run.
    TurnQueued {
        /// Event time.
        at: DateTime<Utc>,
    },
    /// A turn started; this also clears a manual override.
    TurnStarted {
        /// Event time.
        at: DateTime<Utc>,
    },
    /// The harness reported a finer-grained agent state.
    AgentStateChanged {
        /// New state.
        state: AgentState,
        /// Event time.
        at: DateTime<Utc>,
    },
    /// An approval hand was raised.
    ApprovalRequested {
        /// Event time.
        at: DateTime<Utc>,
    },
    /// An approval hand was answered.
    ApprovalResolved {
        /// Event time.
        at: DateTime<Utc>,
    },
    /// The harness asked a question.
    QuestionAsked {
        /// Event time.
        at: DateTime<Utc>,
    },
    /// A question was answered.
    QuestionAnswered {
        /// Event time.
        at: DateTime<Utc>,
    },
    /// A turn finished.
    TurnCompleted {
        /// Outcome.
        status: TurnStatus,
        /// Event time.
        at: DateTime<Utc>,
    },
    /// The provider reported a usage limit.
    UsageLimitHit {
        /// Reset time, if known.
        reset_at: Option<DateTime<Utc>>,
        /// Whether auto-resume is armed.
        auto_resume: bool,
        /// Event time.
        at: DateTime<Utc>,
    },
    /// Auto-resume was armed or cancelled while rate limited.
    LimitAutoResumeSet {
        /// New setting.
        auto_resume: bool,
        /// Event time.
        at: DateTime<Utc>,
    },
    /// The host became unreachable.
    HostLost {
        /// Event time.
        at: DateTime<Utc>,
    },
    /// The host came back.
    HostRestored {
        /// Event time.
        at: DateTime<Utc>,
    },
    /// A failure raised a hand without completing the turn.
    FailureRaised {
        /// Event time.
        at: DateTime<Utc>,
    },
    /// A message arrived from a user, guest or subagent.
    MessageReceived {
        /// Who sent it.
        source: MessageSource,
        /// Event time.
        at: DateTime<Utc>,
    },
    /// The user moved the thread by hand.
    ManualMove {
        /// Destination tab.
        tab: ThreadTab,
        /// Whether to pin an idle thread in Working.
        pinned: bool,
        /// Event time.
        at: DateTime<Utc>,
    },
    /// The user acknowledged a finished turn.
    Acknowledge {
        /// Event time.
        at: DateTime<Utc>,
    },
    /// The user reopened a settled thread.
    Reopen {
        /// Event time.
        at: DateTime<Utc>,
    },
    /// The user snoozed the thread.
    Snooze {
        /// Wake time; `None` clears the snooze.
        until: Option<DateTime<Utc>>,
        /// Event time.
        at: DateTime<Utc>,
    },
    /// The snooze was cleared explicitly.
    Wake {
        /// Event time.
        at: DateTime<Utc>,
    },
    /// A linked branch merged.
    BranchMerged {
        /// Event time.
        at: DateTime<Utc>,
    },
    /// A linked PR closed without merging.
    PrClosed {
        /// Event time.
        at: DateTime<Utc>,
    },
    /// Watching a PR or branch started, holding settlement.
    PrWatchStarted {
        /// Event time.
        at: DateTime<Utc>,
    },
    /// Watching a PR or branch stopped.
    PrWatchStopped {
        /// Event time.
        at: DateTime<Utc>,
    },
}

impl ThreadEvent {
    /// The event's timestamp.
    pub fn at(&self) -> DateTime<Utc> {
        match self {
            Self::TurnQueued { at }
            | Self::TurnStarted { at }
            | Self::AgentStateChanged { at, .. }
            | Self::ApprovalRequested { at }
            | Self::ApprovalResolved { at }
            | Self::QuestionAsked { at }
            | Self::QuestionAnswered { at }
            | Self::TurnCompleted { at, .. }
            | Self::UsageLimitHit { at, .. }
            | Self::LimitAutoResumeSet { at, .. }
            | Self::HostLost { at }
            | Self::HostRestored { at }
            | Self::FailureRaised { at }
            | Self::MessageReceived { at, .. }
            | Self::ManualMove { at, .. }
            | Self::Acknowledge { at }
            | Self::Reopen { at }
            | Self::Snooze { at, .. }
            | Self::Wake { at }
            | Self::BranchMerged { at }
            | Self::PrClosed { at }
            | Self::PrWatchStarted { at }
            | Self::PrWatchStopped { at } => *at,
        }
    }

    /// Whether this event is a raised hand that wakes a snoozed thread.
    pub fn wakes_snooze(&self) -> bool {
        matches!(
            self,
            Self::ApprovalRequested { .. }
                | Self::QuestionAsked { .. }
                | Self::FailureRaised { .. }
                | Self::HostLost { .. }
                | Self::MessageReceived {
                    source: MessageSource::Guest | MessageSource::Subagent,
                    ..
                }
        )
    }
}

/// Replays a log from an initial state.
pub fn apply_all(state: ThreadState, events: &[ThreadEvent]) -> ThreadState {
    events.iter().fold(state, |state, event| state.apply(event))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + seconds, 0).unwrap()
    }

    fn settings() -> LifecycleSettings {
        LifecycleSettings::default()
    }

    fn state_after(events: &[ThreadEvent]) -> ThreadState {
        apply_all(ThreadState::new(at(0)), events)
    }

    fn tab(state: &ThreadState, now: DateTime<Utc>) -> ThreadTab {
        tab_for(state, now, &settings())
    }

    #[test]
    fn new_thread_starts_in_working_then_settles_after_24h() {
        let state = ThreadState::new(at(0));
        assert_eq!(tab(&state, at(0)), ThreadTab::Working);
        assert_eq!(tab(&state, at(24 * 3600 - 1)), ThreadTab::Working);
        assert_eq!(tab(&state, at(24 * 3600)), ThreadTab::Settled);
    }

    #[test]
    fn queued_and_running_are_working() {
        let queued = state_after(&[ThreadEvent::TurnQueued { at: at(1) }]);
        assert_eq!(queued.phase, Phase::Queued);
        assert_eq!(queued.agent_state, AgentState::Queued);
        assert_eq!(tab(&queued, at(1)), ThreadTab::Working);

        let running = queued.apply(&ThreadEvent::TurnStarted { at: at(2) });
        assert_eq!(running.phase, Phase::Running);
        assert_eq!(running.agent_state, AgentState::Thinking);
        assert_eq!(tab(&running, at(2)), ThreadTab::Working);
        assert_eq!(running.turn_count, 1);
    }

    #[test]
    fn completed_turn_stays_working_for_the_grace_then_becomes_done() {
        let state = state_after(&[
            ThreadEvent::TurnQueued { at: at(1) },
            ThreadEvent::TurnStarted { at: at(2) },
            ThreadEvent::TurnCompleted {
                status: TurnStatus::Completed,
                at: at(10),
            },
        ]);
        assert_eq!(state.agent_state, AgentState::Done);
        assert_eq!(tab(&state, at(11)), ThreadTab::Working);
        assert_eq!(tab(&state, at(12)), ThreadTab::Working);
        assert_eq!(
            tab(&state, at(12) + Duration::milliseconds(1)),
            ThreadTab::Done
        );
        assert!(needs_attention(&state, at(20), &settings()));
    }

    #[test]
    fn stale_bypass_disables_a_long_grace() {
        let tuned = LifecycleSettings::default()
            .with_working_exit_grace(Duration::seconds(30))
            .with_stale_bypass(Duration::seconds(5));
        let state = state_after(&[
            ThreadEvent::TurnQueued { at: at(1) },
            ThreadEvent::TurnStarted { at: at(2) },
            ThreadEvent::TurnCompleted {
                status: TurnStatus::Completed,
                at: at(10),
            },
        ]);
        // 6 s after the completion, but 7 s after the last event: stale.
        assert_eq!(
            tab_for(&state, at(16), &tuned),
            ThreadTab::Done,
            "the stale bypass must beat the configured grace"
        );
        // Within the stale window, the grace still applies.
        assert_eq!(tab_for(&state, at(13), &tuned), ThreadTab::Working);
    }

    #[test]
    fn clock_skew_does_not_flicker_out_of_working() {
        let state = state_after(&[
            ThreadEvent::TurnQueued { at: at(10) },
            ThreadEvent::TurnStarted { at: at(11) },
            ThreadEvent::TurnCompleted {
                status: TurnStatus::Completed,
                at: at(10),
            },
        ]);
        // The client clock is behind the completion timestamp.
        assert_eq!(tab(&state, at(8)), ThreadTab::Working);
    }

    #[test]
    fn acknowledge_moves_done_to_settled() {
        let state = state_after(&[
            ThreadEvent::TurnQueued { at: at(1) },
            ThreadEvent::TurnStarted { at: at(2) },
            ThreadEvent::TurnCompleted {
                status: TurnStatus::Completed,
                at: at(3),
            },
        ]);
        assert_eq!(tab(&state, at(10)), ThreadTab::Done);
        let acknowledged = state.apply(&ThreadEvent::Acknowledge { at: at(11) });
        assert!(acknowledged.acknowledged);
        assert_eq!(tab(&acknowledged, at(11)), ThreadTab::Settled);
    }

    #[test]
    fn acknowledge_is_ignored_while_a_turn_is_active() {
        let state = state_after(&[
            ThreadEvent::TurnQueued { at: at(1) },
            ThreadEvent::TurnStarted { at: at(2) },
        ]);
        let after = state.apply(&ThreadEvent::Acknowledge { at: at(3) });
        assert!(!after.acknowledged);
        assert_eq!(tab(&after, at(3)), ThreadTab::Working);
    }

    #[test]
    fn manual_moves_win_until_the_next_turn_starts() {
        for destination in [ThreadTab::Working, ThreadTab::Done, ThreadTab::Settled] {
            let state = ThreadState::new(at(0)).apply(&ThreadEvent::ManualMove {
                tab: destination,
                pinned: destination == ThreadTab::Working,
                at: at(1),
            });
            assert_eq!(tab(&state, at(2)), destination, "{destination:?}");

            let started = state.apply(&ThreadEvent::TurnStarted { at: at(3) });
            assert!(started.manual.is_none(), "a new turn clears the override");
            assert_eq!(tab(&started, at(3)), ThreadTab::Working);
        }
    }

    #[test]
    fn a_working_pin_holds_an_idle_thread_and_settlement() {
        let state = ThreadState::new(at(0)).apply(&ThreadEvent::ManualMove {
            tab: ThreadTab::Working,
            pinned: true,
            at: at(1),
        });
        assert!(state.pinned());
        assert_eq!(tab(&state, at(30 * 3600)), ThreadTab::Working);
    }

    #[test]
    fn manual_done_clears_acknowledgement_and_settled_sets_it() {
        let done = ThreadState::new(at(0)).apply(&ThreadEvent::ManualMove {
            tab: ThreadTab::Done,
            pinned: false,
            at: at(1),
        });
        assert!(!done.acknowledged);

        let settled = done.apply(&ThreadEvent::ManualMove {
            tab: ThreadTab::Settled,
            pinned: false,
            at: at(2),
        });
        assert!(settled.acknowledged);
    }

    #[test]
    fn manual_settle_beats_a_running_turn() {
        let state = state_after(&[
            ThreadEvent::TurnQueued { at: at(1) },
            ThreadEvent::TurnStarted { at: at(2) },
        ])
        .apply(&ThreadEvent::ManualMove {
            tab: ThreadTab::Settled,
            pinned: false,
            at: at(3),
        });
        assert_eq!(tab(&state, at(3)), ThreadTab::Settled);
    }

    #[test]
    fn snooze_hides_a_thread_and_expiry_returns_it() {
        let state = ThreadState::new(at(0)).apply(&ThreadEvent::Snooze {
            until: Some(at(100)),
            at: at(1),
        });
        let snoozed_placement = placement(&state, at(50), &settings());
        assert!(snoozed_placement.is_snoozed());
        assert_eq!(snoozed_placement.tab(), ThreadTab::Working);
        assert!(is_snoozed(&state, at(50)));
        assert!(!is_snoozed(&state, at(101)));
        assert_eq!(
            placement(&state, at(101), &settings()),
            Placement::Tab(ThreadTab::Working)
        );
    }

    #[test]
    fn run_completion_does_not_wake_a_snoozed_thread() {
        let state = state_after(&[
            ThreadEvent::TurnQueued { at: at(1) },
            ThreadEvent::TurnStarted { at: at(2) },
            ThreadEvent::Snooze {
                until: Some(at(100)),
                at: at(3),
            },
            ThreadEvent::TurnCompleted {
                status: TurnStatus::Completed,
                at: at(4),
            },
            ThreadEvent::AgentStateChanged {
                state: AgentState::Editing,
                at: at(5),
            },
        ]);
        assert!(is_snoozed(&state, at(6)));
        assert!(placement(&state, at(6), &settings()).is_snoozed());
    }

    #[test]
    fn raised_hands_wake_a_snoozed_thread() {
        for event in [
            ThreadEvent::ApprovalRequested { at: at(10) },
            ThreadEvent::QuestionAsked { at: at(10) },
            ThreadEvent::FailureRaised { at: at(10) },
            ThreadEvent::MessageReceived {
                source: MessageSource::Guest,
                at: at(10),
            },
            ThreadEvent::MessageReceived {
                source: MessageSource::Subagent,
                at: at(10),
            },
        ] {
            let snoozed = ThreadState::new(at(0)).apply(&ThreadEvent::Snooze {
                until: Some(at(100)),
                at: at(1),
            });
            let woken = snoozed.apply(&event);
            assert!(!is_snoozed(&woken, at(10)), "{event:?} should wake snooze");
        }
    }

    #[test]
    fn a_guest_message_while_snoozed_wakes_but_a_user_message_does_not() {
        let snoozed = ThreadState::new(at(0)).apply(&ThreadEvent::Snooze {
            until: Some(at(100)),
            at: at(1),
        });
        let user = snoozed.clone().apply(&ThreadEvent::MessageReceived {
            source: MessageSource::User,
            at: at(2),
        });
        assert!(is_snoozed(&user, at(2)));

        let guest = snoozed.apply(&ThreadEvent::MessageReceived {
            source: MessageSource::Guest,
            at: at(2),
        });
        assert!(!is_snoozed(&guest, at(2)));
    }

    #[test]
    fn wake_and_snooze_none_clear_the_snooze() {
        let snoozed = ThreadState::new(at(0)).apply(&ThreadEvent::Snooze {
            until: Some(at(100)),
            at: at(1),
        });
        assert!(!is_snoozed(
            &snoozed.clone().apply(&ThreadEvent::Wake { at: at(2) }),
            at(2)
        ));
        assert!(!is_snoozed(
            &snoozed.clone().apply(&ThreadEvent::Snooze {
                until: None,
                at: at(2)
            }),
            at(2)
        ));
    }

    #[test]
    fn auto_settle_is_held_by_approvals_questions_failures_and_watches() {
        let base = state_after(&[
            ThreadEvent::TurnQueued { at: at(1) },
            ThreadEvent::TurnStarted { at: at(2) },
            ThreadEvent::TurnCompleted {
                status: TurnStatus::Completed,
                at: at(3),
            },
        ]);
        let long_after = at(3 + 24 * 3600 + 1);
        assert_eq!(tab(&base, long_after), ThreadTab::Settled);

        for event in [
            ThreadEvent::ApprovalRequested { at: at(4) },
            ThreadEvent::QuestionAsked { at: at(4) },
        ] {
            let held = base.clone().apply(&event);
            assert_eq!(
                tab(&held, long_after),
                ThreadTab::Working,
                "{event:?} must hold settlement in Working"
            );
        }

        // A failure or a PR watch after completion holds settlement, but the
        // terminal turn is already Done (ADR-004) rather than bouncing back to
        // Working.
        for event in [
            ThreadEvent::FailureRaised { at: at(4) },
            ThreadEvent::PrWatchStarted { at: at(4) },
        ] {
            let held = base.clone().apply(&event);
            assert_eq!(
                tab(&held, long_after),
                ThreadTab::Done,
                "{event:?} must hold settlement without auto-settling"
            );
        }
    }

    #[test]
    fn auto_settle_is_held_by_a_queued_turn_and_a_scheduled_resume() {
        let queued = ThreadState::new(at(0)).apply(&ThreadEvent::TurnQueued { at: at(1) });
        assert_eq!(tab(&queued, at(24 * 3600 + 2)), ThreadTab::Working);

        let limited = ThreadState::new(at(0)).apply(&ThreadEvent::UsageLimitHit {
            reset_at: Some(at(100)),
            auto_resume: true,
            at: at(1),
        });
        assert_eq!(tab(&limited, at(24 * 3600 + 2)), ThreadTab::Working);
    }

    #[test]
    fn auto_settle_is_held_by_a_snooze() {
        let state = ThreadState::new(at(0)).apply(&ThreadEvent::Snooze {
            until: Some(at(48 * 3600)),
            at: at(1),
        });
        assert!(placement(&state, at(24 * 3600 + 2), &settings()).is_snoozed());
    }

    #[test]
    fn rate_limit_maps_to_working_or_done_by_auto_resume() {
        let armed = ThreadState::new(at(0)).apply(&ThreadEvent::UsageLimitHit {
            reset_at: Some(at(100)),
            auto_resume: true,
            at: at(1),
        });
        assert_eq!(armed.agent_state, AgentState::Scheduled);
        assert_eq!(tab(&armed, at(2)), ThreadTab::Working);

        let manual = ThreadState::new(at(0)).apply(&ThreadEvent::UsageLimitHit {
            reset_at: Some(at(100)),
            auto_resume: false,
            at: at(1),
        });
        assert_eq!(tab(&manual, at(2)), ThreadTab::Done);
        assert!(needs_attention(&manual, at(2), &settings()));

        let cancelled = armed.apply(&ThreadEvent::LimitAutoResumeSet {
            auto_resume: false,
            at: at(3),
        });
        assert_eq!(tab(&cancelled, at(3)), ThreadTab::Done);
    }

    #[test]
    fn host_loss_keeps_active_work_in_working_and_attention() {
        let lost = state_after(&[
            ThreadEvent::TurnQueued { at: at(1) },
            ThreadEvent::TurnStarted { at: at(2) },
            ThreadEvent::HostLost { at: at(3) },
        ]);
        assert!(lost.host_unreachable);
        assert_eq!(lost.agent_state, AgentState::Unreachable);
        assert_eq!(tab(&lost, at(4)), ThreadTab::Working);
        assert!(needs_attention(&lost, at(4), &settings()));

        let restored = lost.apply(&ThreadEvent::HostRestored { at: at(5) });
        assert!(!restored.host_unreachable);
        assert_eq!(restored.agent_state, AgentState::Thinking);
        assert_eq!(tab(&restored, at(5)), ThreadTab::Working);
    }

    #[test]
    fn cancelling_auto_resume_returns_to_rate_limited() {
        let armed = ThreadState::new(at(0)).apply(&ThreadEvent::UsageLimitHit {
            reset_at: Some(at(100)),
            auto_resume: true,
            at: at(1),
        });
        let cancelled = armed.apply(&ThreadEvent::LimitAutoResumeSet {
            auto_resume: false,
            at: at(2),
        });
        assert_eq!(cancelled.agent_state, AgentState::RateLimited);
    }

    #[test]
    fn host_restore_restores_the_phase_specific_agent_state() {
        let idle = ThreadState::new(at(0))
            .apply(&ThreadEvent::HostLost { at: at(1) })
            .apply(&ThreadEvent::HostRestored { at: at(2) });
        assert_eq!(idle.agent_state, AgentState::Idle);

        let done = state_after(&[
            ThreadEvent::TurnStarted { at: at(1) },
            ThreadEvent::TurnCompleted {
                status: TurnStatus::Completed,
                at: at(2),
            },
            ThreadEvent::HostLost { at: at(3) },
            ThreadEvent::HostRestored { at: at(4) },
        ]);
        assert_eq!(done.agent_state, AgentState::Done);
    }

    #[test]
    fn failed_turn_is_done_until_acknowledged() {
        let state = state_after(&[
            ThreadEvent::TurnQueued { at: at(1) },
            ThreadEvent::TurnStarted { at: at(2) },
            ThreadEvent::TurnCompleted {
                status: TurnStatus::Failed,
                at: at(3),
            },
        ]);
        assert_eq!(state.agent_state, AgentState::Error);
        assert_eq!(tab(&state, at(10)), ThreadTab::Done);
        assert!(needs_attention(&state, at(10), &settings()));

        let acked = state.apply(&ThreadEvent::Acknowledge { at: at(11) });
        assert_eq!(tab(&acked, at(11)), ThreadTab::Settled);
        assert!(!acked.pending_failure);
    }

    #[test]
    fn interrupted_turn_is_done() {
        let state = state_after(&[
            ThreadEvent::TurnStarted { at: at(1) },
            ThreadEvent::TurnCompleted {
                status: TurnStatus::Interrupted,
                at: at(2),
            },
        ]);
        assert_eq!(tab(&state, at(10)), ThreadTab::Done);
    }

    #[test]
    fn linked_branch_merge_settles_and_a_new_turn_reopens() {
        let done = state_after(&[
            ThreadEvent::TurnQueued { at: at(1) },
            ThreadEvent::TurnStarted { at: at(2) },
            ThreadEvent::TurnCompleted {
                status: TurnStatus::Completed,
                at: at(3),
            },
        ]);
        let merged = done.clone().apply(&ThreadEvent::BranchMerged { at: at(4) });
        assert_eq!(tab(&merged, at(10)), ThreadTab::Settled);

        let reopened = merged.apply(&ThreadEvent::TurnStarted { at: at(11) });
        assert!(!reopened.link_closed);
        assert_eq!(tab(&reopened, at(11)), ThreadTab::Working);

        let closed = done.apply(&ThreadEvent::PrClosed { at: at(4) });
        assert_eq!(tab(&closed, at(10)), ThreadTab::Settled);
    }

    #[test]
    fn reopen_moves_settled_back_to_done() {
        let settled = state_after(&[
            ThreadEvent::TurnStarted { at: at(1) },
            ThreadEvent::TurnCompleted {
                status: TurnStatus::Completed,
                at: at(2),
            },
            ThreadEvent::Acknowledge { at: at(3) },
        ]);
        assert_eq!(tab(&settled, at(3)), ThreadTab::Settled);

        let reopened = settled.apply(&ThreadEvent::Reopen { at: at(4) });
        assert_eq!(tab(&reopened, at(10)), ThreadTab::Done);
    }

    #[test]
    fn approval_resolution_returns_to_running() {
        let state = state_after(&[
            ThreadEvent::TurnStarted { at: at(1) },
            ThreadEvent::ApprovalRequested { at: at(2) },
        ]);
        assert_eq!(state.phase, Phase::AwaitingApproval);
        assert_eq!(state.agent_state, AgentState::AwaitingApproval);
        assert_eq!(tab(&state, at(2)), ThreadTab::Working);
        assert!(needs_attention(&state, at(2), &settings()));

        let resolved = state.apply(&ThreadEvent::ApprovalResolved { at: at(3) });
        assert_eq!(resolved.phase, Phase::Running);
        assert!(!resolved.pending_approval);
        assert_eq!(tab(&resolved, at(3)), ThreadTab::Working);
    }

    #[test]
    fn question_resolution_keeps_running() {
        let state = state_after(&[
            ThreadEvent::TurnStarted { at: at(1) },
            ThreadEvent::QuestionAsked { at: at(2) },
        ]);
        assert!(state.pending_question);
        let resolved = state.apply(&ThreadEvent::QuestionAnswered { at: at(3) });
        assert!(!resolved.pending_question);
        assert_eq!(resolved.phase, Phase::Running);
    }

    #[test]
    fn agent_state_events_do_not_change_placement() {
        let state = state_after(&[
            ThreadEvent::TurnStarted { at: at(1) },
            ThreadEvent::AgentStateChanged {
                state: AgentState::Editing,
                at: at(2),
            },
        ]);
        assert_eq!(state.agent_state, AgentState::Editing);
        assert_eq!(tab(&state, at(2)), ThreadTab::Working);
    }

    #[test]
    fn replay_is_deterministic() {
        let events = vec![
            ThreadEvent::TurnQueued { at: at(1) },
            ThreadEvent::TurnStarted { at: at(2) },
            ThreadEvent::ApprovalRequested { at: at(3) },
            ThreadEvent::ApprovalResolved { at: at(4) },
            ThreadEvent::TurnCompleted {
                status: TurnStatus::Completed,
                at: at(5),
            },
        ];
        let once = apply_all(ThreadState::new(at(0)), &events);
        let twice = apply_all(ThreadState::new(at(0)), &events);
        assert_eq!(once, twice);
        assert_eq!(once.turn_count, 1);
    }

    #[test]
    fn every_event_keeps_the_thread_in_exactly_one_placement() {
        let events = vec![
            ThreadEvent::TurnQueued { at: at(1) },
            ThreadEvent::TurnStarted { at: at(2) },
            ThreadEvent::ApprovalRequested { at: at(3) },
            ThreadEvent::ApprovalResolved { at: at(4) },
            ThreadEvent::QuestionAsked { at: at(5) },
            ThreadEvent::QuestionAnswered { at: at(6) },
            ThreadEvent::UsageLimitHit {
                reset_at: Some(at(50)),
                auto_resume: true,
                at: at(7),
            },
            ThreadEvent::LimitAutoResumeSet {
                auto_resume: false,
                at: at(8),
            },
            ThreadEvent::HostLost { at: at(9) },
            ThreadEvent::HostRestored { at: at(10) },
            ThreadEvent::FailureRaised { at: at(11) },
            ThreadEvent::TurnCompleted {
                status: TurnStatus::Failed,
                at: at(12),
            },
            ThreadEvent::Acknowledge { at: at(13) },
            ThreadEvent::Reopen { at: at(14) },
            ThreadEvent::Snooze {
                until: Some(at(100)),
                at: at(15),
            },
            ThreadEvent::Wake { at: at(16) },
            ThreadEvent::BranchMerged { at: at(17) },
            ThreadEvent::PrWatchStarted { at: at(18) },
            ThreadEvent::PrWatchStopped { at: at(19) },
            ThreadEvent::PrClosed { at: at(20) },
            ThreadEvent::ManualMove {
                tab: ThreadTab::Working,
                pinned: false,
                at: at(21),
            },
            ThreadEvent::MessageReceived {
                source: MessageSource::Guest,
                at: at(22),
            },
            ThreadEvent::AgentStateChanged {
                state: AgentState::Searching,
                at: at(23),
            },
        ];

        let mut state = ThreadState::new(at(0));
        for event in events {
            state = state.apply(&event);
            let placement = placement(&state, event.at(), &settings());
            // A placement is always a tab, possibly wrapped in Snoozed.
            let _: ThreadTab = placement.tab();
        }
    }
}
