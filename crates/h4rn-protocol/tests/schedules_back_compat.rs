//! Back-compatibility checks for the scheduled-task additions.
//!
//! The feature only adds request and response variants, so an older client
//! frame must keep parsing exactly as before. The draft's optional ACP agent
//! id is the one new field a hand-written request may omit.

use h4rn_protocol::{ClientFrame, ClientRequest, ScheduleDraft, ScheduleSpec, ScheduleTask};

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

#[test]
fn a_task_body_keeps_the_wire_field_and_hides_the_slot_identity() {
    // An older daemon sends `once_consumed` as a boolean. The slot identity is
    // daemon state, so it never crosses the wire in either direction.
    let mut task: ScheduleTask = serde_json::from_value(serde_json::json!({
        "task_id": "sch_1",
        "name": "Nightly review",
        "host": "host_1",
        "workspace_path": "/home/atlas/code/api",
        "harness": "claude_code",
        "agent_id": null,
        "model": "claude-sonnet",
        "runtime_mode": "approval_required",
        "prompt": "Review the diff.",
        "schedule": { "kind": "once", "date": "2026-12-01", "time": "09:30" },
        "timezone": "Europe/Berlin",
        "destination": "new_thread",
        "enabled": true,
        "thread_id": null,
        "webhook_path_set": false,
        "once_consumed": false,
        "next_run_at": null,
        "last_run": null,
        "created_at": "2026-11-20T10:00:00Z",
        "updated_at": "2026-11-20T10:00:00Z"
    }))
    .expect("an older task body still parses");
    assert!(!task.once_consumed);
    assert!(task.once_consumed_slot.is_none());

    task.once_consumed = true;
    task.once_consumed_slot = Some("2026-12-01T09:30".into());
    let json = serde_json::to_string(&task).expect("serialise");
    assert!(json.contains("\"once_consumed\":true"));
    assert!(
        !json.contains("once_consumed_slot"),
        "the spent slot identity must stay daemon-side"
    );
}
