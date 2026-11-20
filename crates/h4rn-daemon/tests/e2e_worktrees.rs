//! Worktree tracking through the real daemon and the wire protocol.
//!
//! A worktree-isolated subagent runs on a fake harness, so the supervisor
//! creates and settles a real git worktree. The test then lists it with
//! `ListWorktrees` over the websocket and clears it with `ClearWorktrees`.

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use h4rn_daemon::auth::TokenStore;
use h4rn_daemon::config::Config;
use h4rn_daemon::daemon::DaemonBuilder;
use h4rn_daemon::harnesses::StaticHarnessCatalog;
use h4rn_daemon::threads::NewThread;
use h4rn_harness::FakeHarness;
use h4rn_protocol::{
    ClientFrame, ClientRequest, DeviceId, DeviceInfo, HarnessKind, ProtoError, ResponseBody,
    RuntimeMode, ServerFrame, SpawnSubagentRequest, SubagentIsolation, SubagentState, ThreadTab,
    TrackedWorktree, WorktreeOrigin, WorktreeStatus,
};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

struct Client {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
}

impl Client {
    async fn request(&mut self, body: ClientRequest) -> Result<ResponseBody, ProtoError> {
        let id = self.next_id;
        self.next_id += 1;
        let frame = ClientFrame { id, body };
        self.ws
            .send(Message::Text(serde_json::to_string(&frame).unwrap().into()))
            .await
            .expect("send frame");
        loop {
            let message = tokio::time::timeout(Duration::from_secs(30), self.ws.next())
                .await
                .expect("timed out waiting for a frame")
                .expect("socket closed")
                .expect("websocket error");
            if let Message::Text(text) = message
                && let ServerFrame::Response {
                    id: response_id,
                    result,
                } = serde_json::from_str(&text).expect("parse ServerFrame")
                && response_id == id
            {
                return result;
            }
        }
    }

    async fn list(&mut self) -> Vec<TrackedWorktree> {
        match self
            .request(ClientRequest::ListWorktrees)
            .await
            .expect("list worktrees")
        {
            ResponseBody::Worktrees { worktrees } => worktrees,
            other => panic!("unexpected response: {other:?}"),
        }
    }
}

fn git_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let run = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    };
    run(&["init", "-q"]);
    run(&["config", "user.email", "worktrees@h4rn.invalid"]);
    run(&["config", "user.name", "h4rn worktrees"]);
    std::fs::write(dir.path().join("README.md"), "# worktrees\n").expect("write readme");
    run(&["add", "README.md"]);
    run(&["commit", "-q", "-m", "init"]);
    dir
}

#[tokio::test]
async fn a_settled_subagent_worktree_is_listed_and_cleared_over_the_wire() {
    let repo = git_repo();
    let data = tempfile::tempdir().expect("data dir");
    let mut catalog = StaticHarnessCatalog::new();
    catalog.register(Arc::new(FakeHarness::new()));
    catalog.register(Arc::new(FakeHarness::new().with_kind(HarnessKind::Codex)));
    let config = Config {
        data_dir: data.path().to_path_buf(),
        bind: "127.0.0.1:0".parse().expect("bind"),
        extra_bind: None,
        log_level: "off".to_owned(),
        tailscale: h4rn_daemon::remote::TailscaleMode::Auto,
    };
    let daemon = DaemonBuilder::new(config)
        .with_catalog(Arc::new(catalog))
        .build()
        .await
        .expect("build daemon");
    let token = TokenStore::new(data.path())
        .load()
        .expect("read token")
        .expect("token exists");
    let manager = daemon.manager();
    let subagents = daemon.subagents();
    let bound = daemon.bind().await.expect("bind daemon");
    let addr = bound.local_addr();
    let (shutdown, rx) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        bound
            .serve(async move {
                let _ = rx.await;
            })
            .await
    });

    let (ws, _) = connect_async(format!("ws://{addr}/ws?token={token}"))
        .await
        .expect("connect");
    let mut client = Client { ws, next_id: 1 };
    client
        .request(ClientRequest::Hello {
            protocol_version: h4rn_protocol::PROTOCOL_VERSION,
            device: DeviceInfo {
                device_id: DeviceId::new("dev_worktrees"),
                name: "test".into(),
                platform: "linux".into(),
                app_version: "0.0.0".into(),
            },
        })
        .await
        .expect("hello");
    assert!(client.list().await.is_empty(), "nothing is tracked yet");

    let host =
        h4rn_daemon::hosts::HostInventory::detect(Arc::new(StaticHarnessCatalog::new())).local_id();
    let parent = manager
        .create_thread(NewThread {
            thread_id: None,
            parent_thread_id: None,
            subagent_id: None,
            host,
            harness: HarnessKind::ClaudeCode,
            workspace_path: repo.path().display().to_string(),
            model: "default".into(),
            prompt: "parent thread".into(),
            attachments: Vec::new(),
            runtime_mode: RuntimeMode::ApprovalRequired,
            worktree: None,
        })
        .await
        .expect("create parent")
        .thread_id;
    let view = subagents
        .spawn(SpawnSubagentRequest {
            parent_thread: parent,
            task: "reply with pong".into(),
            harness: Some(HarnessKind::Codex),
            host: None,
            model: None,
            isolation: SubagentIsolation::Worktree,
            context: Default::default(),
            budget: Default::default(),
        })
        .await
        .expect("spawn subagent");
    let finished = subagents
        .wait(&view.subagent_id, Duration::from_secs(20))
        .await
        .expect("wait for the child");
    assert_eq!(finished.state, SubagentState::Completed);

    // A just-finished turn keeps its thread in the Working tab for a short exit
    // grace. Clearing re-checks live activity, so wait that out first.
    let child_thread = finished.child_thread_id.clone().expect("child thread");
    for _ in 0..100 {
        if manager
            .thread_summary(&child_thread)
            .await
            .is_none_or(|summary| summary.tab != ThreadTab::Working)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Settled: the supervisor removed the directory and kept the branch.
    let listed = client.list().await;
    let entry = listed
        .iter()
        .find(|entry| entry.subagent_id.as_ref() == Some(&view.subagent_id))
        .unwrap_or_else(|| panic!("the subagent worktree must be listed: {listed:?}"));
    assert_eq!(entry.origin, WorktreeOrigin::Subagent);
    assert_eq!(entry.status, WorktreeStatus::Settled);
    assert!(entry.owned);
    assert!(!entry.exists, "the settled directory is gone");
    assert!(
        entry
            .branch
            .as_deref()
            .is_some_and(|branch| branch.starts_with("h4rn/sa-"))
    );
    let branch = entry.branch.clone().expect("branch");

    // Clear it by path.
    let response = client
        .request(ClientRequest::ClearWorktrees {
            paths: Some(vec![entry.path.clone()]),
            force: false,
        })
        .await
        .expect("clear worktrees");
    let ResponseBody::WorktreesCleared {
        removed,
        skipped,
        worktrees,
    } = response
    else {
        panic!("unexpected response: {response:?}");
    };
    assert_eq!(removed, vec![entry.path.clone()], "skipped: {skipped:?}");
    assert!(worktrees.is_empty(), "{worktrees:?}");
    let branches = std::process::Command::new("git")
        .args(["branch", "--list", &branch])
        .current_dir(repo.path())
        .output()
        .expect("list branches");
    assert!(
        String::from_utf8_lossy(&branches.stdout).trim().is_empty(),
        "clearing deletes the h4rn branch"
    );

    let _ = shutdown.send(());
    let _ = task.await;
}
