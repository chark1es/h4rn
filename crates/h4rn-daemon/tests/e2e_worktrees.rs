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
    RuntimeMode, ServerFrame, SpawnSubagentRequest, SubagentIsolation, SubagentState,
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
        network_access: None,
        network_port: 0,
    };
    let daemon = DaemonBuilder::new(config)
        // Titles never spawn the machine's own claude or codex.
        .without_title_agents()
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
            agent_id: None,
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

    // The supervisor removes the directory and keeps the branch when the
    // child settles. The listing is computed fresh each time, so poll it
    // instead of guessing how long the exit grace lasts.
    let mut settled = None;
    for _ in 0..600 {
        let listed = client.list().await;
        if let Some(found) = listed
            .iter()
            .find(|entry| entry.subagent_id.as_ref() == Some(&view.subagent_id))
            .filter(|entry| entry.status == WorktreeStatus::Settled)
        {
            settled = Some(found.clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let entry = settled.unwrap_or_else(|| panic!("the subagent worktree must settle"));

    // Settled: the supervisor removed the directory and kept the branch.
    assert_eq!(entry.origin, WorktreeOrigin::Subagent);
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
            bulk: false,
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

#[tokio::test]
async fn clearing_a_worktree_while_its_child_still_works_skips_it() {
    let repo = git_repo();
    let data = tempfile::tempdir().expect("data dir");
    let mut catalog = StaticHarnessCatalog::new();
    catalog.register(Arc::new(FakeHarness::new()));
    // A child that keeps its first turn open, so it is still busy when the
    // clear request arrives.
    catalog.register(Arc::new(
        FakeHarness::with_events(Vec::new()).with_kind(HarnessKind::Codex),
    ));
    let config = Config {
        data_dir: data.path().to_path_buf(),
        bind: "127.0.0.1:0".parse().expect("bind"),
        extra_bind: None,
        log_level: "off".to_owned(),
        tailscale: h4rn_daemon::remote::TailscaleMode::Auto,
        network_access: None,
        network_port: 0,
    };
    let daemon = DaemonBuilder::new(config)
        // Titles never spawn the machine's own claude or codex.
        .without_title_agents()
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
                device_id: DeviceId::new("dev_worktrees_busy"),
                name: "test".into(),
                platform: "linux".into(),
                app_version: "0.0.0".into(),
            },
        })
        .await
        .expect("hello");

    let host =
        h4rn_daemon::hosts::HostInventory::detect(Arc::new(StaticHarnessCatalog::new())).local_id();
    let parent = manager
        .create_thread(NewThread {
            thread_id: None,
            parent_thread_id: None,
            subagent_id: None,
            host,
            harness: HarnessKind::ClaudeCode,
            agent_id: None,
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
            task: "keep working".into(),
            harness: Some(HarnessKind::Codex),
            host: None,
            model: None,
            isolation: SubagentIsolation::Worktree,
            context: Default::default(),
            budget: Default::default(),
        })
        .await
        .expect("spawn subagent");

    // The child's checkout exists and is active while its turn runs.
    let mut busy = None;
    for _ in 0..600 {
        let listed = client.list().await;
        if let Some(found) = listed
            .iter()
            .find(|entry| entry.subagent_id.as_ref() == Some(&view.subagent_id))
            .filter(|entry| entry.status == WorktreeStatus::Active)
        {
            busy = Some(found.clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let entry = busy.unwrap_or_else(|| panic!("the running child's worktree is active"));
    assert!(entry.exists);

    let response = client
        .request(ClientRequest::ClearWorktrees {
            paths: Some(vec![entry.path.clone()]),
            force: false,
            bulk: false,
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
    assert!(removed.is_empty(), "a busy worktree is never removed");
    assert!(skipped[0].reason.starts_with("in use by "), "{skipped:?}");
    assert!(
        std::path::Path::new(&entry.path).is_dir(),
        "the checkout stays on disk"
    );
    assert!(
        worktrees.iter().any(|worktree| worktree.path == entry.path),
        "the busy worktree is still listed: {worktrees:?}"
    );

    let _ = subagents.cancel(&view.subagent_id).await;
    let _ = shutdown.send(());
    let _ = task.await;
}

#[tokio::test]
async fn a_thread_in_a_linked_checkout_of_a_bare_repository_is_tracked_and_its_subagent_cleared() {
    // `project.git` is bare; the parent thread works in a linked checkout of it.
    let origin = git_repo();
    let root = tempfile::tempdir().expect("root");
    let run = |dir: &std::path::Path, args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    };
    run(
        root.path(),
        &[
            "clone",
            "-q",
            "--bare",
            &origin.path().display().to_string(),
            "project.git",
        ],
    );
    let bare = std::fs::canonicalize(root.path().join("project.git")).expect("bare");
    let checkout = root.path().join("project-wt");
    run(
        &bare,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "bare-feature",
            &checkout.display().to_string(),
        ],
    );
    let checkout = std::fs::canonicalize(&checkout).expect("checkout");

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
        network_access: None,
        network_port: 0,
    };
    let daemon = DaemonBuilder::new(config)
        // Titles never spawn the machine's own claude or codex.
        .without_title_agents()
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
                device_id: DeviceId::new("dev_worktrees_bare"),
                name: "test".into(),
                platform: "linux".into(),
                app_version: "0.0.0".into(),
            },
        })
        .await
        .expect("hello");

    let host =
        h4rn_daemon::hosts::HostInventory::detect(Arc::new(StaticHarnessCatalog::new())).local_id();
    let parent = manager
        .create_thread(NewThread {
            thread_id: None,
            parent_thread_id: None,
            subagent_id: None,
            host,
            harness: HarnessKind::ClaudeCode,
            agent_id: None,
            workspace_path: checkout.display().to_string(),
            model: "default".into(),
            prompt: "parent thread".into(),
            attachments: Vec::new(),
            runtime_mode: RuntimeMode::ApprovalRequired,
            worktree: None,
        })
        .await
        .expect("create parent")
        .thread_id;

    // The parent's checkout is listed as a thread row of the bare repository.
    let checkout_path = checkout.display().to_string();
    let bare_path = bare.display().to_string();
    let listed = client.list().await;
    let row = listed
        .iter()
        .find(|entry| entry.path == checkout_path)
        .unwrap_or_else(|| panic!("the thread's checkout is listed: {listed:?}"));
    assert_eq!(row.origin, WorktreeOrigin::Thread);
    assert_eq!(row.repo, bare_path);
    assert_eq!(row.branch.as_deref(), Some("bare-feature"));

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

    let mut settled = None;
    for _ in 0..600 {
        let listed = client.list().await;
        if let Some(found) = listed
            .iter()
            .find(|entry| entry.subagent_id.as_ref() == Some(&view.subagent_id))
            .filter(|entry| entry.status == WorktreeStatus::Settled)
        {
            settled = Some(found.clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let entry = settled.unwrap_or_else(|| panic!("the subagent worktree must settle"));
    assert_eq!(
        entry.repo, bare_path,
        "the subagent belongs to the bare repository"
    );
    let branch = entry.branch.clone().expect("branch");

    let response = client
        .request(ClientRequest::ClearWorktrees {
            paths: Some(vec![entry.path.clone()]),
            force: false,
            bulk: false,
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
    assert!(
        worktrees
            .iter()
            .all(|worktree| worktree.subagent_id.as_ref() != Some(&view.subagent_id)),
        "{worktrees:?}"
    );
    assert!(
        worktrees
            .iter()
            .any(|worktree| worktree.path == checkout_path),
        "the parent's checkout stays listed: {worktrees:?}"
    );
    let branches = std::process::Command::new("git")
        .args(["branch", "--list", &branch])
        .current_dir(&bare)
        .output()
        .expect("list branches");
    assert!(
        String::from_utf8_lossy(&branches.stdout).trim().is_empty(),
        "clearing deletes the h4rn branch"
    );
    assert!(checkout.is_dir(), "the parent's checkout is untouched");

    let _ = shutdown.send(());
    let _ = task.await;
}
