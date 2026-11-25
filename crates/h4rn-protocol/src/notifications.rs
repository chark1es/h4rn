//! Phone alerts for agent activity (ADR-048).
//!
//! The daemon publishes a small, closed set of state transitions over its
//! existing relay connection. Connect turns a new transition into an FCM data
//! message; nothing else about a thread leaves the machine. The payload is
//! deliberately fixed: a thread id, its title, the project name, one of four
//! states and the time. Message text, tool output, file paths and credentials
//! have no field to travel in.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::ids::ThreadId;

/// The transitions that are allowed to wake a phone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum AgentAlertState {
    /// The turn finished normally and the thread landed in Done.
    Completed,
    /// The turn ended with an error and the thread landed in Done.
    Failed,
    /// The harness is waiting for an approval.
    Approval,
    /// The harness asked a question and waits for an answer.
    Input,
    /// The turn stopped at a usage, token or turn limit and will not resume by
    /// itself.
    Limit,
}

impl AgentAlertState {
    /// Stable spelling used in push data and the delivered baseline.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Approval => "approval",
            Self::Input => "input",
            Self::Limit => "limit",
        }
    }

    /// Whether this is a finishing state (subject to the two-minute cutoff).
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }

    /// Whether this state is the agent asking for a person.
    pub fn needs_attention(self) -> bool {
        matches!(self, Self::Approval | Self::Input | Self::Limit)
    }
}

/// One alertable thread transition, as published by the daemon.
///
/// The daemon sends this as a [`crate::RelayFrameKind::Activity`] payload.
/// Connect reads it to decide whether to push; the relay's normal routing is
/// untouched.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct AgentActivity {
    /// Thread the alert opens.
    pub thread_id: ThreadId,
    /// Human title at the moment of the transition.
    #[serde(default)]
    pub thread_title: String,
    /// Project the thread belongs to, as shown in the sidebar.
    #[serde(default)]
    pub project_name: String,
    /// What happened.
    pub state: AgentAlertState,
    /// When the daemon observed the transition.
    pub at: DateTime<Utc>,
    /// Identity of this transition within the thread.
    ///
    /// A later transition repeats the same state (a second completion, a
    /// second approval), so the state alone cannot tell a new event from a
    /// replayed frame. The daemon gives every publish a fresh id; a daemon
    /// older than this field leaves it empty and Connect falls back to `at`.
    #[serde(default)]
    pub transition: String,
}

impl AgentActivity {
    /// The identity Connect stores with the delivered baseline.
    ///
    /// A replay of one frame carries the same value; a later transition, even
    /// with the same state, carries a different one.
    pub fn transition_id(&self) -> String {
        if self.transition.is_empty() {
            self.at.to_rfc3339()
        } else {
            self.transition.clone()
        }
    }
}

/// This machine's switch for sending agent alerts to a phone.
///
/// The setting lives on the daemon, not in a browser: the daemon is what
/// publishes, and it must stop publishing the moment the switch is off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct AgentAlertSettings {
    /// Whether activity publishing is on. Off by default.
    #[serde(default)]
    pub enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_older_frame_without_a_transition_uses_its_timestamp() {
        let at = Utc::now();
        let json = format!(
            r#"{{"thread_id":"th_one","state":"completed","at":"{}"}}"#,
            at.to_rfc3339()
        );
        let activity: AgentActivity = serde_json::from_str(&json).expect("parse");
        assert!(activity.transition.is_empty());
        assert_eq!(activity.transition_id(), at.to_rfc3339());
    }

    #[test]
    fn a_later_transition_has_its_own_identity() {
        let at = Utc::now();
        let mut activity = AgentActivity {
            thread_id: ThreadId::new("th_one"),
            thread_title: String::new(),
            project_name: String::new(),
            state: AgentAlertState::Completed,
            at,
            transition: String::new(),
        };
        // The same frame replayed is one transition.
        assert_eq!(activity.transition_id(), activity.transition_id());
        // A later completion of the same state is a different one.
        activity.transition = "later".to_string();
        assert_ne!(activity.transition_id(), at.to_rfc3339());
    }

    #[test]
    fn a_limit_asks_for_a_person_without_an_age_cutoff() {
        assert!(AgentAlertState::Limit.needs_attention());
        assert!(!AgentAlertState::Limit.is_terminal());
        assert_eq!(AgentAlertState::Limit.as_str(), "limit");
    }
}
