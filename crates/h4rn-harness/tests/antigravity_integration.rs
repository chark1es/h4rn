//! End-to-end tests against a real `agy` installation.
//!
//! Ignored by default because they need a signed-in Antigravity account and
//! spend tokens. Run them with:
//!
//! ```text
//! cargo test -p h4rn-harness --test antigravity_integration -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Every test pins `H4RN_AGY_TRANSPORT=acp` so the ACP transport is exercised;
//! the stream-json fallback has its own parser tests. ACP turns can take
//! minutes, so each read is bounded by [`TURN_TIMEOUT`].

use std::collections::BTreeMap;
use std::time::Duration;

use futures::StreamExt;
use futures::stream::BoxStream;
use h4rn_harness::{
    AntigravityHarness, ENV_AGY_TRANSPORT, Harness, HarnessEvent, HarnessSession, SessionSpec,
    UserInput,
};
use h4rn_protocol::{AgentState, RuntimeMode, TurnStatus};

const TURN_TIMEOUT: Duration = Duration::from_secs(180);

fn spec(workspace: &std::path::Path, resume_id: Option<String>) -> SessionSpec {
    let mut env_overrides = BTreeMap::new();
    env_overrides.insert(ENV_AGY_TRANSPORT.to_string(), "acp".to_string());
    SessionSpec {
        workspace: workspace.to_path_buf(),
        model: "default".into(),
        runtime_mode: RuntimeMode::ApprovalRequired,
        reasoning_effort: None,
        fast: false,
        resume_id,
        mcp_endpoint: None,
        env_overrides,
        attachments: Vec::new(),
        agent_id: None,
    }
}

async fn start(
    harness: &AntigravityHarness,
    workspace: &std::path::Path,
    resume_id: Option<String>,
) -> (Box<dyn HarnessSession>, BoxStream<'static, HarnessEvent>) {
    let mut session = harness
        .start_session(spec(workspace, resume_id))
        .await
        .expect("start ACP session");
    let events = session.events();
    (session, events)
}

/// Sends one prompt and drains events until the turn completes.
async fn run_turn(
    session: &mut Box<dyn HarnessSession>,
    events: &mut BoxStream<'static, HarnessEvent>,
    prompt: &str,
) -> (String, TurnStatus) {
    session
        .send(UserInput {
            text: prompt.into(),
            attachments: Vec::new(),
        })
        .await
        .expect("send prompt");

    let (text, completed) = tokio::time::timeout(TURN_TIMEOUT, async {
        let mut text = String::new();
        let mut completed = None;
        while let Some(event) = events.next().await {
            match event {
                HarnessEvent::TextDelta { text: delta } => text.push_str(&delta),
                HarnessEvent::TurnCompleted { status, usage } => {
                    eprintln!(
                        "turn finished: {status:?}, in={} out={} cached={}",
                        usage.input_tokens, usage.output_tokens, usage.cached_input_tokens
                    );
                    completed = Some(status);
                    break;
                }
                HarnessEvent::LimitHit { reset_at, .. } => {
                    panic!("hit a usage limit, reset at {reset_at:?}");
                }
                HarnessEvent::Error { message, .. } => panic!("harness error: {message}"),
                HarnessEvent::ApprovalRequested { prompt, .. } => {
                    panic!("unexpected approval request during a text-only turn: {prompt}")
                }
                _ => {}
            }
        }
        (text, completed)
    })
    .await
    .expect("timed out waiting for the turn");

    let status = completed.expect("a turn completion");
    (text, status)
}

fn assert_completed(status: TurnStatus) {
    assert_eq!(status, TurnStatus::Completed, "the turn did not complete");
}

#[tokio::test]
#[ignore = "spawns a real agy process and spends tokens"]
async fn real_antigravity_pong() {
    let harness = AntigravityHarness::new();
    let install = harness
        .detect()
        .expect("agy must be installed for this test");
    eprintln!("using {} ({})", install.path.display(), install.version);

    let workspace = tempfile::tempdir().expect("tempdir");
    let (mut session, mut events) = start(&harness, workspace.path(), None).await;
    let session_id = session.session_id().expect("a session id");
    eprintln!("session {session_id}");

    let (text, status) = run_turn(&mut session, &mut events, "Reply with exactly: pong").await;
    assert_completed(status);
    assert!(
        text.to_lowercase().contains("pong"),
        "expected the model text to contain pong, got {text:?}"
    );
    assert_eq!(session.agent_state(), AgentState::Done);
    assert!(session.last_turn().is_some(), "a turn report");
    assert_eq!(session.session_id().as_deref(), Some(session_id.as_str()));

    session.shutdown().await.expect("shutdown");
}

#[tokio::test]
#[ignore = "spawns a real agy process and spends tokens"]
async fn real_antigravity_second_turn_keeps_the_session() {
    let harness = AntigravityHarness::new();
    harness
        .detect()
        .expect("agy must be installed for this test");

    let workspace = tempfile::tempdir().expect("tempdir");
    let (mut session, mut events) = start(&harness, workspace.path(), None).await;
    let session_id = session.session_id().expect("a session id");

    let (first, status) = run_turn(&mut session, &mut events, "Reply with exactly: pong").await;
    assert_completed(status);
    assert!(
        first.to_lowercase().contains("pong"),
        "expected pong, got {first:?}"
    );

    // The same process, the same ACP session id: this is the multi-turn path
    // the daemon uses for a live thread.
    let (second, status) = run_turn(&mut session, &mut events, "Reply with exactly: ping").await;
    assert_completed(status);
    assert!(
        second.to_lowercase().contains("ping"),
        "expected ping, got {second:?}"
    );
    assert_eq!(session.session_id().as_deref(), Some(session_id.as_str()));
    assert_eq!(session.agent_state(), AgentState::Done);

    session.shutdown().await.expect("shutdown");
}

#[tokio::test]
#[ignore = "spawns a real agy process and spends tokens"]
async fn real_antigravity_resumes_after_restart() {
    let harness = AntigravityHarness::new();
    harness
        .detect()
        .expect("agy must be installed for this test");
    let workspace = tempfile::tempdir().expect("tempdir");

    let (mut session, mut events) = start(&harness, workspace.path(), None).await;
    let session_id = session.session_id().expect("a session id");
    eprintln!("first session {session_id}");

    let (first, status) = run_turn(
        &mut session,
        &mut events,
        "Remember the codeword ORCHID for later. Reply with exactly: stored",
    )
    .await;
    assert_completed(status);
    assert!(
        first.to_lowercase().contains("stored"),
        "expected stored, got {first:?}"
    );
    session.shutdown().await.expect("shutdown");

    // A fresh process must recover the conversation through `session/load`.
    let (mut resumed, mut events) =
        start(&harness, workspace.path(), Some(session_id.clone())).await;
    assert_eq!(
        resumed.session_id().as_deref(),
        Some(session_id.as_str()),
        "session/load must keep the requested session id"
    );

    let (answer, status) = run_turn(
        &mut resumed,
        &mut events,
        "What was the codeword I asked you to remember? Reply with just the codeword.",
    )
    .await;
    assert_completed(status);
    assert!(
        answer.to_lowercase().contains("orchid"),
        "expected the resumed conversation to remember ORCHID, got {answer:?}"
    );
    assert_eq!(resumed.agent_state(), AgentState::Done);

    resumed.shutdown().await.expect("shutdown");
}
