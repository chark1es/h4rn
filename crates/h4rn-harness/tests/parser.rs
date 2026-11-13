//! Fixture tests for the Claude Code stream-json normaliser.
//!
//! The `basic` and `tool_use` fixtures were recorded from Claude Code 2.1.323
//! with `--max-turns`; session ids, workspace paths and MCP inventories were
//! redacted. The limit, error and interrupt fixtures are hand-written from the
//! frames the CLI produces for those paths.

use h4rn_harness::{ClaudeStreamParser, HarnessEvent};
use h4rn_protocol::{AgentState, ToolCallStatus, TurnStatus};

fn fixture(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/claude/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {path}: {error}"))
}

fn replay(name: &str) -> (ClaudeStreamParser, Vec<HarnessEvent>) {
    let mut parser = ClaudeStreamParser::new();
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
    let (parser, events) = replay("basic.ndjson");

    assert!(matches!(
        events.first(),
        Some(HarnessEvent::SessionStarted { session_id })
            if session_id == "0b7b0f0e-1111-4222-8333-944455556666"
    ));
    assert_eq!(text_deltas(&events), vec!["pong"]);
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, HarnessEvent::LimitHit { .. })),
        "an allowed_warning rate limit must not pause the thread"
    );

    let usage = events
        .iter()
        .find_map(|event| match event {
            HarnessEvent::Usage { usage } => Some(usage.clone()),
            _ => None,
        })
        .expect("usage event");
    assert!(usage.input_tokens > 0);
    assert!(usage.output_tokens > 0);
    assert!(usage.cached_input_tokens > 0);
    assert!(usage.cost_micros > 0);
    assert!(usage.duration_ms > 0);

    match events.last() {
        Some(HarnessEvent::TurnCompleted { status, usage }) => {
            assert_eq!(*status, TurnStatus::Completed);
            assert_eq!(usage.output_tokens, 4);
        }
        other => panic!("expected a completed turn, got {other:?}"),
    }

    assert_eq!(
        parser.session_id(),
        Some("0b7b0f0e-1111-4222-8333-944455556666")
    );
    assert_eq!(parser.model(), Some("claude-opus-5-5"));
    assert!(parser.tools().contains(&"Read".to_string()));
    assert_eq!(parser.agent_state(), AgentState::Done);

    let turn = parser.last_turn().expect("turn report");
    assert_eq!(turn.num_turns, 1);
    assert!(!turn.is_error);
    assert_eq!(turn.subtype.as_deref(), Some("success"));
    assert!(turn.cost_micros > 0);
}

#[test]
fn tool_use_is_running_then_completed_with_output() {
    let (parser, events) = replay("tool_use.ndjson");

    let calls: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            HarnessEvent::ToolCall { tool_call } => Some(tool_call),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 2, "one start and one finish: {calls:?}");

    assert_eq!(calls[0].name, "Read");
    assert_eq!(calls[0].status, ToolCallStatus::Running);
    assert_eq!(calls[0].summary, "/workspace/notes.txt");
    assert_eq!(
        calls[0]
            .input
            .as_ref()
            .and_then(|input| input["file_path"].as_str()),
        Some("/workspace/notes.txt")
    );

    assert_eq!(calls[1].tool_call_id, calls[0].tool_call_id);
    assert_eq!(calls[1].status, ToolCallStatus::Completed);
    assert!(
        calls[1]
            .output
            .as_deref()
            .is_some_and(|output| output.contains("beta"))
    );
    assert!(calls[1].completed_at.is_some());

    assert_eq!(text_deltas(&events), vec!["beta"]);
    assert_eq!(parser.agent_state(), AgentState::Done);
    // `Read` is not a mutating tool.
    assert!(parser.touched_paths().is_empty());
}

#[test]
fn usage_limits_are_detected_with_a_reset_time() {
    let (parser, events) = replay("rate_limit.ndjson");

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
    assert_eq!(limits[0].0.map(|at| at.timestamp()), Some(1_794_805_200));
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
        Some("error_during_execution")
    );
}

#[test]
fn api_errors_surface_as_retryable_diagnostics() {
    let (parser, events) = replay("error.ndjson");

    let errors: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            HarnessEvent::Error { message, retryable } => Some((message.as_str(), *retryable)),
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].0.contains("API Error: 500"));
    assert!(errors[0].1);
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, HarnessEvent::LimitHit { .. }))
    );
    assert_eq!(parser.agent_state(), AgentState::Error);
}

#[test]
fn interrupt_frames_are_ignored_and_the_error_result_is_reported() {
    let (parser, events) = replay("interrupt.ndjson");
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, HarnessEvent::TextDelta { .. })),
        "the synthetic interrupt text is not assistant output"
    );
    // The session rewrites this to `Interrupted` when its interrupt request is
    // outstanding; the parser alone only sees the CLI's error result.
    assert!(matches!(
        events.last(),
        Some(HarnessEvent::TurnCompleted {
            status: TurnStatus::Failed,
            ..
        })
    ));
    assert_eq!(
        parser.session_id(),
        Some("0b7b0f0e-1111-4222-8333-944455556666")
    );
}

#[test]
fn malformed_lines_do_not_stop_the_stream() {
    let mut parser = ClaudeStreamParser::new();
    let mut events = parser.push_line("Warning: something odd");
    assert!(matches!(
        events.first(),
        Some(HarnessEvent::Error {
            retryable: false,
            ..
        })
    ));

    events = parser.push_line(
        r#"{"type":"system","subtype":"init","session_id":"sess-1","model":"m","tools":["Read"]}"#,
    );
    assert!(matches!(
        events.first(),
        Some(HarnessEvent::SessionStarted { session_id }) if session_id == "sess-1"
    ));
    assert!(parser.push_line("").is_empty());
    // A JSON object without a `type` tag is malformed too, and is reported
    // rather than silently dropped.
    assert!(matches!(
        parser.push_line("{}").first(),
        Some(HarnessEvent::Error {
            retryable: false,
            ..
        })
    ));
}
