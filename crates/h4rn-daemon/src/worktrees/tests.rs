//! Worktree tracking tests over real temporary git repositories.

use std::path::{Path, PathBuf};

use chrono::Utc;
use h4rn_protocol::{
    AgentState, HarnessKind, HostId, SubagentBudget, SubagentContext, SubagentCredential,
    SubagentId, SubagentIsolation, SubagentState, SubagentView, ThreadId, TrackedWorktree,
    WorktreeOrigin, WorktreeStatus,
};
use serde_json::json;

use super::store::{StoredAccess, StoredWorktree};
use super::{ListInput, RepoCaches, ThreadInfo, clear_entries, compute, git};

fn run(dir: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A repository with one commit, inside a directory that also has room for
/// sibling worktrees: `<tmp>/repo` and `<tmp>/<worktree>`.
struct Fixture {
    root: tempfile::TempDir,
    repo: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = git::norm(&root.path().join("repo"));
        std::fs::create_dir_all(&repo).expect("repo dir");
        run(&repo, &["init", "-q", "-b", "main"]);
        run(&repo, &["config", "user.email", "wt@h4rn.invalid"]);
        run(&repo, &["config", "user.name", "h4rn worktrees"]);
        std::fs::write(repo.join("README.md"), "# wt\n").expect("readme");
        run(&repo, &["add", "README.md"]);
        run(&repo, &["commit", "-q", "-m", "init"]);
        Self { root, repo }
    }

    fn add_worktree(&self, name: &str, branch: &str) -> PathBuf {
        let path = git::norm(&self.root.path().join(name));
        run(
            &self.repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                branch,
                &path.to_string_lossy(),
            ],
        );
        path
    }
}

fn entry(path: &Path, repo: &Path, branch: Option<&str>, owned: bool) -> TrackedWorktree {
    TrackedWorktree {
        path: path.to_string_lossy().into_owned(),
        repo: repo.to_string_lossy().into_owned(),
        branch: branch.map(str::to_owned),
        origin: if owned {
            WorktreeOrigin::Subagent
        } else {
            WorktreeOrigin::Harness
        },
        status: WorktreeStatus::Settled,
        exists: path.is_dir(),
        dirty: git::is_dirty(path),
        owned,
        subagent_id: owned.then(|| SubagentId::new("sa_test")),
        task: None,
        accessed_by: Vec::new(),
        first_seen: Utc::now(),
        last_seen: Utc::now(),
    }
}

fn subagent_view(
    id: &str,
    parent: &str,
    path: &Path,
    branch: &str,
    state: SubagentState,
) -> SubagentView {
    SubagentView {
        subagent_id: SubagentId::new(id),
        parent_thread_id: ThreadId::new(parent),
        child_thread_id: Some(ThreadId::new(format!("th_{id}"))),
        task: "do the thing".into(),
        harness: HarnessKind::Codex,
        host: HostId::new("host_local"),
        model: "m".into(),
        isolation: SubagentIsolation::Worktree,
        workspace_path: path.to_string_lossy().into_owned(),
        branch: Some(branch.into()),
        depth: 1,
        state,
        agent_state: AgentState::Idle,
        progress: Default::default(),
        budget: SubagentBudget::default(),
        context: SubagentContext::None,
        credential: SubagentCredential {
            label: "parent".into(),
            shared_with_parent: true,
        },
        created_at: Utc::now(),
        started_at: None,
        finished_at: state.is_terminal().then(Utc::now),
        result: None,
    }
}

fn input(fixture: &Fixture, views: Vec<SubagentView>) -> ListInput {
    ListInput {
        subagents: views,
        parent_workspaces: [(
            "th_parent".to_owned(),
            fixture.repo.to_string_lossy().into_owned(),
        )]
        .into_iter()
        .collect(),
        threads: Vec::new(),
        stored: Vec::new(),
        subagents_root: git::norm(&fixture.root.path().join("data/subagents")),
    }
}

