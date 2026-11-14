//! A scriptable harness used by tests in this crate and downstream.
//!
//! `FakeHarness` does not pretend to speak a vendor protocol. It emits the
//! normalized [`HarnessEvent`] values directly, which is exactly what a
//! projection or lifecycle test needs, and lets a test assert the daemon's
//! response to any event sequence without a child process.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures::stream::{BoxStream, StreamExt};
use h4rn_protocol::{ApprovalDecision, HarnessKind, ToolCall, ToolCallStatus, TurnStatus, Usage};

use crate::{
    Harness, HarnessError, HarnessEvent, HarnessInstall, HarnessSession, SessionSpec, UserInput,
};

/// A fake harness adapter.
#[derive(Clone, Debug)]
pub struct FakeHarness {
    kind: HarnessKind,
    install: Option<HarnessInstall>,
    script: Option<Vec<HarnessEvent>>,
    /// Per-session scripts, consumed in order; the last one repeats.
    session_scripts: Option<Arc<SessionScripts>>,
    /// A test-only pause before a turn's input is answered.
    send_delay: Option<Duration>,
}

#[derive(Debug)]
struct SessionScripts {
    next: AtomicUsize,
    scripts: Vec<Vec<HarnessEvent>>,
}

impl SessionScripts {
    /// The next script, repeating the last one once the list is exhausted.
    fn next_script(&self) -> Vec<HarnessEvent> {
        if self.scripts.is_empty() {
            return Vec::new();
        }
        let index = self
            .next
            .fetch_add(1, Ordering::SeqCst)
            .min(self.scripts.len() - 1);
        self.scripts[index].clone()
    }
}

impl Default for FakeHarness {
    fn default() -> Self {
        Self {
            kind: HarnessKind::ClaudeCode,
            install: Some(HarnessInstall {
                path: PathBuf::from("/usr/local/bin/fake-harness"),
                version: "0.0.0-fake".into(),
            }),
            script: None,
            session_scripts: None,
            send_delay: None,
        }
    }
}

impl FakeHarness {
    /// A detected harness that echoes each message and completes the turn.
    pub fn new() -> Self {
        Self::default()
    }

    /// A harness that is not installed.
    pub fn undetected() -> Self {
        Self {
            install: None,
            ..Self::default()
        }
    }

    /// A harness that emits exactly [`events`] after `SessionStarted`.
    ///
    /// The script is replayed once per session.
    pub fn with_events(events: Vec<HarnessEvent>) -> Self {
        Self {
            script: Some(events),
            ..Self::default()
        }
    }

    /// A harness that emits a different script for each new turn.
    ///
    /// The first script runs when the session starts; later scripts run when
    /// the daemon sends another input, so a test can model "first turn hits a
    /// limit, the resumed turn succeeds". The last script repeats.
    pub fn with_session_scripts(scripts: Vec<Vec<HarnessEvent>>) -> Self {
        Self {
            session_scripts: Some(Arc::new(SessionScripts {
                next: AtomicUsize::new(0),
                scripts,
            })),
            ..Self::default()
        }
    }

    /// Overrides the adapter kind.
    pub fn with_kind(mut self, kind: HarnessKind) -> Self {
        self.kind = kind;
        self
    }

    /// Pauses before answering a turn's input (but not the priming send).
    ///
    /// A test uses this to model a slow harness whose first token arrives long
    /// after the request that started the turn.
    pub fn with_send_delay(mut self, delay: Duration) -> Self {
        self.send_delay = Some(delay);
        self
    }
}

#[async_trait]
impl Harness for FakeHarness {
    fn kind(&self) -> HarnessKind {
        self.kind
    }

    fn detect(&self) -> Option<HarnessInstall> {
        self.install.clone()
    }

