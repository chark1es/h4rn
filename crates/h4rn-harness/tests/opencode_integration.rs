//! End-to-end test against a real `opencode serve` installation.
//!
//! Ignored by default because it needs provider credentials and spends tokens.
//! It runs two turns on one session and then resumes that session in a fresh
//! server process, which is what the daemon does after a restart.
//!
//! The provider comes from the user's OpenCode config by default. To exercise
//! h4rn's generated `OPENCODE_CONFIG_CONTENT` instead, export
//! `H4RN_OPENCODE_BASE_URL` and `H4RN_OPENCODE_API_KEY` (plus optionally
//! `H4RN_OPENCODE_PROVIDER`, `H4RN_OPENCODE_PACKAGE` and
//! `H4RN_OPENCODE_MODELS`) before running:
//!
//! ```text
//! cargo test -p h4rn-harness --test opencode_integration -- --ignored --nocapture
//! ```

use std::time::Duration;

use futures::{Stream, StreamExt};
use h4rn_harness::{
    ENV_CLIPROXY_API_KEY, ENV_CLIPROXY_BASE_URL, ENV_OPENCODE_API_KEY, ENV_OPENCODE_BASE_URL,
    ENV_OPENCODE_MODELS, ENV_OPENCODE_PACKAGE, ENV_OPENCODE_PROVIDER, Harness, HarnessEvent,
    OpenCodeHarness, SessionSpec, UserInput,
};
use h4rn_protocol::{RuntimeMode, TurnStatus};

const DEFAULT_MODEL: &str = "cliproxy/deepseek-v4.1-flash";

fn model() -> String {
    std::env::var("H4RN_OPENCODE_TEST_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into())
}

fn spec(workspace: &std::path::Path, resume_id: Option<String>) -> SessionSpec {
    let mut env_overrides = std::collections::BTreeMap::new();
    for key in [
        ENV_OPENCODE_BASE_URL,
        ENV_OPENCODE_API_KEY,
        ENV_OPENCODE_PROVIDER,
        ENV_OPENCODE_PACKAGE,
        ENV_OPENCODE_MODELS,
        ENV_CLIPROXY_BASE_URL,
        ENV_CLIPROXY_API_KEY,
    ] {
        if let Ok(value) = std::env::var(key) {
            if !value.trim().is_empty() {
                env_overrides.insert(key.to_string(), value);
            }
        }
    }

    SessionSpec {
        workspace: workspace.to_path_buf(),
        model: model(),
        runtime_mode: RuntimeMode::FullAccess,
        reasoning_effort: None,
        fast: false,
        resume_id,
        native_fork: false,
        mcp_endpoint: None,
        env_overrides,
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
#[ignore = "spawns a real opencode server and spends tokens"]
async fn real_opencode_two_turns_and_resume() {
    let harness = OpenCodeHarness::new();
    let install = harness
        .detect()
        .expect("opencode must be installed for this test");
    eprintln!(
        "using {} ({}) with model {}",
        install.path.display(),
        install.version,
        model()
    );

    let workspace = tempfile::tempdir().expect("tempdir");
    let mut session = harness
        .start_session(spec(workspace.path(), None))
        .await
        .expect("start session");
    let session_id = session.session_id().expect("an opencode session id");
    eprintln!("session {session_id}");
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
        .start_session(spec(workspace.path(), Some(session_id.clone())))
        .await
        .expect("resume session");
    assert_eq!(resumed.session_id().as_deref(), Some(session_id.as_str()));
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
