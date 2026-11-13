//! Fixture tests for the ACP client and the Antigravity stream-json fallback.
//!
//! Every frame except `plan`, `diff` and `thought` was recorded from
//! `agy acp` (1.5.7) while probing a real account; session ids, workspace
//! paths and MCP inventories were redacted. The three hand-written fixtures
//! cover update kinds the probe did not happen to trigger and keep the parser
//! honest about their documented shapes.

use h4rn_harness::AgyStreamParser;
use h4rn_harness::acp::{
    AcpPermissionRequest, AcpPromptResult, AcpServerInfo, AcpSessionInfo, AcpToolKind,
    AcpToolStatus, AcpUpdate, parse_session_update,
};
use h4rn_protocol::{AgentState, TurnStatus};
use serde_json::Value;

fn fixture(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/antigravity/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {path}: {error}"))
}

fn frames(name: &str) -> Vec<Value> {
    fixture(name)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("parse fixture frame {line}: {error}"))
        })
        .collect()
}

fn result(name: &str) -> Value {
    frames(name)
        .into_iter()
        .find_map(|frame| frame.get("result").cloned())
        .unwrap_or_else(|| panic!("{name} has no result frame"))
}

fn updates(name: &str) -> Vec<AcpUpdate> {
    frames(name)
        .iter()
        .filter(|frame| frame.get("method").and_then(Value::as_str) == Some("session/update"))
        .filter_map(|frame| parse_session_update(frame.get("params")?).map(|(_, update)| update))
        .collect()
}

fn session_id(name: &str) -> String {
    frames(name)
        .iter()
        .find_map(|frame| {
            parse_session_update(frame.get("params")?).map(|(session_id, _)| session_id)
        })
        .unwrap_or_else(|| panic!("{name} has no session/update frame"))
}

#[test]
fn initialize_negotiates_capabilities_and_version() {
    let info = AcpServerInfo::from_value(&result("initialize.ndjson"));

    assert_eq!(info.protocol_version, 1);
    assert_eq!(info.agent_name, "Antigravity ACP Server");
    assert_eq!(info.agent_version, "1.5.7");
    assert!(info.capabilities.load_session);
    assert!(info.capabilities.resume_session);
    assert!(info.capabilities.close_session);
    assert!(info.capabilities.list_sessions);
    assert!(info.capabilities.image);
    assert!(!info.capabilities.audio);
    assert!(info.capabilities.embedded_context);
    assert!(info.capabilities.supports_http_mcp());
    assert!(info.capabilities.supports_load());

    let ids: Vec<&str> = info
        .auth_methods
        .iter()
        .map(|method| method.id.as_str())
        .collect();
    assert_eq!(
        ids,
        vec![
            "oauth-personal",
            "oauth-business",
            "gemini-api-key",
            "agent-platform"
        ]
    );
    // `authMethods` is documented, but the vendor's duplicate `_meta` keys must
    // not break the handshake.
    assert_eq!(
        info.raw["agentCapabilities"]["_meta"]["antigravity"]["tier"],
        true
    );
}

#[test]
fn session_new_parses_models_modes_and_config() {
    let info = AcpSessionInfo::from_value(&result("session_new.ndjson"), "fallback");

    assert_eq!(info.session_id, "00000000-0000-4000-8000-000000000001");
    assert_eq!(info.models.len(), 18);
    assert_eq!(info.models[0].model_id, "gemini-4-argon-medium");
    assert_eq!(info.models[0].name, "Gemini 4 Argon (Medium)");
    assert_eq!(
        info.current_model_id.as_deref(),
        Some("gemini-4-argon-medium")
    );
    let mode_ids: Vec<&str> = info.modes.iter().map(|mode| mode.id.as_str()).collect();
    assert_eq!(mode_ids, vec!["default", "accept-edits", "auto", "plan"]);
    assert_eq!(info.current_mode_id.as_deref(), Some("default"));
    assert!(info.config_options.is_array());
}

#[test]
fn model_resolution_maps_slugs_and_prefixes() {
    let info = AcpSessionInfo::from_value(&result("session_new.ndjson"), "fallback");

    assert_eq!(
        info.resolve_model("gemini-4-hydrogen-high").as_deref(),
        Some("gemini-4-hydrogen-high")
    );
    assert_eq!(
        info.resolve_model("Claude Opus 5.5 (High)").as_deref(),
        Some("claude-opus-5-5-high")
    );
    // The account default is the medium effort of the same family.
    assert_eq!(
        info.resolve_model("gemini-4-argon").as_deref(),
        Some("gemini-4-argon-medium")
    );
    assert_eq!(
        info.resolve_model("default").as_deref(),
        Some("gemini-4-argon-medium")
    );
    assert_eq!(info.resolve_model("not-a-real-model"), None);
}