    async fn start_session(
        &self,
        spec: SessionSpec,
    ) -> Result<Box<dyn HarnessSession>, HarnessError> {
        if self.install.is_none() {
            return Err(HarnessError::NotDetected { kind: self.kind });
        }

        let (sender, receiver) = unbounded();
        let session_id = spec
            .resume_id
            .clone()
            .unwrap_or_else(|| format!("fake-{}", uuid_like(&spec.workspace)));
        send(&sender, HarnessEvent::SessionStarted { session_id });

        let session_scripts = self.session_scripts.clone();
        let primed = session_scripts.is_some();
        let initial = match &session_scripts {
            Some(scripts) => scripts.next_script(),
            None => self.script.clone().unwrap_or_default(),
        };
        let scripted = session_scripts.is_some() || self.script.is_some();
        if scripted {
            for event in &initial {
                send(&sender, event.clone());
            }
        }

        // A test can slow the echo turn down so a UI e2e has time to cancel a
        // live child. Normal daemon builds never set this.
        let env_delay = std::env::var("H4RN_FAKE_HARNESS_DELAY_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or_default();
        let delay = self.send_delay.unwrap_or(env_delay);

        Ok(Box::new(FakeSession {
            kind: self.kind,
            workspace: spec.workspace,
            sender,
            receiver: Some(receiver),
            script: self.script.clone(),
            session_scripts,
            primed,
            delay,
            closed: false,
        }))
    }
}

/// The session returned by [`FakeHarness`].
pub struct FakeSession {
    kind: HarnessKind,
    workspace: PathBuf,
    sender: UnboundedSender<HarnessEvent>,
    receiver: Option<UnboundedReceiver<HarnessEvent>>,
    script: Option<Vec<HarnessEvent>>,
    session_scripts: Option<Arc<SessionScripts>>,
    /// The daemon's first `send` is the input that started the scripted turn.
    primed: bool,
    /// Env-gated pause before an echo turn, for cancel-flow e2e tests.
    delay: Duration,
    closed: bool,
}

#[async_trait]
impl HarnessSession for FakeSession {
    async fn send(&mut self, input: UserInput) -> Result<(), HarnessError> {
        if self.closed {
            return Err(HarnessError::Transport {
                harness: self.kind,
                message: "session is shut down".into(),
            });
        }

        // The priming send only announces the first script; it is not a turn
        // whose latency a test is modelling.
        if !self.delay.is_zero() && !self.primed {
            tokio::time::sleep(self.delay).await;
        }

        if let Some(scripts) = &self.session_scripts {
            if self.primed {
                self.primed = false;
                return Ok(());
            }
            for event in scripts.next_script() {
                send(&self.sender, event);
            }
            return Ok(());
        }

        if self.script.is_some() {
            // A scripted session is a complete fixture; `send` is accepted
            // (the harness contract allows it) but does not replay the script.
            return Ok(());
        }

        // Test convention: a user message may carry one write directive,
        // `[[write:relative/path]]<content>`, so daemon end-to-end tests can
        // produce a real working-tree change without a vendor harness.
        if let Some((path, _content)) = parse_write_directive(&input.text) {
            // Announce the edit first. The write itself runs on its own task
            // after a short pause: `send` returns immediately, which lets the
            // daemon's pump observe the running tool call and snapshot the
            // pre-turn bytes before the file exists. Writing inline here would
            // race that snapshot because the daemon holds the thread lock for
            // the whole `send` call.
            let now = chrono::Utc::now();
            let tool_call_id = format!("fake-edit-{now}");
            send(
                &self.sender,
                HarnessEvent::ToolCall {
                    tool_call: ToolCall {
                        tool_call_id: tool_call_id.clone(),
                        name: "Edit".into(),
                        status: ToolCallStatus::Running,
                        summary: path.to_owned(),
                        input: Some(serde_json::json!({ "path": path })),
                        output: None,
                        started_at: now,
                        completed_at: None,
                    },
                },
            );
            let sender = self.sender.clone();
            let workspace = self.workspace.clone();
            let usage = usage_for(&input.text);
            let text = input.text;
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                if let Some(written) = apply_write_directive(&workspace, &text) {
                    send(
                        &sender,
                        HarnessEvent::ToolCall {
                            tool_call: ToolCall {
                                tool_call_id,
                                name: "Edit".into(),
                                status: ToolCallStatus::Completed,
                                summary: written.clone(),
                                input: None,
                                output: Some(format!("wrote {written}")),
                                started_at: now,
                                completed_at: Some(chrono::Utc::now()),
                            },
                        },
                    );
                    send(
                        &sender,
                        HarnessEvent::TextDelta {
                            text: format!("wrote {written}"),
                        },
                    );
                }
                send(
                    &sender,
                    HarnessEvent::TextDelta {
                        text: format!("echo: {text}"),
                    },
                );
                send(
                    &sender,
                    HarnessEvent::Usage {
                        usage: usage.clone(),
                    },
                );
                send(
                    &sender,
                    HarnessEvent::TurnCompleted {
                        status: TurnStatus::Completed,
                        usage,
                    },
                );
            });
            return Ok(());
        }

