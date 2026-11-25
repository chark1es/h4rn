//! Phone alerts for agent activity (ADR-049).
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
}

impl AgentAlertState {
    /// Stable spelling used in push data and the delivered baseline.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Approval => "approval",
            Self::Input => "input",
        }
    }

    /// Parses the stored spelling.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "approval" => Some(Self::Approval),
            "input" => Some(Self::Input),
            _ => None,
        }
    }

    /// Whether this is a finishing state (subject to the two-minute cutoff).
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }

    /// Whether this state is the agent asking for a person.
    pub fn needs_attention(self) -> bool {
        matches!(self, Self::Approval | Self::Input)
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
