//! Fixture tests for the Codex `app-server` JSON-RPC normaliser.
//!
//! `basic.ndjson` was recorded from `codex-cli 0.168.0`; thread ids, item ids,
//! workspace paths, the codex home and the installation id were redacted.
//! `tool_use`, `approval`, `rate_limit`, `error` and `interrupt` are built from
//! the frames the server emits on those paths.

use h4rn_harness::{
    ApprovalRequest, CodexParser, HarnessEvent, approval_from_request, approval_response,
};
use h4rn_protocol::{AgentState, ApprovalDecision, ApprovalKind, ToolCallStatus, TurnStatus};
use serde_json::Value;

fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/codex/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {path}: {error}"))
}

fn replay(name: &str) -> (CodexParser, Vec<HarnessEvent>) {
    let mut parser = CodexParser::new();
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
            if session_id == "01a1d36f-1111-7222-8333-944455556666"
    ));
    assert_eq!(text_deltas(&events), vec!["pong"]);
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, HarnessEvent::LimitHit { .. })),
        "a 20% utilisation snapshot must not pause the thread"
    );

    let usage = events
        .iter()
        .find_map(|event| match event {
            HarnessEvent::Usage { usage } => Some(usage.clone()),
            _ => None,
        })
        .expect("usage event");
    assert_eq!(usage.input_tokens, 14_954);
    assert_eq!(usage.output_tokens, 5);
    assert_eq!(usage.cached_input_tokens, 12_160);

    match events.last() {
        Some(HarnessEvent::TurnCompleted { status, usage }) => {
            assert_eq!(*status, TurnStatus::Completed);
            assert_eq!(usage.output_tokens, 5);
            assert_eq!(usage.duration_ms, 2_800);
        }
        other => panic!("expected a completed turn, got {other:?}"),
    }

    assert_eq!(
        parser.session_id(),
        Some("01a1d36f-1111-7222-8333-944455556666")
    );
    assert_eq!(parser.model(), Some("gpt-6.1-sol"));
    assert_eq!(parser.agent_state(), AgentState::Done);

    let turn = parser.last_turn().expect("turn report");
    assert_eq!(turn.num_turns, 1);
    assert!(!turn.is_error);
    assert_eq!(turn.subtype.as_deref(), Some("completed"));
    assert_eq!(turn.duration_ms, 2_800);
}

#[test]
fn command_file_and_mcp_items_become_tool_calls() {
    let (parser, events) = replay("tool_use.ndjson");

    let calls: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            HarnessEvent::ToolCall { tool_call } => Some(tool_call),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 6, "three starts and three finishes: {calls:?}");

    assert_eq!(calls[0].name, "command");
    assert_eq!(calls[0].status, ToolCallStatus::Running);
    assert_eq!(calls[0].summary, "ls");
    assert_eq!(calls[1].tool_call_id, calls[0].tool_call_id);
    assert_eq!(calls[1].status, ToolCallStatus::Completed);
    assert_eq!(calls[1].output.as_deref(), Some("alpha.txt"));
    assert!(calls[1].completed_at.is_some());

    assert_eq!(calls[2].name, "apply_patch");
    assert_eq!(calls[2].summary, "/workspace/alpha.txt");
    assert_eq!(calls[3].status, ToolCallStatus::Completed);

    assert_eq!(calls[4].name, "mcp__h4rn__thread_status");
    assert_eq!(calls[4].status, ToolCallStatus::Running);
    assert_eq!(
        calls[4]
            .input
            .as_ref()
            .and_then(|input| input["threadId"].as_str()),
        Some("thread-1")
    );
    assert_eq!(calls[5].status, ToolCallStatus::Completed);
    assert_eq!(calls[5].output.as_deref(), Some("ok"));

    assert_eq!(text_deltas(&events), vec!["beta"]);
    assert_eq!(parser.touched_paths(), ["/workspace/alpha.txt".to_string()]);
    assert_eq!(parser.agent_state(), AgentState::Done);
}

#[test]
fn rate_limits_are_detected_once_with_a_reset_time() {
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
        Some("failed")
    );
}