#[test]
fn path_candidates_come_from_known_keys_and_command_tokens() {
    let candidates = git::path_candidates(&json!({
        "cwd": "/work/my repo",
        "file_path": "/work/a.rs",
        "command": "cd '/work/wt-1' && cargo test --manifest-path=/work/wt-2/Cargo.toml; ls relative",
        "nested": [{"filePath": "C:\\work\\wt-3\\x.rs"}],
        "count": 3,
        "note": "no paths here",
    }));
    for expected in [
        "/work/my repo",
        "/work/a.rs",
        "/work/wt-1",
        "/work/wt-2/Cargo.toml",
        "C:\\work\\wt-3\\x.rs",
    ] {
        assert!(
            candidates.iter().any(|c| c == expected),
            "missing {expected}: {candidates:?}"
        );
    }
    assert!(!candidates.iter().any(|c| c == "relative"));
    assert!(git::path_candidates(&json!({"q": "hello"})).is_empty());
    assert!(git::mentions_worktree(
        &json!({"command": "git worktree add ../x"})
    ));
    assert!(!git::mentions_worktree(&json!({"command": "ls"})));
}

#[test]
fn linked_worktrees_are_told_apart_from_main_checkouts() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-linked", "feature");
    assert!(!git::is_linked_worktree(&fixture.repo));
    assert!(git::is_linked_worktree(&linked));
    assert_eq!(git::repo_of(&linked), Some(fixture.repo.clone()));
    assert_eq!(git::repo_of(&fixture.repo), Some(fixture.repo.clone()));
    let plain = tempfile::tempdir().expect("plain dir");
    assert_eq!(git::repo_of(plain.path()), None);

    let listed = git::list_worktrees(&fixture.repo).expect("list");
    assert_eq!(listed.len(), 1, "the main worktree is excluded: {listed:?}");
    assert_eq!(listed[0].path, linked);
    assert_eq!(listed[0].branch.as_deref(), Some("feature"));
}

#[test]
fn a_turn_diff_attributes_only_new_worktrees() {
    let fixture = Fixture::new();
    let before_wt = fixture.add_worktree("wt-before", "before");
    let before = git::list_worktrees(&fixture.repo).expect("snapshot");
    let created = fixture.add_worktree("wt-during", "during");
    let after = git::list_worktrees(&fixture.repo).expect("after");
    let new = git::new_worktrees(&before, &after);
    assert_eq!(new.len(), 1);
    assert_eq!(new[0].path, created);
    assert_ne!(new[0].path, before_wt);
    assert!(git::new_worktrees(&after, &after).is_empty());
}

#[test]
fn tool_call_paths_match_the_containing_linked_worktree() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-tools", "tools");
    let caches = RepoCaches::default();
    let file = linked.join("src/new.rs");
    let hits = caches.match_candidates(
        &fixture.repo,
        &[
            file.to_string_lossy().into_owned(),
            fixture
                .repo
                .join("README.md")
                .to_string_lossy()
                .into_owned(),
        ],
        false,
    );
    assert_eq!(hits.len(), 1, "the main worktree never matches: {hits:?}");
    assert_eq!(hits[0].path, linked);
    // A prefix that only shares leading characters is not inside the worktree.
    let sibling = format!("{}-other/x", linked.display());
    assert!(
        caches
            .matching(&fixture.repo, Path::new(&sibling))
            .is_none()
    );
}

#[test]
fn subagent_worktrees_are_listed_with_their_status() {
    let fixture = Fixture::new();
    let running = fixture.add_worktree("sa-running", "h4rn/sa-running");
    let orphan = fixture.add_worktree("sa-orphan", "h4rn/sa-orphan");
    run(&fixture.repo, &["branch", "h4rn/sa-kept"]);
    let views = vec![
        subagent_view(
            "sa_running",
            "th_parent",
            &running,
            "h4rn/sa-running",
            SubagentState::Running,
        ),
        subagent_view(
            "sa_orphan",
            "th_parent",
            &orphan,
            "h4rn/sa-orphan",
            SubagentState::Failed,
        ),
        subagent_view(
            "sa_kept",
            "th_parent",
            &fixture.root.path().join("gone"),
            "h4rn/sa-kept",
            SubagentState::Completed,
        ),
        subagent_view(
            "sa_nothing",
            "th_parent",
            &fixture.root.path().join("gone2"),
            "h4rn/sa-missing",
            SubagentState::Completed,
        ),
    ];
    let (entries, gone) = compute(&input(&fixture, views));
    assert!(gone.is_empty());
    let status = |branch: &str| {
        entries
            .iter()
            .find(|entry| entry.branch.as_deref() == Some(branch))
            .map(|entry| (entry.status, entry.exists))
    };
    assert_eq!(
        status("h4rn/sa-running"),
        Some((WorktreeStatus::Active, true))
    );
    assert_eq!(
        status("h4rn/sa-orphan"),
        Some((WorktreeStatus::Orphaned, true))
    );
    assert_eq!(
        status("h4rn/sa-kept"),
        Some((WorktreeStatus::Settled, false))
    );
    assert_eq!(
        status("h4rn/sa-missing"),
        None,
        "nothing left to clear is never listed"
    );
    let order: Vec<WorktreeStatus> = entries.iter().map(|entry| entry.status).collect();
    assert_eq!(
        order,
        [
            WorktreeStatus::Active,
            WorktreeStatus::Orphaned,
            WorktreeStatus::Settled
        ]
    );
}

