//! End-to-end test against a real `claude` installation.
//!
//! Ignored by default because it needs credentials and spends tokens. Run it
//! with:
//!
//! ```text
//! cargo test -p h4rn-harness --test claude_integration -- --ignored --nocapture
//! ```

use std::time::Duration;

use futures::StreamExt;
use h4rn_harness::{ClaudeCodeHarness, Harness, HarnessEvent, SessionSpec, UserInput};
use h4rn_protocol::TurnStatus;

#[tokio::test]
#[ignore = "spawns a real claude process and spends tokens"]
async fn real_claude_code_round_trip() {
    let harness = ClaudeCodeHarness::new();
    let install = harness
        .detect()
        .expect("claude must be installed for this test");
    eprintln!("using {} ({})", install.path.display(), install.version);

    let workspace = tempfile::tempdir().expect("tempdir");
    let spec = SessionSpec {
        workspace: workspace.path().to_path_buf(),
        model: "default".into(),
        runtime_mode: h4rn_protocol::RuntimeMode::ApprovalRequired,
        reasoning_effort: None,
        fast: false,
        resume_id: None,
        mcp_endpoint: None,
        env_overrides: Default::default(),
        attachments: Vec::new(),
        agent_id: None,
    };

    let mut session = harness.start_session(spec).await.expect("start session");
    let mut events = session.events();
    session
        .send(UserInput {
            text: "Reply with exactly: pong".into(),
            attachments: Vec::new(),
        })
        .await
        .expect("send prompt");

    let (text, completed) = tokio::time::timeout(Duration::from_secs(180), async {
        let mut text = String::new();
        let mut completed = None;
        while let Some(event) = events.next().await {
            match event {
                HarnessEvent::TextDelta { text: delta } => text.push_str(&delta),
                HarnessEvent::TurnCompleted { status, usage } => {
                    completed = Some((status, usage));
                    break;
                }
                HarnessEvent::LimitHit { reset_at, .. } => {
                    panic!("hit a usage limit, reset at {reset_at:?}");
                }
                HarnessEvent::Error { message, .. } => panic!("harness error: {message}"),
                _ => {}
            }
        }
        (text, completed)
    })
    .await
    .expect("timed out waiting for the turn");

    assert!(
        text.to_lowercase().contains("pong"),
        "expected the model text to contain pong, got {text:?}"
    );
    let (status, usage) = completed.expect("a turn completion");
    assert_eq!(status, TurnStatus::Completed);
    assert!(usage.output_tokens > 0, "usage: {usage:?}");
    assert!(usage.input_tokens > 0, "usage: {usage:?}");
    assert_eq!(session.agent_state(), h4rn_protocol::AgentState::Done);
    assert!(session.session_id().is_some());

    session.shutdown().await.expect("shutdown");
}