#[test]
fn api_errors_surface_as_diagnostics() {
    let (parser, events) = replay("error.ndjson");

    let errors: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            HarnessEvent::Error { message, retryable } => Some((message.as_str(), *retryable)),
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].0.contains("500"));
    assert!(!errors[0].1);
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, HarnessEvent::LimitHit { .. }))
    );
    assert_eq!(parser.agent_state(), AgentState::Error);
    assert!(parser.last_turn().is_some_and(|turn| turn.is_error));
}

#[test]
fn interrupted_turns_report_the_status_and_keep_partial_text() {
    let (parser, events) = replay("interrupt.ndjson");
    assert_eq!(text_deltas(&events), vec!["partial"]);
    assert!(matches!(
        events.last(),
        Some(HarnessEvent::TurnCompleted {
            status: TurnStatus::Interrupted,
            ..
        })
    ));
    assert_eq!(parser.agent_state(), AgentState::Done);
    assert_eq!(
        parser
            .last_turn()
            .and_then(|turn| turn.subtype.clone())
            .as_deref(),
        Some("interrupted")
    );
}

#[test]
fn approval_requests_normalise_and_answer_in_the_server_shape() {
    let requests: Vec<ApprovalRequest> = fixture("approval.ndjson")
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|frame| {
            let method = frame["method"].as_str()?;
            let params = frame.get("params").cloned().unwrap_or(Value::Null);
            approval_from_request(method, &params)
        })
        .collect();
    assert_eq!(requests.len(), 5, "{requests:?}");

    let command = &requests[0];
    assert_eq!(command.kind, ApprovalKind::Command);
    assert_eq!(command.prompt, "cargo test\nneeds to run the test suite");
    assert_eq!(command.options.len(), 4);
    assert_eq!(command.options[1].id, "acceptForSession");

    let response = approval_response(
        &Value::from(10),
        &command.method,
        &Value::Null,
        ApprovalDecision::Approve,
        Some("acceptForSession"),
    );
    assert_eq!(response["id"], 10);
    assert_eq!(response["result"]["decision"], "acceptForSession");

    let file = &requests[1];
    assert_eq!(file.kind, ApprovalKind::FileChange);
    let response = approval_response(
        &Value::from(11),
        &file.method,
        &Value::Null,
        ApprovalDecision::Deny,
        None,
    );
    assert_eq!(response["result"]["decision"], "decline");

    let legacy_patch = &requests[2];
    assert_eq!(legacy_patch.method, "applyPatchApproval");
    assert_eq!(legacy_patch.kind, ApprovalKind::FileChange);
    assert!(legacy_patch.prompt.contains("/workspace/alpha.txt"));
    let response = approval_response(
        &Value::from(12),
        &legacy_patch.method,
        &Value::Null,
        ApprovalDecision::Deny,
        None,
    );
    assert_eq!(
        response["result"]["decision"]["denied"]["rejection"],
        "denied by user"
    );

    let legacy_exec = &requests[3];
    assert_eq!(legacy_exec.method, "execCommandApproval");
    assert_eq!(legacy_exec.kind, ApprovalKind::Command);
    assert_eq!(legacy_exec.prompt, "cargo test");
    let response = approval_response(
        &Value::from(13),
        &legacy_exec.method,
        &Value::Null,
        ApprovalDecision::Cancel,
        None,
    );
    assert_eq!(response["result"]["decision"], "abort");

    let question = &requests[4];
    assert_eq!(question.kind, ApprovalKind::Question);
    assert_eq!(
        question.prompt,
        "Deployment: Which environment should be used?"
    );
    assert_eq!(question.options.len(), 2);
    let response = approval_response(
        &Value::from(14),
        &question.method,
        &serde_json::json!({"questions": [{"id": "q1"}]}),
        ApprovalDecision::Approve,
        Some("staging"),
    );
    assert_eq!(response["result"]["answers"]["q1"]["answers"][0], "staging");
}

#[test]
fn malformed_lines_do_not_stop_the_stream() {
    let mut parser = CodexParser::new();
    let events = parser.push_line("Warning: something odd");
    assert!(matches!(
        events.first(),
        Some(HarnessEvent::Error {
            retryable: false,
            ..
        })
    ));
    assert!(parser.push_line("").is_empty());
    assert!(parser.push_line("{}").first().is_some_and(|event| matches!(
        event,
        HarnessEvent::Error {
            retryable: false,
            ..
        }
    )));
}