#[test]
fn harness_observations_merge_and_vanish_with_their_directory() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-harness", "harness-branch");
    let stored = |path: &Path| StoredWorktree {
        path: path.to_string_lossy().into_owned(),
        repo: fixture.repo.to_string_lossy().into_owned(),
        branch: Some("harness-branch".into()),
        first_seen: Utc::now(),
        last_seen: Utc::now(),
        accesses: vec![StoredAccess {
            thread_id: "th_a".into(),
            subagent_id: None,
            last_seen: Utc::now(),
        }],
    };
    let missing = fixture.root.path().join("vanished");
    let mut list_input = input(&fixture, Vec::new());
    list_input.threads = vec![ThreadInfo {
        thread_id: ThreadId::new("th_a"),
        title: "Fix it".into(),
        workspace: fixture.repo.to_string_lossy().into_owned(),
        working: true,
        last_activity: Utc::now(),
    }];
    list_input.stored = vec![stored(&linked), stored(&missing)];
    let (entries, gone) = compute(&list_input);
    assert_eq!(gone, vec![missing.to_string_lossy().into_owned()]);
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0].origin, WorktreeOrigin::Harness);
    assert_eq!(entries[0].status, WorktreeStatus::Active);
    assert_eq!(entries[0].accessed_by[0].title, "Fix it");
    assert_eq!(entries[0].dirty, Some(false));
}

#[test]
fn thread_workspaces_that_are_linked_worktrees_are_tracked() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-thread", "thread-branch");
    let mut list_input = input(&fixture, Vec::new());
    let thread = |id: &str, workspace: &Path, working: bool| ThreadInfo {
        thread_id: ThreadId::new(id),
        title: id.into(),
        workspace: workspace.to_string_lossy().into_owned(),
        working,
        last_activity: Utc::now(),
    };
    let nested = linked.join("crates").join("app");
    std::fs::create_dir_all(&nested).expect("nested workspace");
    list_input.threads = vec![
        thread("th_main", &fixture.repo, true),
        thread("th_linked", &linked, false),
        thread("th_linked2", &linked, false),
        thread("th_nested", &nested, false),
    ];
    let (entries, _) = compute(&list_input);
    assert_eq!(
        entries.len(),
        1,
        "the main checkout is not a worktree, and a subdirectory folds into its checkout: {entries:?}"
    );
    assert_eq!(entries[0].origin, WorktreeOrigin::Thread);
    assert_eq!(entries[0].status, WorktreeStatus::Settled);
    assert_eq!(entries[0].branch.as_deref(), Some("thread-branch"));
    assert_eq!(entries[0].accessed_by.len(), 3);
    assert_eq!(Path::new(&entries[0].path), git::norm(&linked));
    assert!(!entries[0].owned);

    // Clearing everything never removes a thread's workspace; naming it does.
    let data = fixture.root.path().join("data");
    let (removed, skipped) = clear_entries(&entries, None, false, &data);
    assert!(removed.is_empty() && skipped.is_empty());
    assert!(linked.is_dir());
    let path = entries[0].path.clone();
    let (removed, skipped) =
        clear_entries(&entries, Some(std::slice::from_ref(&path)), false, &data);
    assert_eq!(removed, vec![path], "{skipped:?}");
    assert!(!linked.exists());
}

#[test]
fn clear_skips_dirty_worktrees_without_force_and_removes_them_with_it() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-dirty", "dirty-branch");
    std::fs::write(linked.join("scratch.txt"), "wip").expect("dirty file");
    let entries = vec![entry(&linked, &fixture.repo, Some("dirty-branch"), false)];
    assert_eq!(entries[0].dirty, Some(true));
    let data = fixture.root.path().join("data");
    let path = entries[0].path.clone();

    let (removed, skipped) =
        clear_entries(&entries, Some(std::slice::from_ref(&path)), false, &data);
    assert!(removed.is_empty());
    assert_eq!(skipped[0].reason, "has uncommitted changes");
    assert!(linked.is_dir());

    let (removed, skipped) =
        clear_entries(&entries, Some(std::slice::from_ref(&path)), true, &data);
    assert_eq!(removed, vec![path]);
    assert!(skipped.is_empty());
    assert!(!linked.exists());
    assert!(
        git::branch_exists(&fixture.repo, "dirty-branch"),
        "a branch h4rn did not create is never deleted"
    );
}

