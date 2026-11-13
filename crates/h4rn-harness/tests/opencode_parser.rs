//! Fixture tests for the OpenCode v2 event-stream normaliser.
//!
//! `basic.sse` was recorded from `opencode 2.2.7`; session ids, shell ids,
//! project ids, workspace paths and the shell-output path were redacted.
//! `tool_use`, `permission`, `rate_limit`, `error` and `interrupt` are built
//! from the event shapes the server emits on those paths.

use h4rn_harness::{HarnessEvent, OpenCodeParser};
use h4rn_protocol::{AgentState, ApprovalKind, ToolCallStatus, TurnStatus};

fn fixture(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/opencode/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {path}: {error}"))
}

fn replay(name: &str) -> (OpenCodeParser, Vec<HarnessEvent>) {
    let mut parser = OpenCodeParser::new();
    let mut events = Vec::new();
    for line in fixture(name).lines() {
        events.extend(parser.push_line(line));
    }
    (parser, events)
}

fn text_deltas(events: &[HarnessEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            HarnessEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn basic_turn_emits_session_text_and_usage() {
    let (parser, events) = replay("basic.sse");

    assert!(matches!(
        events.first(),
        Some(HarnessEvent::SessionStarted { session_id })
            if session_id == "ses_000000000000000000000000"
    ));
    assert_eq!(text_deltas(&events), vec!["pong"]);
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, HarnessEvent::LimitHit { .. })),
        "a normal turn must not pause the thread"
    );

    let usage = events
        .iter()
        .find_map(|event| match event {
            HarnessEvent::Usage { usage } => Some(usage.clone()),
            _ => None,
        })
        .expect("usage event");
    assert_eq!(usage.input_tokens, 8_232);
    assert_eq!(usage.output_tokens, 4);
    assert_eq!(usage.cached_input_tokens, 256);
    assert_eq!(usage.cost_micros, 0);
    assert_eq!(usage.duration_ms, 944);

    match events.last() {
        Some(HarnessEvent::TurnCompleted { status, usage }) => {
            assert_eq!(*status, TurnStatus::Completed);
            assert_eq!(usage.output_tokens, 4);
            assert_eq!(usage.duration_ms, 948);
        }
        other => panic!("expected a completed turn, got {other:?}"),
    }

    assert_eq!(parser.session_id(), Some("ses_000000000000000000000000"));
    assert_eq!(parser.model(), Some("cliproxy/deepseek-v4.1-flash"));
    assert_eq!(parser.agent_state(), AgentState::Done);
    assert!(parser.touched_paths().is_empty());

    let turn = parser.last_turn().expect("turn report");
    assert_eq!(turn.num_turns, 1);
    assert!(!turn.is_error);
    assert_eq!(turn.subtype.as_deref(), Some("completed"));
    assert_eq!(turn.stop_reason.as_deref(), Some("stop"));
    assert_eq!(turn.duration_ms, 948);
}

#[test]
fn shell_tool_calls_report_state_summary_and_output() {
    let (parser, events) = replay("tool_use.sse");

    let calls: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            HarnessEvent::ToolCall { tool_call } => Some(tool_call),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 2, "one start and one finish: {calls:?}");

    assert_eq!(calls[0].tool_call_id, "call_1");
    assert_eq!(calls[0].name, "shell");
    assert_eq!(calls[0].status, ToolCallStatus::Running);
    assert_eq!(calls[0].summary, "echo h4rn-perm-test");
    assert_eq!(
        calls[0]
            .input
            .as_ref()
            .and_then(|input| input["command"].as_str()),
        Some("echo h4rn-perm-test")
    );
    assert!(calls[1].completed_at.is_some());
    assert_eq!(calls[1].status, ToolCallStatus::Completed);
    assert_eq!(calls[1].output.as_deref(), Some("h4rn-perm-test"));

    assert_eq!(text_deltas(&events), vec!["h4rn-perm-test"]);
    assert_eq!(parser.agent_state(), AgentState::Done);
    assert!(
        parser.touched_paths().is_empty(),
        "a shell command does not touch a file"
    );

    match events.last() {
        Some(HarnessEvent::TurnCompleted { status, usage }) => {
            assert_eq!(*status, TurnStatus::Completed);
            // The turn sums both model steps, not the cumulative session
            // snapshot in `session.usage.updated`.
            assert_eq!(usage.input_tokens, 8_398);
            assert_eq!(usage.output_tokens, 52);
            assert_eq!(usage.cached_input_tokens, 8_704);
            assert_eq!(usage.duration_ms, 2_761);
        }
        other => panic!("expected a completed turn, got {other:?}"),
    }

    let turn = parser.last_turn().expect("turn report");
    assert_eq!(turn.num_turns, 2);
    assert_eq!(turn.stop_reason.as_deref(), Some("stop"));
}

