//! Back-compatibility checks for the scheduled-task additions.
//!
//! The feature only adds request and response variants, so an older client
//! frame must keep parsing exactly as before. The draft's optional ACP agent
//! id is the one new field a hand-written request may omit.

use h4rn_protocol::{ClientFrame, ClientRequest, ScheduleDraft, ScheduleSpec};

#[test]
fn an_older_client_frame_still_parses() {
    let frame: ClientFrame =
        serde_json::from_str(r#"{"id":3,"body":{"type":"list_hosts"}}"#).expect("older frame");
    assert!(matches!(frame.body, ClientRequest::ListHosts));
}

#[test]
fn a_schedule_draft_without_an_agent_id_defaults_to_none() {
    let draft: ScheduleDraft = serde_json::from_value(serde_json::json!({
        "name": "Nightly review",
        "host": "host_1",
        "workspace_path": "/home/atlas/code/api",
        "harness": "claude_code",
        "model": "claude-sonnet",
        "runtime_mode": "approval_required",
        "prompt": "Review the diff.",
        "schedule": { "kind": "daily", "time": "09:00" },
        "destination": "new_thread",
        "enabled": true
    }))
    .expect("draft without agent_id");
    assert_eq!(draft.agent_id, None);
    assert_eq!(draft.timezone, "", "an omitted zone means the daemon's own");
    assert_eq!(
        draft.schedule,
        ScheduleSpec::Daily {
            time: "09:00".into()
        }
    );
}

#[test]
fn an_unknown_schedule_kind_is_a_clean_parse_error() {
    let error = serde_json::from_value::<ScheduleSpec>(serde_json::json!({
        "kind": "whenever",
    }));
    assert!(error.is_err());
}
