//! The Codex adapter's native rewind: `thread/revert` at the next start.
//!
//! A small script stands in for `codex app-server`; it records the revert it
//! is asked to perform so the test can prove the boundary names the first
//! removed provider turn.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use h4rn_harness::{CodexHarness, Harness, HarnessError, HarnessInstall, SessionSpec};
use h4rn_protocol::{HarnessKind, RuntimeMode, SessionRewind};

fn install_script(log: &std::path::Path) -> PathBuf {
    let path = log.with_file_name("fake-codex");
    let body = format!(
        r#"#!/usr/bin/env python3
import json, sys
log = open({log:?}, "a")
for line in sys.stdin:
    try:
        request = json.loads(line)
    except Exception:
        continue
    method = request.get("method")
    ident = request.get("id")
    params = request.get("params") or {{}}
    if method == "thread/revert":
        log.write(json.dumps(params) + "\n")
        log.flush()
        result = {{}}
    elif method in ("thread/resume", "thread/start"):
        turns = [{{"id": "turn-a"}}, {{"id": "turn-b"}}] if method == "thread/resume" else []
        result = {{"thread": {{"id": "thread-1", "turns": turns}}, "model": "gpt-test"}}
    elif method == "model/list":
        result = {{"data": []}}
    elif method == "turn/start":
        result = {{"turn": {{"id": "turn-c"}}}}
    else:
        result = {{}}
    print(json.dumps({{"id": ident, "result": result}}), flush=True)
"#
    );
    std::fs::write(&path, body).expect("script");
    let mut permissions = std::fs::metadata(&path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).expect("chmod");
    path
}

fn spec(
    workspace: PathBuf,
    resume_id: Option<String>,
    rewind: Option<SessionRewind>,
) -> SessionSpec {
    std::fs::create_dir_all(&workspace).expect("workspace");
    SessionSpec {
        workspace,
        model: "default".into(),
        runtime_mode: RuntimeMode::FullAccess,
        reasoning_effort: None,
        fast: false,
        resume_id,
        mcp_endpoint: None,
        env_overrides: Default::default(),
        attachments: Vec::new(),
        agent_id: None,
        rewind,
    }
}

#[tokio::test]
async fn a_pending_codex_rewind_reverts_the_provider_thread_before_the_first_prompt() {
    let directory = tempfile::tempdir().expect("tempdir");
    let log = directory.path().join("reverts.jsonl");
    let path = install_script(&log);
    let harness = CodexHarness::with_install(HarnessInstall {
        path,
        version: "test".into(),
    });

    let mut session = harness
        .start_session(spec(
            directory.path().join("workspace"),
            Some("thread-1".into()),
            Some(SessionRewind::BeforeTurn {
                turn: "turn-b".into(),
            }),
        ))
        .await
        .expect("start");
    session.shutdown().await.expect("shutdown");

    let line = std::fs::read_to_string(&log).expect("log");
    let revert: serde_json::Value =
        serde_json::from_str(line.lines().next().expect("a revert")).expect("revert json");
    assert_eq!(revert["threadId"], "thread-1");
    assert_eq!(
        revert["beforeTurnId"], "turn-b",
        "the second provider turn is the first one removed"
    );
}

#[tokio::test]
async fn codex_refuses_a_boundary_its_conversation_does_not_know() {
    let directory = tempfile::tempdir().expect("tempdir");
    let log = directory.path().join("reverts.jsonl");
    let path = install_script(&log);
    let harness = CodexHarness::with_install(HarnessInstall {
        path,
        version: "test".into(),
    });
    let error = match harness
        .start_session(spec(
            directory.path().join("workspace"),
            Some("thread-1".into()),
            Some(SessionRewind::BeforeTurn {
                turn: "turn-missing".into(),
            }),
        ))
        .await
    {
        Ok(_) => panic!("an unknown boundary is not a success"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        HarnessError::Unsupported {
            harness: HarnessKind::Codex,
            ..
        }
    ));
    assert!(
        !log.exists()
            || std::fs::read_to_string(&log)
                .expect("log")
                .trim()
                .is_empty(),
        "thread/revert was never asked to act on a turn the conversation does not hold"
    );
}

#[tokio::test]
async fn codex_refuses_a_boundary_only_claude_code_can_name() {
    let directory = tempfile::tempdir().expect("tempdir");
    let log = directory.path().join("reverts.jsonl");
    let path = install_script(&log);
    let harness = CodexHarness::with_install(HarnessInstall {
        path,
        version: "test".into(),
    });
    let error = match harness
        .start_session(spec(
            directory.path().join("workspace"),
            Some("thread-1".into()),
            Some(SessionRewind::ResumeAtMessage {
                message: "message-1".into(),
            }),
        ))
        .await
    {
        Ok(_) => panic!("Codex must refuse a Claude Code message boundary"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        HarnessError::Unsupported {
            harness: HarnessKind::Codex,
            ..
        }
    ));
}

#[test]
fn codex_advertises_a_native_rewind() {
    assert_eq!(
        CodexHarness::new().rewind_method(),
        h4rn_protocol::RewindMethod::Native
    );
}

#[cfg(unix)]
#[test]
fn the_script_is_executable() {
    let directory = tempfile::tempdir().expect("tempdir");
    let log = directory.path().join("reverts.jsonl");
    let path = install_script(&log);
    let mode = std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode();
    assert_eq!(mode & 0o111, 0o111);
}