        send(
            &self.sender,
            HarnessEvent::TextDelta {
                text: format!("echo: {}", input.text),
            },
        );
        // Deterministic, plausible numbers so the UI's token, cost and
        // throughput surfaces have something real to render in e2e tests.
        let usage = usage_for(&input.text);
        send(
            &self.sender,
            HarnessEvent::Usage {
                usage: usage.clone(),
            },
        );
        // Tests opt into a limit with a prompt marker; the reset delay comes
        // from the environment so a test can keep the countdown on screen.
        if input.text.contains("[h4rn-limit]") {
            let reset_ms = std::env::var("H4RN_FAKE_LIMIT_MS")
                .ok()
                .and_then(|value| value.parse::<i64>().ok())
                .unwrap_or(60_000);
            send(
                &self.sender,
                HarnessEvent::LimitHit {
                    reset_at: Some(
                        chrono::Utc::now() + chrono::Duration::milliseconds(reset_ms.max(1)),
                    ),
                    auto_resume: true,
                },
            );
        }
        send(
            &self.sender,
            HarnessEvent::TurnCompleted {
                status: TurnStatus::Completed,
                usage,
            },
        );
        Ok(())
    }

    async fn interrupt(&mut self) -> Result<(), HarnessError> {
        send(
            &self.sender,
            HarnessEvent::TurnCompleted {
                status: TurnStatus::Interrupted,
                usage: Usage::default(),
            },
        );
        Ok(())
    }

    async fn respond_approval(
        &mut self,
        approval_id: &str,
        decision: ApprovalDecision,
        option_id: Option<&str>,
    ) -> Result<(), HarnessError> {
        send(
            &self.sender,
            HarnessEvent::TextDelta {
                text: match option_id {
                    Some(option) => {
                        format!("approval {approval_id}: {decision:?} ({option})")
                    }
                    None => format!("approval {approval_id}: {decision:?}"),
                },
            },
        );
        Ok(())
    }

    fn events(&mut self) -> BoxStream<'static, HarnessEvent> {
        match self.receiver.take() {
            Some(receiver) => receiver.boxed(),
            None => futures::stream::empty().boxed(),
        }
    }

    async fn shutdown(&mut self) -> Result<(), HarnessError> {
        self.closed = true;
        // Dropping the sender would end the stream; keep it alive until the
        // session is dropped so an in-flight event is not lost.
        Ok(())
    }
}

fn send(sender: &UnboundedSender<HarnessEvent>, event: HarnessEvent) {
    // A closed receiver means the consumer went away; that is not an error for
    // the fake session, and the daemon handles the same case by shutting down.
    let _ = sender.unbounded_send(event);
}

fn uuid_like(path: &std::path::Path) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!(
        "{}-{nanos}",
        path.file_name().unwrap_or_default().to_string_lossy()
    )
}

/// A minimal spec for tests.
pub fn test_spec() -> SessionSpec {
    SessionSpec {
        workspace: PathBuf::from("/tmp/h4rn-workspace"),
        model: "test-model".into(),
        runtime_mode: h4rn_protocol::RuntimeMode::ApprovalRequired,
        resume_id: None,
        mcp_endpoint: Some("http://127.0.0.1:9100/mcp".into()),
        env_overrides: BTreeMap::new(),
        attachments: Vec::new(),
    }
}

/// Deterministic usage so the token, cost and throughput surfaces have
/// something real to render in e2e tests.
fn usage_for(text: &str) -> Usage {
    let output_tokens = (text.chars().count() as u64 / 4).max(8);
    Usage {
        input_tokens: 120 + text.chars().count() as u64 / 2,
        output_tokens,
        cached_input_tokens: 64,
        cost_micros: 0,
        duration_ms: 0,
    }
}

/// Applies the first `[[write:path]]content` directive in `text`.
///
/// Returns the relative path written. Traversal and absolute paths are
/// ignored: the fake harness must never write outside the session workspace.
fn parse_write_directive(text: &str) -> Option<(&str, &str)> {
    let start = text.find("[[write:")?;
    let rest = &text[start + "[[write:".len()..];
    let end = rest.find("]]")?;
    let relative = rest[..end].trim();
    if relative.is_empty()
        || relative.starts_with('/')
        || relative
            .split(['/', '\\'])
            .any(|component| component == ".." || component.is_empty())
    {
        return None;
    }
    let content = rest[end + 2..]
        .strip_prefix('\n')
        .unwrap_or(&rest[end + 2..]);
    Some((relative, content))
}

