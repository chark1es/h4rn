//! Editing an earlier prompt rewinds the conversation, and optionally its files.
//!
//! A rewind is one durable decision (ADR-050). The daemon truncates the thread
//! at the chosen user message, tells the harness to drop that message and
//! everything after it from its own history, and can put the workspace back to
//! the turn snapshot taken before the message. The wire types here are shared
//! by that request, its answer and the event every client folds.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// This machine's Labs switches for rewinding a thread.
///
/// File restore is off until the person turns it on in Settings → Labs →
/// **Rewind files with Edit from here**. While it is off the daemon refuses a
/// rewind that asks for files, the thread summary reports file rewind as
/// unavailable, and the dialog offers only the conversation rewind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct RewindSettings {
    /// Whether Settings → Labs → **Rewind files with Edit from here** is on for
    /// this machine.
    #[serde(default)]
    pub files_enabled: bool,
}

/// What one selected prompt's rewind will really do.
///
/// The thread's harness only says what it *can* do. A prompt recorded before
/// the provider boundary was kept, or a conversation that never started a
/// provider session, can only use a handoff. A preview names the method the
/// request would use, so the dialog never promises the wrong one.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct RewindPlan {
    /// The method the next rewind of this prompt would use.
    #[serde(default)]
    pub method: RewindMethod,
    /// Whether file restore is turned on for this machine (Settings → Labs).
    #[serde(default)]
    pub files_enabled: bool,
    /// Whether **Revert files too** would be accepted for this prompt.
    #[serde(default)]
    pub files_available: bool,
    /// Paths the removed turns touched that git ignored when the checkpoint
    /// was taken, so **Revert files too** leaves them exactly as they are.
    #[serde(default)]
    pub left_alone: Vec<String>,
}

/// How a harness drops the rewound turns from its own history.
///
/// The method is advertised per thread so the UI can say what will really
/// happen instead of guessing. A native truncation keeps the provider session
/// and its earlier turns; a handoff starts a fresh session and carries the
/// kept conversation in a bounded preface.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum RewindMethod {
    /// The harness can drop the turns itself.
    Native,
    /// h4rn starts a fresh session with the kept history as a handoff.
    Handoff,
    /// Neither is possible for this thread's harness.
    #[default]
    Unsupported,
}

/// What a session started after a rewind must do to its provider history.
///
/// The daemon records this with the thread until the next session starts, so a
/// restarted daemon still rewinds the provider instead of silently resuming
/// the old conversation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionRewind {
    /// Resume the session but discard the prompt with this harness message id
    /// and everything after it (Claude Code's truncating resume).
    ResumeAtMessage {
        /// The last kept chain entry: the harness message id of the last
        /// message that stays, an assistant reply or the kept prompt itself
        /// when that prompt's turn wrote none.
        message: String,
    },
    /// Resume the session, then drop this provider turn and everything after
    /// it (Codex's `thread/revert`).
    BeforeTurn {
        /// The first removed provider turn id, as Codex accepted it.
        turn: String,
    },
}

/// A pending rewind that the next session start must apply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PendingRewind {
    /// Keep the provider session and truncate it at the next start.
    Native {
        /// The boundary the adapter applies.
        session: SessionRewind,
    },
    /// Start a fresh session and give the kept conversation as a handoff.
    Handoff,
}

/// The prompt a rewind returns to the composer, with its attachments.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct RewoundPrompt {
    /// Thread that was rewound.
    pub thread_id: crate::ThreadId,
    /// The user message the active thread now ends before.
    pub before_item_id: String,
    /// The prompt text exactly as it was sent.
    pub prompt: String,
    /// Attachments that came with it, in their original order.
    #[serde(default)]
    pub attachments: Vec<crate::Attachment>,
    /// Whether the workspace files went back to the pre-message checkpoint.
    #[serde(default)]
    pub files_restored: bool,
    /// Paths the removed turns touched that git ignored when the checkpoint
    /// was taken. They were left exactly as they are, never rewound.
    #[serde(default)]
    pub left_alone: Vec<String>,
    /// What the harness history did.
    pub method: RewindMethod,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rewind_method_has_an_unsupported_default() {
        assert_eq!(RewindMethod::default(), RewindMethod::Unsupported);
        assert_eq!(
            serde_json::to_string(&RewindMethod::Handoff).unwrap(),
            "\"handoff\""
        );
    }

    #[test]
    fn a_pending_native_rewind_names_its_boundary() {
        let pending = PendingRewind::Native {
            session: SessionRewind::BeforeTurn {
                turn: "turn-b".into(),
            },
        };
        let value = serde_json::to_value(&pending).unwrap();
        assert_eq!(value["kind"], "native");
        assert_eq!(value["session"]["kind"], "before_turn");
        assert_eq!(value["session"]["turn"], "turn-b");
        assert_eq!(
            serde_json::from_value::<PendingRewind>(value).unwrap(),
            pending
        );
        assert_eq!(
            serde_json::to_string(&PendingRewind::Handoff).unwrap(),
            "{\"kind\":\"handoff\"}"
        );
    }

    #[test]
    fn rewind_settings_and_plans_default_to_off_and_unsupported() {
        let settings: RewindSettings = serde_json::from_str("{}").unwrap();
        assert!(!settings.files_enabled, "file restore is off by default");
        let plan: RewindPlan = serde_json::from_str("{}").unwrap();
        assert!(!plan.files_enabled);
        assert!(!plan.files_available);
        assert_eq!(plan.method, RewindMethod::Unsupported);
        assert!(plan.left_alone.is_empty());
        let plan: RewindPlan =
            serde_json::from_str(r#"{"method":"native","files_enabled":true}"#).unwrap();
        assert!(plan.files_enabled);
        assert!(!plan.files_available, "a missing field is not a yes");
        assert!(plan.left_alone.is_empty(), "a missing field names nothing");
    }

    #[test]
    fn an_older_rewound_prompt_without_file_or_attachment_fields_parses() {
        let value = serde_json::json!({
            "thread_id": "th_1",
            "before_item_id": "item_user_1",
            "prompt": "try again",
            "method": "handoff"
        });
        let prompt: RewoundPrompt = serde_json::from_value(value).unwrap();
        assert!(prompt.attachments.is_empty());
        assert!(!prompt.files_restored);
        assert!(prompt.left_alone.is_empty());
    }
}
