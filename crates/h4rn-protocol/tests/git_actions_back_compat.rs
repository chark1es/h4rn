//! Frames written by a client or daemon without this feature's new fields
//! still parse, so an older app and a newer daemon (or the reverse) keep
//! working.

use h4rn_protocol::{
    ClientFrame, ClientRequest, GitPushResult, GitThreadStatus, SourceControlModelMode,
    SourceControlModelSetting, TextGenerationSettings, TextGenerationSettingsUpdate,
    TitleModelMode, TitleModelSetting,
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
fn an_older_client_saving_a_title_model_keeps_the_commit_and_pr_mode() {
    // A client built before the Commit & PR mode never sends source_control.
    let body = request(
        r#"{"id":1,"body":{"type":"set_text_generation_settings","settings":{"title":{"mode":"off","harness":null,"generator_id":null,"model":null}}}}"#,
    );
    let ClientRequest::SetTextGenerationSettings { settings } = body else {
        panic!("unexpected request: {body:?}");
    };
    let current = TextGenerationSettings {
        title: TitleModelSetting::default(),
        source_control: SourceControlModelSetting {
            mode: SourceControlModelMode::CodingAgent,
            harness: None,
            generator_id: None,
            model: None,
            instructions: Some("Use the imperative mood.".into()),
        },
    };
    let merged = settings.merged_with(&current);
    assert_eq!(merged.title.mode, TitleModelMode::Off);
    assert_eq!(
        merged.source_control.mode,
        SourceControlModelMode::CodingAgent
    );
    assert_eq!(
        merged.source_control.instructions.as_deref(),
        Some("Use the imperative mood.")
    );
}

#[test]
fn a_client_that_names_the_commit_and_pr_mode_still_turns_it_off() {
    let body = request(
        r#"{"id":1,"body":{"type":"set_text_generation_settings","settings":{"title":{"mode":"coding_agent","harness":null,"generator_id":null,"model":null},"source_control":{"mode":"off","harness":null,"generator_id":null,"model":null,"instructions":null}}}}"#,
    );
    let ClientRequest::SetTextGenerationSettings { settings } = body else {
        panic!("unexpected request: {body:?}");
    };
    let update: TextGenerationSettingsUpdate = settings;
    assert!(update.source_control.is_some());
    let current = TextGenerationSettings {
        title: TitleModelSetting::default(),
        source_control: SourceControlModelSetting {
            mode: SourceControlModelMode::TextGenerator,
            ..SourceControlModelSetting::default()
        },
    };
    assert_eq!(
        update.merged_with(&current).source_control.mode,
        SourceControlModelMode::Off
    );
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

/// Protocol 1 named the pushed branch's target `upstream`. A reply written by
/// an older daemon still parses in the new code.
#[test]
fn an_older_push_reply_parses_in_the_new_code() {
    let reply = serde_json::json!({
        "branch": "feature/widget",
        "upstream": "origin/feature/widget",
        "status": GitThreadStatus::default(),
    });
    let result: GitPushResult = serde_json::from_value(reply).expect("an older reply parses");
    assert_eq!(result.branch, "feature/widget");
    assert_eq!(result.remote_branch, "origin/feature/widget");
}

/// The new reply keeps the protocol 1 key, so an older client reads it instead
/// of showing an undefined target.
#[test]
fn a_new_push_reply_keeps_the_key_an_older_client_reads() {
    let result = GitPushResult {
        branch: "feature/widget".into(),
        remote_branch: "origin/feature/widget".into(),
        status: GitThreadStatus::default(),
    };
    let reply = serde_json::to_value(&result).expect("serialize");
    assert_eq!(reply["upstream"], "origin/feature/widget");
    assert!(
        reply.get("remote_branch").is_none(),
        "exactly one key travels: {reply}"
    );

    #[derive(serde::Deserialize)]
    struct OlderReply {
        branch: String,
        upstream: String,
    }
    let older: OlderReply = serde_json::from_value(reply).expect("an older client parses");
    assert_eq!(older.branch, "feature/widget");
    assert_eq!(older.upstream, "origin/feature/widget");
}