fn apply_write_directive(workspace: &Path, text: &str) -> Option<String> {
    let (relative, content) = parse_write_directive(text)?;
    let path = workspace.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    std::fs::write(&path, content).ok()?;
    Some(relative.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Harness, HarnessSession, UserInput};

    async fn next(session: &mut dyn HarnessSession) -> HarnessEvent {
        session.events().next().await.expect("event")
    }

    #[tokio::test]
    async fn detects_and_starts_a_session() {
        let harness = FakeHarness::new();
        assert!(harness.detect().is_some());
        assert_eq!(harness.kind(), HarnessKind::ClaudeCode);

        let mut session = harness.start_session(test_spec()).await.unwrap();
        assert!(matches!(
            next(session.as_mut()).await,
            HarnessEvent::SessionStarted { .. }
        ));
    }

    #[tokio::test]
    async fn write_directive_edits_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = test_spec();
        spec.workspace = dir.path().to_path_buf();
        let mut session = FakeHarness::new().start_session(spec).await.unwrap();
        let mut events = session.events();
        assert!(matches!(
            events.next().await,
            Some(HarnessEvent::SessionStarted { .. })
        ));
        session
            .send(UserInput {
                text: "edit it [[write:src/lib.rs]]fn main() {}\n".into(),
                attachments: Vec::new(),
            })
            .await
            .unwrap();
        assert!(matches!(
            events.next().await,
            Some(HarnessEvent::ToolCall { .. })
        ));
        assert!(matches!(
            events.next().await,
            Some(HarnessEvent::ToolCall { .. })
        ));
        assert!(matches!(
            events.next().await,
            Some(HarnessEvent::TextDelta { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
            "fn main() {}\n"
        );
        assert!(
            parse_write_directive("[[write:../escape]]x").is_none(),
            "traversal is refused"
        );
    }

    #[tokio::test]
    async fn undetected_harness_refuses_to_start() {
        let result = FakeHarness::undetected().start_session(test_spec()).await;
        assert!(matches!(result, Err(HarnessError::NotDetected { .. })));
    }

    #[tokio::test]
    async fn send_echoes_and_completes_the_turn() {
        let mut session = FakeHarness::new().start_session(test_spec()).await.unwrap();
        let mut events = session.events();
        assert!(matches!(
            events.next().await,
            Some(HarnessEvent::SessionStarted { .. })
        ));

        session
            .send(UserInput {
                text: "hello".into(),
                attachments: Vec::new(),
            })
            .await
            .unwrap();

        assert_eq!(
            events.next().await,
            Some(HarnessEvent::TextDelta {
                text: "echo: hello".into()
            })
        );
        assert!(matches!(
            events.next().await,
            Some(HarnessEvent::Usage { .. })
        ));
        assert!(matches!(
            events.next().await,
            Some(HarnessEvent::TurnCompleted {
                status: TurnStatus::Completed,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn interrupt_reports_an_interrupted_turn() {
        let mut session = FakeHarness::new().start_session(test_spec()).await.unwrap();
        let mut events = session.events();
        let _ = events.next().await;

        session.interrupt().await.unwrap();
        assert!(matches!(
            events.next().await,
            Some(HarnessEvent::TurnCompleted {
                status: TurnStatus::Interrupted,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn approvals_are_answered_through_the_event_stream() {
        let mut session = FakeHarness::new().start_session(test_spec()).await.unwrap();
        let mut events = session.events();
        let _ = events.next().await;

        session
            .respond_approval("approval-1", ApprovalDecision::Approve, Some("opt_allow"))
            .await
            .unwrap();
        assert!(matches!(
            events.next().await,
            Some(HarnessEvent::TextDelta { text }) if text.contains("approval-1")
        ));
    }

    #[tokio::test]
    async fn scripted_events_are_replayed_in_order() {
        let script = vec![
            HarnessEvent::TextDelta { text: "one".into() },
            HarnessEvent::TextDelta { text: "two".into() },
        ];
        let mut session = FakeHarness::with_events(script)
            .start_session(test_spec())
            .await
            .unwrap();
        let mut events = session.events();
        assert!(matches!(
            events.next().await,
            Some(HarnessEvent::SessionStarted { .. })
        ));
        assert_eq!(
            events.next().await,
            Some(HarnessEvent::TextDelta { text: "one".into() })
        );
        assert_eq!(
            events.next().await,
            Some(HarnessEvent::TextDelta { text: "two".into() })
        );
    }

    #[tokio::test]
    async fn limit_marker_emits_a_limit_before_completion() {
        let mut session = FakeHarness::new().start_session(test_spec()).await.unwrap();
        let mut events = session.events();
        assert!(matches!(
            events.next().await,
            Some(HarnessEvent::SessionStarted { .. })
        ));

        session
            .send(UserInput {
                text: "[h4rn-limit]".into(),
                attachments: Vec::new(),
            })
            .await
            .unwrap();

        let mut saw_usage = false;
        let mut reset_at = None;
        loop {
            match events.next().await.expect("event") {
                HarnessEvent::Usage { .. } => saw_usage = true,
                HarnessEvent::LimitHit {
                    reset_at: at,
                    auto_resume,
                } => {
                    assert!(auto_resume);
                    reset_at = at;
                }
                HarnessEvent::TurnCompleted { status, .. } => {
                    assert_eq!(status, TurnStatus::Completed);
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_usage, "limit marker still reports usage");
        assert!(reset_at.is_some(), "limit carries a reset time");
    }

    #[tokio::test]
    async fn shutdown_rejects_further_sends() {
        let mut session = FakeHarness::new().start_session(test_spec()).await.unwrap();
        session.shutdown().await.unwrap();
        assert!(
            session
                .send(UserInput {
                    text: "late".into(),
                    attachments: Vec::new(),
                })
                .await
                .is_err()
        );
    }
}