#[test]
fn clear_refuses_active_worktrees_even_with_force() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-active", "active-branch");
    let mut active = entry(&linked, &fixture.repo, Some("active-branch"), false);
    active.status = WorktreeStatus::Active;
    active.accessed_by.push(h4rn_protocol::WorktreeAccess {
        thread_id: ThreadId::new("th_busy"),
        title: "Busy thread".into(),
        subagent_id: None,
        working: true,
        last_seen: Utc::now(),
    });
    let entries = vec![active];
    let data = fixture.root.path().join("data");
    let path = entries[0].path.clone();

    let (removed, skipped) = clear_entries(&entries, Some(&[path]), true, &data);
    assert!(removed.is_empty());
    assert_eq!(skipped[0].reason, "in use by Busy thread");
    assert!(linked.is_dir());

    // Clearing everything leaves active entries alone without reporting them.
    let (removed, skipped) = clear_entries(&entries, None, true, &data);
    assert!(removed.is_empty() && skipped.is_empty());
    assert!(linked.is_dir());

    let (_, skipped) = clear_entries(&entries, Some(&["/nowhere".into()]), false, &data);
    assert_eq!(skipped[0].reason, "not tracked");
}

#[test]
fn clear_deletes_only_h4rn_branches_for_owned_entries() {
    let fixture = Fixture::new();
    let data = fixture.root.path().join("data");
    let h4rn = fixture.add_worktree("sa-own", "h4rn/sa-own");
    let other = fixture.add_worktree("sa-foreign", "feature/foreign");
    let mut own = entry(&h4rn, &fixture.repo, Some("h4rn/sa-own"), true);
    own.subagent_id = Some(SubagentId::new("sa_own"));
    let mut foreign = entry(&other, &fixture.repo, Some("feature/foreign"), true);
    foreign.subagent_id = Some(SubagentId::new("sa_foreign"));
    let entries = vec![own, foreign];

    let (removed, skipped) = clear_entries(&entries, None, false, &data);
    assert_eq!(removed.len(), 2, "{skipped:?}");
    assert!(!h4rn.exists() && !other.exists());
    assert!(!git::branch_exists(&fixture.repo, "h4rn/sa-own"));
    assert!(
        git::branch_exists(&fixture.repo, "feature/foreign"),
        "only branches under h4rn/ are deleted"
    );
}

#[test]
fn clear_skips_unmerged_owned_branches_without_force() {
    let fixture = Fixture::new();
    let data = fixture.root.path().join("data");
    let linked = fixture.add_worktree("sa-unmerged", "h4rn/sa-unmerged");
    std::fs::write(linked.join("work.txt"), "work").expect("work file");
    run(&linked, &["add", "work.txt"]);
    run(&linked, &["commit", "-q", "-m", "child work"]);
    let entries = vec![entry(
        &linked,
        &fixture.repo,
        Some("h4rn/sa-unmerged"),
        true,
    )];
    assert_eq!(entries[0].dirty, Some(false));

    let (removed, skipped) = clear_entries(&entries, None, false, &data);
    assert!(removed.is_empty());
    assert_eq!(
        skipped[0].reason,
        "branch h4rn/sa-unmerged has unmerged commits"
    );
    assert!(linked.is_dir());
    assert!(git::branch_exists(&fixture.repo, "h4rn/sa-unmerged"));

    let (removed, _) = clear_entries(&entries, None, true, &data);
    assert_eq!(removed.len(), 1);
    assert!(!linked.exists());
    assert!(!git::branch_exists(&fixture.repo, "h4rn/sa-unmerged"));
}

#[test]
fn clear_never_touches_the_main_worktree() {
    let fixture = Fixture::new();
    let data = fixture.root.path().join("data");
    let entries = vec![entry(&fixture.repo, &fixture.repo, Some("main"), false)];
    let (removed, skipped) = clear_entries(&entries, None, true, &data);
    assert!(removed.is_empty());
    assert_eq!(skipped[0].reason, "is the repository's main worktree");
    assert!(fixture.repo.join("README.md").exists());
}
