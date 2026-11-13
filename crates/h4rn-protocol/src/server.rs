//! Daemon-to-client frames, responses and events.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::common::{
    AgentState, ApprovalKind, ApprovalOption, HostSummary, Item, ThreadSummary, ToolCall,
    TurnStatus, Usage,
};
use crate::ids::{DeviceId, ThreadId, TurnId};
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
        /// Hosts this daemon can route to.
        hosts: Vec<HostSummary>,
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
    /// The harness is waiting for a decision.
    ApprovalRequested {
        /// Thread.
        thread_id: ThreadId,
        /// Turn.
        turn_id: TurnId,
        /// Opaque approval id.
        approval_id: String,
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
