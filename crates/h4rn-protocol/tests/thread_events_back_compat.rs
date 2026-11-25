//! Older thread frames still parse after the harness-switch additions.

use h4rn_protocol::{ClientRequest, Event, HarnessKind};

#[test]
fn an_older_turn_started_event_has_no_harness() {
    let event: Event = serde_json::from_value(serde_json::json!({
        "type": "turn_started",
        "thread_id": "th_1",
        "turn_id": "turn_1",
        "started_at": "2026-11-20T10:00:00Z",
    }))
    .expect("an older frame parses");
    match event {
        Event::TurnStarted { harness, .. } => assert_eq!(harness, None),
        other => panic!("wrong event: {other:?}"),
    }
}

#[test]
fn a_harness_switch_event_names_both_harnesses() {
    let event: Event = serde_json::from_value(serde_json::json!({
        "type": "harness_switched",
        "thread_id": "th_1",
        "from": "claude_code",
        "to": "codex",
    }))
    .expect("switch event parses");
    assert_eq!(
        event,
        Event::HarnessSwitched {
            thread_id: h4rn_protocol::ThreadId::new("th_1"),
            from: HarnessKind::ClaudeCode,
            to: HarnessKind::Codex,
        }
    );
}

#[test]
fn a_switch_request_without_an_agent_id_still_parses() {
    let request: ClientRequest = serde_json::from_value(serde_json::json!({
        "type": "switch_thread_harness",
        "thread_id": "th_1",
        "harness": "codex",
        "model": "gpt-5.1-codex",
    }))
    .expect("request parses");
    match request {
        ClientRequest::SwitchThreadHarness {
            harness, agent_id, ..
        } => {
            assert_eq!(harness, HarnessKind::Codex);
            assert_eq!(agent_id, None);
        }
        other => panic!("wrong request: {other:?}"),
    }
}
