//! End-to-end test against a real `codex app-server` installation.
//!
//! Ignored by default because it needs credentials and spends tokens. It runs
//! two turns on one thread and then resumes that thread in a fresh process,
//! which is exactly what the daemon does after a restart.
//!
//! ```text
//! cargo test -p h4rn-harness --test codex_integration -- --ignored --nocapture
//! ```

use std::time::Duration;

use futures::{Stream, StreamExt};
use h4rn_harness::{CodexHarness, Harness, HarnessEvent, SessionSpec, UserInput};
use h4rn_protocol::TurnStatus;

fn spec(workspace: &std::path::Path, resume_id: Option<String>) -> SessionSpec {
    SessionSpec {
        workspace: workspace.to_path_buf(),
        model: "default".into(),
        runtime_mode: h4rn_protocol::RuntimeMode::FullAccess,
        reasoning_effort: None,
        fast: false,
        resume_id,
        mcp_endpoint: None,
        env_overrides: Default::default(),
        attachments: Vec::new(),
        agent_id: None,
    }
}

async fn run_turn(
    events: &mut (impl Stream<Item = HarnessEvent> + Unpin),
    prompt: &str,
) -> (String, TurnStatus) {
    let (text, completed) = tokio::time::timeout(Duration::from_secs(240), async {
        let mut text = String::new();
        let mut completed: Option<TurnStatus> = None;
        while let Some(event) = events.next().await {
            match event {
                HarnessEvent::TextDelta { text: delta } => text.push_str(&delta),
                HarnessEvent::TurnCompleted { status, usage } => {
                    eprintln!("turn completed: {status:?} {usage:?}");
                    completed = Some(status);
                    break;
                }
                HarnessEvent::LimitHit { reset_at, .. } => {
                    panic!("hit a usage limit, reset at {reset_at:?}");
                }
                HarnessEvent::Error { message, .. } => panic!("harness error: {message}"),
                _ => {}
            }
        }
        (text, completed.expect("a turn completion"))
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {prompt:?}"));
    (text, completed)
}

#[tokio::test]
#[ignore = "spawns a real codex process and spends tokens"]
async fn real_codex_two_turns_and_resume() {
    let harness = CodexHarness::new();
    let install = harness
        .detect()
        .expect("codex must be installed for this test");
    eprintln!("using {} ({})", install.path.display(), install.version);

    let workspace = tempfile::tempdir().expect("tempdir");
    let mut session = harness
        .start_session(spec(workspace.path(), None))
        .await
        .expect("start session");
    let thread_id = session.session_id().expect("a codex thread id");
    let mut events = session.events();

    session
        .send(UserInput {
            text: "Reply with exactly: pong".into(),
            attachments: Vec::new(),
        })
        .await
        .expect("send first prompt");
    let (text, status) = run_turn(&mut events, "Reply with exactly: pong").await;
    assert_eq!(status, TurnStatus::Completed);
    assert!(
        text.to_lowercase().contains("pong"),
        "expected pong, got {text:?}"
    );

    session
        .send(UserInput {
            text: "Reply with exactly: ping".into(),
            attachments: Vec::new(),
        })
        .await
        .expect("send second prompt");
    let (text, status) = run_turn(&mut events, "Reply with exactly: ping").await;
    assert_eq!(status, TurnStatus::Completed);
    assert!(
        text.to_lowercase().contains("ping"),
        "expected ping, got {text:?}"
    );

    session.shutdown().await.expect("shutdown");

    let mut resumed = harness
        .start_session(spec(workspace.path(), Some(thread_id.clone())))
        .await
        .expect("resume session");
    assert_eq!(resumed.session_id().as_deref(), Some(thread_id.as_str()));
    let mut events = resumed.events();
    resumed
        .send(UserInput {
            text: "Reply with exactly: pong".into(),
            attachments: Vec::new(),
        })
        .await
        .expect("send resumed prompt");
    let (text, status) = run_turn(&mut events, "resumed turn").await;
    assert_eq!(status, TurnStatus::Completed);
    assert!(
        text.to_lowercase().contains("pong"),
        "expected pong after resume, got {text:?}"
    );
    resumed.shutdown().await.expect("shutdown resumed session");
}

#[tokio::test]
#[ignore = "spawns a real codex process and spends tokens"]
async fn real_codex_takes_a_steer_mid_turn() {
    let harness = CodexHarness::new();
    let install = harness
        .detect()
        .expect("codex must be installed for this test");
    eprintln!("using {} ({})", install.path.display(), install.version);

    let workspace = tempfile::tempdir().expect("tempdir");
    let mut session = harness
        .start_session(spec(workspace.path(), None))
        .await
        .expect("start session");
    let mut events = session.events();
    session
        .send(UserInput {
            text: "Write a short paragraph about tides, then stop.".into(),
            attachments: Vec::new(),
        })
        .await
        .expect("send prompt");

    tokio::time::timeout(Duration::from_secs(180), async {
        while let Some(event) = events.next().await {
            if matches!(event, HarnessEvent::TextDelta { .. }) {
                return;
            }
        }
    })
    .await
    .expect("timed out waiting for the turn to start");

    session
        .steer(UserInput {
            text: "Now reply with exactly: steered-ok".into(),
            attachments: Vec::new(),
        })
        .await
        .expect("codex app-server accepts a turn/steer");

    let completed = tokio::time::timeout(Duration::from_secs(240), async {
        while let Some(event) = events.next().await {
            if let HarnessEvent::TurnCompleted { status, usage } = event {
                return (status, usage);
            }
        }
        panic!("the event stream ended before the turn completed");
    })
    .await
    .expect("timed out waiting for the turn");
    assert_eq!(completed.0, TurnStatus::Completed);
    session.shutdown().await.expect("shutdown");
}