#[test]
fn session_load_keeps_the_requested_id() {
    let params = result("session_load.ndjson");
    let info = AcpSessionInfo::from_value(&params, "resume-me");

    assert_eq!(info.session_id, "00000000-0000-4000-8000-000000000001");
    assert_eq!(info.models.len(), 18);
    assert_eq!(info.current_mode_id.as_deref(), Some("plan"));

    // Some builds omit the id; the client keeps the id it asked to load.
    let without_id = Value::Object(
        params
            .as_object()
            .map(|object| {
                object
                    .iter()
                    .filter(|(key, _)| key.as_str() != "sessionId")
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default(),
    );
    let info = AcpSessionInfo::from_value(&without_id, "resume-me");
    assert_eq!(info.session_id, "resume-me");
}

#[test]
fn pong_turn_streams_text_and_usage() {
    let updates = updates("pong.ndjson");
    assert_eq!(updates.len(), 2);

    match &updates[0] {
        AcpUpdate::UserMessageChunk { content } => {
            assert_eq!(content[0].kind, "text");
            assert_eq!(content[0].text.as_deref(), Some("Reply with exactly: pong"));
        }
        other => panic!("expected a user chunk, got {other:?}"),
    }
    match &updates[1] {
        AcpUpdate::AgentMessageChunk { content } => {
            assert_eq!(content[0].text.as_deref(), Some("pong"));
        }
        other => panic!("expected an agent chunk, got {other:?}"),
    }
    assert_eq!(updates[1].text().as_deref(), Some("pong"));
    assert_eq!(
        session_id("pong.ndjson"),
        "00000000-0000-4000-8000-000000000001"
    );

    let result = AcpPromptResult::from_value(&result("pong.ndjson"));
    assert_eq!(result.stop_reason, "end_turn");
    assert!(result.completed());
    assert!(!result.cancelled());
    assert_eq!(result.usage.input_tokens, 8_174);
    assert_eq!(result.usage.output_tokens, 13);
    assert_eq!(result.usage.thought_tokens, 8);
    assert_eq!(result.usage.total_tokens, 8_187);
    assert!(!result.usage.is_empty());
}

#[test]
fn second_turn_reports_cache_hits() {
    let result = AcpPromptResult::from_value(&result("second_turn.ndjson"));

    assert_eq!(result.stop_reason, "end_turn");
    assert_eq!(result.usage.input_tokens, 2_140);
    assert_eq!(result.usage.cached_read_tokens, 6_120);
    // The adapter folds cached tokens into input when it maps to protocol usage.
    assert_eq!(
        result.usage.input_tokens + result.usage.cached_read_tokens,
        8_260
    );
}

#[test]
fn tool_calls_track_status_locations_and_output() {
    let updates = updates("tool_call.ndjson");
    let kinds: Vec<&AcpUpdate> = updates.iter().collect();
    assert_eq!(kinds.len(), 5, "{updates:?}");

    assert!(matches!(
        &updates[0],
        AcpUpdate::SessionInfo { title } if title.as_deref() == Some("Run Command Probe Output")
    ));

    let started = match &updates[1] {
        AcpUpdate::ToolCall { tool_call } => tool_call,
        other => panic!("expected a tool call, got {other:?}"),
    };
    assert_eq!(started.kind, AcpToolKind::Execute);
    assert_eq!(started.status, AcpToolStatus::InProgress);
    assert_eq!(started.summary(), "echo h4rn_permission_probe");
    assert_eq!(
        started
            .raw_input
            .as_ref()
            .and_then(|input| input["CommandLine"].as_str()),
        Some("echo h4rn_permission_probe")
    );

    let completed = updates
        .iter()
        .find_map(|update| match update {
            AcpUpdate::ToolCallUpdate { tool_call }
                if tool_call.status == AcpToolStatus::Completed =>
            {
                Some(tool_call)
            }
            _ => None,
        })
        .expect("a completed tool call update");
    assert_eq!(completed.tool_call_id, started.tool_call_id);
    assert_eq!(completed.summary(), "echo h4rn_permission_probe");

    match updates.last() {
        Some(AcpUpdate::AgentMessageChunk { content }) => {
            assert!(
                content[0]
                    .text
                    .as_deref()
                    .is_some_and(|text| text.contains("h4rn_permission_probe"))
            );
        }
        other => panic!("expected the final text chunk, got {other:?}"),
    }

    let result = AcpPromptResult::from_value(&result("tool_call.ndjson"));
    assert!(result.completed());
    assert_eq!(result.usage.cached_read_tokens, 6_151);
}

#[test]
fn permission_requests_carry_the_tool_call_and_options() {
    let frame = frames("permission.ndjson").remove(0);
    assert_eq!(
        frame.get("method").and_then(Value::as_str),
        Some("session/request_permission")
    );
    let request = AcpPermissionRequest::from_value(frame.get("params").expect("permission params"));

    assert_eq!(request.session_id, "00000000-0000-4000-8000-000000000001");
    let tool_call = request.tool_call.expect("tool call");
    assert_eq!(tool_call.kind, AcpToolKind::Execute);
    assert_eq!(tool_call.status, AcpToolStatus::Pending);
    assert_eq!(tool_call.summary(), "echo h4rn_permission_probe");
    let options: Vec<(&str, &str)> = request
        .options
        .iter()
        .map(|option| (option.option_id.as_str(), option.kind.as_str()))
        .collect();
    assert_eq!(
        options,
        vec![("allow", "allow_once"), ("deny", "reject_once")]
    );
}

#[test]
fn plan_diff_and_thought_updates_are_typed() {
    let plan = updates("plan.ndjson");
    match &plan[0] {
        AcpUpdate::Plan { entries } => {
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0].content, "Inspect the repository");
            assert_eq!(entries[0].status, "completed");
            assert_eq!(entries[1].priority, "medium");
        }
        other => panic!("expected a plan, got {other:?}"),
    }

    let diff = updates("diff.ndjson");
    match &diff[0] {
        AcpUpdate::ToolCall { tool_call } => {
            assert_eq!(tool_call.kind, AcpToolKind::Edit);
            assert_eq!(tool_call.paths(), vec!["/workspace/src/lib.rs".to_string()]);
            let preview = tool_call.output_preview().expect("diff preview");
            assert!(preview.contains("+++ /workspace/src/lib.rs"));
            assert!(preview.contains("new\\n") || preview.contains("new\n"));
        }
        other => panic!("expected an edit call, got {other:?}"),
    }

    let thought = updates("thought.ndjson");
    match &thought[0] {
        AcpUpdate::AgentThoughtChunk { content } => {
            assert_eq!(content[0].text.as_deref(), Some("Considering the diff."));
            // The adapter decides whether reasoning text reaches the UI, so the
            // neutral client keeps it distinct from assistant output.
            assert_eq!(thought[0].text().as_deref(), Some("Considering the diff."));
        }
        other => panic!("expected a thought chunk, got {other:?}"),
    }
}

