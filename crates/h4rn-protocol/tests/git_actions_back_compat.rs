//! Frames written by a client or daemon without this feature's new fields
//! still parse, so an older app and a newer daemon (or the reverse) keep
//! working.

use h4rn_protocol::{
    ClientFrame, ClientRequest, SourceControlModelMode, TextGenerationSettings, TitleModelMode,
};

fn request(json: &str) -> ClientRequest {
    serde_json::from_str::<ClientFrame>(json)
        .expect("an older frame parses")
        .body
}

#[test]
fn git_commit_without_optional_fields_keeps_its_defaults() {
    let body =
        request(r#"{"id":1,"body":{"type":"git_commit","thread_id":"th_1","message":"Fix it"}}"#);
    match body {
        ClientRequest::GitCommit {
            thread_id,
            message,
            include_unstaged,
            paths,
        } => {
            assert_eq!(thread_id.as_str(), "th_1");
            assert_eq!(message, "Fix it");
            assert!(!include_unstaged);
            assert_eq!(paths, None);
        }
        other => panic!("unexpected request: {other:?}"),
    }
}

#[test]
fn git_push_defaults_to_not_creating_an_upstream() {
    let body = request(r#"{"id":1,"body":{"type":"git_push","thread_id":"th_1"}}"#);
    match body {
        ClientRequest::GitPush { set_upstream, .. } => assert!(!set_upstream),
        other => panic!("unexpected request: {other:?}"),
    }
}

#[test]
fn an_older_forge_open_pr_is_not_a_draft() {
    let body = request(
        r#"{"id":1,"body":{"type":"forge_open_pr","thread_id":"th_1","repo":null,"title":"T","body":"B","base":"main"}}"#,
    );
    match body {
        ClientRequest::ForgeOpenPr { draft, .. } => assert!(!draft),
        other => panic!("unexpected request: {other:?}"),
    }
}

#[test]
fn an_older_text_generation_settings_file_has_source_control_off() {
    let settings: TextGenerationSettings = serde_json::from_str(
        r#"{"title":{"mode":"coding_agent","harness":null,"generator_id":null,"model":null}}"#,
    )
    .expect("older settings parse");
    assert_eq!(settings.title.mode, TitleModelMode::CodingAgent);
    assert_eq!(settings.source_control.mode, SourceControlModelMode::Off);
    assert_eq!(settings.source_control.instructions, None);
}