#[test]
fn permission_requests_become_approval_hands() {
    let (parser, events) = replay("permission.sse");

    let approvals: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            HarnessEvent::ApprovalRequested {
                approval_id,
                kind,
                prompt,
                options,
            } => Some((approval_id, kind, prompt, options)),
            _ => None,
        })
        .collect();
    assert_eq!(approvals.len(), 1, "{approvals:?}");

    let (approval_id, kind, prompt, options) = approvals[0];
    assert_eq!(approval_id, "per_redacted");
    assert_eq!(*kind, ApprovalKind::Command);
    assert_eq!(prompt, "Run command: echo h4rn-perm-test2");
    assert_eq!(
        options
            .iter()
            .map(|option| option.id.as_str())
            .collect::<Vec<_>>(),
        vec!["once", "always", "reject"]
    );

    let calls: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            HarnessEvent::ToolCall { tool_call } => Some(tool_call),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].status, ToolCallStatus::Running);
    assert_eq!(calls[1].status, ToolCallStatus::Completed);
    assert_eq!(calls[1].output.as_deref(), Some("h4rn-perm-test2"));

    assert_eq!(text_deltas(&events), vec!["h4rn-perm-test2"]);
    assert_eq!(parser.agent_state(), AgentState::Done);

    match events.last() {
        Some(HarnessEvent::TurnCompleted { status, usage }) => {
            assert_eq!(*status, TurnStatus::Completed);
            assert_eq!(usage.input_tokens, 150);
            assert_eq!(usage.output_tokens, 54);
        }
        other => panic!("expected a completed turn, got {other:?}"),
    }
}

#[test]
fn api_errors_surface_as_diagnostics() {
    let (parser, events) = replay("error.sse");

    let errors: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            HarnessEvent::Error { message, retryable } => Some((message.as_str(), *retryable)),
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].0.contains("500"));
    assert!(errors[0].1, "a 500 is retryable");
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, HarnessEvent::LimitHit { .. }))
    );
    assert_eq!(parser.agent_state(), AgentState::Error);
    assert!(parser.last_turn().is_some_and(|turn| turn.is_error));

    match events.last() {
        Some(HarnessEvent::TurnCompleted { status, usage }) => {
            assert_eq!(*status, TurnStatus::Failed);
            assert_eq!(usage.input_tokens, 1_200);
            assert_eq!(usage.duration_ms, 300);
        }
        other => panic!("expected a failed turn, got {other:?}"),
    }
}

#[test]
fn rate_limits_are_detected_once_with_a_reset_time() {
    let (parser, events) = replay("rate_limit.sse");

    let limits: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            HarnessEvent::LimitHit {
                reset_at,
                auto_resume,
            } => Some((*reset_at, *auto_resume)),
            _ => None,
        })
        .collect();
    assert_eq!(limits.len(), 1, "one limit per turn: {limits:?}");
    assert_eq!(
        limits[0].0.map(|at| at.timestamp_millis()),
        Some(1_794_549_915_000)
    );
    assert!(limits[0].1);
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, HarnessEvent::Error { .. })),
        "a limit is not a harness error"
    );
    assert!(matches!(
        events.last(),
        Some(HarnessEvent::TurnCompleted {
            status: TurnStatus::Failed,
            ..
        })
    ));
    assert_eq!(parser.agent_state(), AgentState::RateLimited);
    assert_eq!(
        parser
            .last_turn()
            .and_then(|turn| turn.subtype.clone())
            .as_deref(),
        Some("failed")
    );
}

#[test]
fn interrupted_turns_report_the_status_and_keep_partial_text() {
    let (parser, events) = replay("interrupt.sse");
    assert_eq!(text_deltas(&events), vec!["partial answer"]);
    assert!(matches!(
        events.last(),
        Some(HarnessEvent::TurnCompleted {
            status: TurnStatus::Interrupted,
            ..
        })
    ));
    assert_eq!(parser.agent_state(), AgentState::Idle);
    assert_eq!(
        parser
            .last_turn()
            .and_then(|turn| turn.subtype.clone())
            .as_deref(),
        Some("interrupted")
    );
}

#[test]
fn bare_json_and_sse_lines_are_both_accepted() {
    let mut parser = OpenCodeParser::new();
    let first = parser.push_line(
        r#"data: {"id":"evt_1","created":1000,"type":"session.text.delta","data":{"sessionID":"ses_1","assistantMessageID":"msg_1","delta":"a"}}"#,
    );
    assert!(matches!(
        first.as_slice(),
        [
            HarnessEvent::SessionStarted { session_id },
            HarnessEvent::TextDelta { text },
        ] if session_id == "ses_1" && text == "a"
    ));

    let second = parser.push_line(
        r#"{"id":"evt_2","created":1001,"type":"session.text.delta","data":{"sessionID":"ses_1","assistantMessageID":"msg_1","delta":"b"}}"#,
    );
    assert!(matches!(
        second.as_slice(),
        [HarnessEvent::TextDelta { text }] if text == "b"
    ));

    assert!(parser.push_line(": heartbeat").is_empty());
    assert!(parser.push_line("").is_empty());
}

#[test]
fn malformed_lines_do_not_stop_the_stream() {
    let mut parser = OpenCodeParser::new();
    let events = parser.push_line("Warning: something odd");
    assert!(matches!(
        events.first(),
        Some(HarnessEvent::Error {
            retryable: false,
            ..
        })
    ));
    assert!(parser.push_line("data: [DONE]").is_empty());
    assert!(parser.push_line("{}").first().is_some_and(|event| matches!(
        event,
        HarnessEvent::Error {
            retryable: false,
            ..
        }
    )));
}