#[test]
fn stream_json_fallback_replays_a_real_turn() {
    let mut parser = AgyStreamParser::new();
    let mut events = Vec::new();
    for line in fixture("stream_pong.ndjson").lines() {
        events.extend(parser.push_line(line));
    }

    assert!(matches!(
        events.first(),
        Some(h4rn_harness::HarnessEvent::SessionStarted { session_id })
            if session_id == "00000000-0000-4000-8000-000000000001"
    ));
    let text: String = events
        .iter()
        .filter_map(|event| match event {
            h4rn_harness::HarnessEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "STREAM_PROBE\n");

    let usage = events
        .iter()
        .find_map(|event| match event {
            h4rn_harness::HarnessEvent::Usage { usage } => Some(usage.clone()),
            _ => None,
        })
        .expect("usage event");
    assert_eq!(usage.input_tokens, 8_939);
    assert_eq!(usage.output_tokens, 7);

    match events.last() {
        Some(h4rn_harness::HarnessEvent::TurnCompleted { status, usage }) => {
            assert_eq!(*status, TurnStatus::Completed);
            assert_eq!(usage.total_tokens(), 8_946);
        }
        other => panic!("expected a completed turn, got {other:?}"),
    }

    assert_eq!(
        parser.session_id(),
        Some("00000000-0000-4000-8000-000000000001")
    );
    assert!(parser.tools().contains(&"run_command".to_string()));
    assert_eq!(parser.agent_state(), AgentState::Done);
    assert_eq!(parser.last_turn().map(|turn| turn.num_turns), Some(1));
    assert!(parser.touched_paths().is_empty());
}
