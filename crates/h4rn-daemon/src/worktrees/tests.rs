//! Worktree tracking tests over real temporary git repositories.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use h4rn_harness::{FakeHarness, Harness, HarnessEvent};
use h4rn_protocol::{
    AgentState, HarnessKind, HostId, RuntimeMode, SubagentBudget, SubagentContext,
    SubagentCredential, SubagentId, SubagentIsolation, SubagentState, SubagentView, ThreadId,
    ThreadTab, ToolCall, ToolCallStatus, TrackedWorktree, WorktreeOrigin, WorktreeStatus,
};
use serde_json::json;

use super::store::{StoredAccess, StoredWorktree};
use super::{
    ClearOne, ListInput, RepoCaches, ThreadInfo, WorktreeTracker, compute, git, is_busy,
    open_snapshot, preflight, remove, run_clear_entry, select_entries,
};
use crate::harnesses::StaticHarnessCatalog;
use crate::journal::SqliteJournal;
use crate::subagents::{SubagentManager, SubagentOptions};
use crate::threads::{NewThread, ThreadManager, ThreadManagerOptions};

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

    fn data_dir(&self) -> PathBuf {
        self.root.path().join("data")
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

/// The two blocking phases of a clear, for the unit tests below.
fn clear_one(entry: &TrackedWorktree, force: bool, data_dir: &Path) -> Result<ClearOne, String> {
    preflight(entry, force)?;
    remove(entry, force, data_dir)
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
        subagent_repos: HashMap::new(),
        threads: Vec::new(),
        stored: Vec::new(),
        subagents_root: git::norm(&fixture.root.path().join("data/subagents")),
    }
}

/// The daemon's real managers over a test repository, with a tracker in front.
async fn managers(
    fixture: &Fixture,
    harnesses: Vec<Arc<dyn Harness>>,
) -> (WorktreeTracker, Arc<ThreadManager>, Arc<SubagentManager>) {
    let mut catalog = StaticHarnessCatalog::new();
    for harness in harnesses {
        catalog.register(harness);
    }
    let catalog: Arc<dyn crate::harnesses::HarnessCatalog> = Arc::new(catalog);
    let data = fixture.data_dir();
    std::fs::create_dir_all(&data).expect("data dir");
    let journal = Arc::new(SqliteJournal::in_memory().await.expect("journal"));
    let threads = Arc::new(
        ThreadManager::open(
            journal,
            Arc::clone(&catalog),
            HostId::new("host_local"),
            ThreadManagerOptions::default(),
            None,
        )
        .await
        .expect("thread manager"),
    );
    let subagents = Arc::new(
        SubagentManager::in_memory(
            Arc::clone(&threads),
            catalog,
            data.clone(),
            SubagentOptions::default(),
        )
        .await
        .expect("subagent manager"),
    );
    subagents.start();
    let tracker = WorktreeTracker::in_memory(Arc::clone(&threads), Arc::clone(&subagents), data)
        .await
        .expect("tracker");
    (tracker, threads, subagents)
}

async fn start_thread(threads: &ThreadManager, workspace: &Path, prompt: &str) -> ThreadId {
    threads
        .create_thread(NewThread {
            thread_id: None,
            parent_thread_id: None,
            subagent_id: None,
            host: HostId::new("host_local"),
            harness: HarnessKind::ClaudeCode,
            workspace_path: workspace.to_string_lossy().into_owned(),
            model: "default".into(),
            prompt: prompt.to_owned(),
            attachments: Vec::new(),
            runtime_mode: RuntimeMode::ApprovalRequired,
            worktree: None,
        })
        .await
        .expect("create thread")
        .thread_id
}

/// Waits until the thread reports busy and returns its title.
async fn wait_busy(threads: &ThreadManager, thread: &ThreadId) -> String {
    for _ in 0..250 {
        if let Some(summary) = threads.thread_summary(thread).await
            && is_busy(
                summary.tab,
                summary.agent_state,
                threads.turn_in_flight(thread).await,
            )
        {
            return summary.title;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("thread {thread:?} never became busy");
}

/// Waits until the subagent's worktree reports settled.
async fn wait_settled(tracker: &WorktreeTracker, subagent: &SubagentId) -> TrackedWorktree {
    for _ in 0..500 {
        let listed = tracker.list().await;
        if let Some(entry) = listed
            .iter()
            .find(|entry| entry.subagent_id.as_ref() == Some(subagent))
            && entry.status == WorktreeStatus::Settled
        {
            return entry.clone();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("subagent {subagent:?} never settled");
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
    let (entries, gone) = compute(&input(&fixture, views), true);
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
    let (entries, gone) = compute(&list_input, true);
    assert_eq!(gone, vec![missing.to_string_lossy().into_owned()]);
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0].origin, WorktreeOrigin::Harness);
    assert_eq!(entries[0].status, WorktreeStatus::Active);
    assert_eq!(entries[0].accessed_by[0].title, "Fix it");
    assert_eq!(entries[0].dirty, Some(false));

    // The selection listing skips `git status` for every entry: clearing does
    // its own last-moment check.
    let (entries, _) = compute(&list_input, false);
    assert_eq!(entries[0].dirty, None);
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
    let (entries, _) = compute(&list_input, true);
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
    let data = fixture.data_dir();
    let selection = select_entries(&entries, None);
    assert!(selection.targets.is_empty() && selection.skipped.is_empty());
    assert!(linked.is_dir());
    let path = entries[0].path.clone();
    let selection = select_entries(&entries, Some(std::slice::from_ref(&path)));
    assert_eq!(selection.targets.len(), 1);
    assert!(
        matches!(
            clear_one(selection.targets[0], false, &data),
            Ok(ClearOne::Removed)
        ),
        "the named thread workspace is removed"
    );
    assert!(!linked.exists());
}

#[test]
fn selecting_entries_keeps_unknown_and_active_paths_apart() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-select", "select-branch");
    let mut active = entry(&linked, &fixture.repo, Some("select-branch"), false);
    active.status = WorktreeStatus::Active;
    let entries = vec![active];

    // Clearing everything leaves active entries alone without reporting them.
    let selection = select_entries(&entries, None);
    assert!(selection.targets.is_empty() && selection.skipped.is_empty());

    // An explicit path is selected; whether it can go is the re-check's call.
    let selection = select_entries(&entries, Some(std::slice::from_ref(&entries[0].path)));
    assert_eq!(selection.targets.len(), 1);

    let selection = select_entries(&entries, Some(&["/nowhere".into()]));
    assert_eq!(selection.skipped[0].reason, "not tracked");
    assert_eq!(selection.skipped[0].path, "/nowhere");
}

#[test]
fn clear_skips_dirty_worktrees_without_force_and_removes_them_with_it() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-dirty", "dirty-branch");
    std::fs::write(linked.join("scratch.txt"), "wip").expect("dirty file");
    let entries = [entry(&linked, &fixture.repo, Some("dirty-branch"), false)];
    assert_eq!(entries[0].dirty, Some(true));
    let data = fixture.data_dir();

    assert_eq!(
        clear_one(&entries[0], false, &data).err().as_deref(),
        Some("has uncommitted changes")
    );
    assert!(linked.is_dir());

    assert!(matches!(
        clear_one(&entries[0], true, &data),
        Ok(ClearOne::Removed)
    ));
    assert!(!linked.exists());
    assert!(
        git::branch_exists(&fixture.repo, "dirty-branch"),
        "a branch h4rn did not create is never deleted"
    );
}

#[test]
fn clear_deletes_only_h4rn_branches_for_owned_entries() {
    let fixture = Fixture::new();
    let data = fixture.data_dir();
    let h4rn = fixture.add_worktree("sa-own", "h4rn/sa-own");
    let other = fixture.add_worktree("sa-foreign", "feature/foreign");
    let mut own = entry(&h4rn, &fixture.repo, Some("h4rn/sa-own"), true);
    own.subagent_id = Some(SubagentId::new("sa_own"));
    let mut foreign = entry(&other, &fixture.repo, Some("feature/foreign"), true);
    foreign.subagent_id = Some(SubagentId::new("sa_foreign"));

    // Clean owned checkouts are removed without `--force` too: master is not
    // needed to delete them.
    assert!(matches!(
        clear_one(&own, false, &data),
        Ok(ClearOne::Removed)
    ));
    assert!(matches!(
        clear_one(&foreign, false, &data),
        Ok(ClearOne::Removed)
    ));
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
    let data = fixture.data_dir();
    let linked = fixture.add_worktree("sa-unmerged", "h4rn/sa-unmerged");
    std::fs::write(linked.join("work.txt"), "work").expect("work file");
    run(&linked, &["add", "work.txt"]);
    run(&linked, &["commit", "-q", "-m", "child work"]);
    let entries = [entry(
        &linked,
        &fixture.repo,
        Some("h4rn/sa-unmerged"),
        true,
    )];
    assert_eq!(entries[0].dirty, Some(false));

    assert_eq!(
        clear_one(&entries[0], false, &data).err().as_deref(),
        Some("branch h4rn/sa-unmerged has unmerged commits")
    );
    assert!(linked.is_dir());
    assert!(git::branch_exists(&fixture.repo, "h4rn/sa-unmerged"));

    assert!(matches!(
        clear_one(&entries[0], true, &data),
        Ok(ClearOne::Removed)
    ));
    assert!(!linked.exists());
    assert!(!git::branch_exists(&fixture.repo, "h4rn/sa-unmerged"));
}

#[test]
fn clear_never_touches_the_main_worktree() {
    let fixture = Fixture::new();
    let data = fixture.data_dir();
    let entries = [entry(&fixture.repo, &fixture.repo, Some("main"), false)];
    assert_eq!(
        clear_one(&entries[0], true, &data).err().as_deref(),
        Some("is the repository's main worktree")
    );
    assert!(fixture.repo.join("README.md").exists());
}

#[test]
fn a_dirty_worktree_is_skipped_even_when_status_hides_untracked_files() {
    let fixture = Fixture::new();
    // The user asked `git status` to hide untracked files. Clearing must not
    // believe that: this is the case that lost data in the first round.
    run(
        &fixture.repo,
        &["config", "status.showUntrackedFiles", "no"],
    );
    let linked = fixture.add_worktree("sa-untracked", "h4rn/sa-untracked");
    std::fs::write(linked.join("notes.md"), "wip").expect("untracked file");
    assert_eq!(
        git::is_dirty(&linked),
        Some(true),
        "untracked files always count"
    );

    // A listing taken before the file appeared says clean. The last-moment
    // re-check must catch it anyway.
    let mut stale = entry(&linked, &fixture.repo, Some("h4rn/sa-untracked"), true);
    stale.dirty = Some(false);
    let data = fixture.data_dir();
    assert_eq!(
        clear_one(&stale, false, &data).err().as_deref(),
        Some("has uncommitted changes")
    );
    assert!(linked.is_dir());

    assert!(matches!(
        clear_one(&stale, true, &data),
        Ok(ClearOne::Removed)
    ));
    assert!(!linked.exists());
}

#[test]
fn a_detached_commit_on_no_branch_is_kept_without_force() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-detached", "detached-base");
    run(&linked, &["checkout", "-q", "--detach"]);
    std::fs::write(linked.join("work.txt"), "work").expect("work file");
    run(&linked, &["add", "work.txt"]);
    run(&linked, &["commit", "-q", "-m", "detached work"]);
    let data = fixture.data_dir();
    let detached = entry(&linked, &fixture.repo, None, false);
    assert_eq!(
        clear_one(&detached, false, &data).err().as_deref(),
        Some("detached HEAD has commits on no branch")
    );
    assert!(linked.is_dir(), "the checkout is kept");

    // The same commit is now reachable from a branch, so the checkout can go.
    let sha = git::run(&linked, ["rev-parse", "HEAD"]).expect("head");
    run(&fixture.repo, &["branch", "keep-detached", &sha]);
    assert!(matches!(
        clear_one(&detached, false, &data),
        Ok(ClearOne::Removed)
    ));
    assert!(!linked.exists());
    assert!(git::branch_exists(&fixture.repo, "keep-detached"));
}

#[test]
fn a_branch_that_cannot_be_deleted_is_reported_after_removing_the_checkout() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("sa-branchfail", "h4rn/sa-branchfail");
    // Free the branch, then check it out in a second worktree: git refuses to
    // delete a branch another worktree has checked out.
    run(&linked, &["checkout", "-q", "--detach"]);
    let elsewhere = git::norm(&fixture.root.path().join("sa-elsewhere"));
    run(
        &fixture.repo,
        &[
            "worktree",
            "add",
            "-q",
            &elsewhere.to_string_lossy(),
            "h4rn/sa-branchfail",
        ],
    );
    let mut own = entry(&linked, &fixture.repo, Some("h4rn/sa-branchfail"), true);
    own.dirty = Some(false);
    let data = fixture.data_dir();
    let outcome = clear_one(&own, false, &data);
    assert!(
        !matches!(outcome, Ok(ClearOne::Removed)),
        "a branch failure is not a clean removal"
    );
    assert!(!linked.exists(), "the checkout itself was removed");
    let Ok(ClearOne::RemovedWithBranchFailure(reason)) = outcome else {
        panic!("unexpected outcome");
    };
    assert!(
        reason.starts_with("checkout removed, but branch h4rn/sa-branchfail could not be deleted:"),
        "{reason}"
    );
    assert!(git::branch_exists(&fixture.repo, "h4rn/sa-branchfail"));
}

#[test]
fn a_git_call_that_outlives_its_deadline_is_killed() {
    let fixture = Fixture::new();
    // A git alias runs a shell command; this one sleeps past the deadline.
    let started = std::time::Instant::now();
    let error = git::run_with_timeout(
        &fixture.repo,
        ["-c", "alias.hang=!sleep 5", "hang"],
        Duration::from_secs(1),
    )
    .expect_err("the call must time out");
    assert!(git::timed_out(&error), "{error}");
    assert_eq!(error, "git timed out after 1s");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "the child is killed at the deadline, not when the command finishes"
    );
}

#[test]
fn a_tag_does_not_hide_a_branch_with_the_same_name() {
    let fixture = Fixture::new();
    run(&fixture.repo, &["branch", "h4rn/sa-x"]);
    run(&fixture.repo, &["tag", "h4rn/sa-x"]);
    let branches = git::h4rn_branches(&fixture.repo);
    assert!(
        branches.contains("h4rn/sa-x"),
        "the branch is found alongside its tag: {branches:?}"
    );
    assert_eq!(
        branches.iter().filter(|name| *name == "h4rn/sa-x").count(),
        1
    );
}

#[test]
fn a_child_of_a_removed_subagent_worktree_keeps_its_repo() {
    let fixture = Fixture::new();
    run(&fixture.repo, &["branch", "h4rn/sa-parent"]);
    run(&fixture.repo, &["branch", "h4rn/sa-child"]);
    let root = git::norm(&fixture.root.path().join("data/subagents"));
    let parent_worktree = root.join("sa_parent").join("worktree");
    let child_worktree = root.join("sa_child").join("worktree");
    let child = subagent_view(
        "sa_child",
        "th_sa_parent",
        &child_worktree,
        "h4rn/sa-child",
        SubagentState::Completed,
    );
    let mut list_input = input(&fixture, vec![child]);
    // The parent subagent's workspace is gone; only its recorded repository
    // (and the map from the child's own record) knows where the branches are.
    list_input.parent_workspaces = [(
        "th_sa_parent".to_owned(),
        parent_worktree.to_string_lossy().into_owned(),
    )]
    .into_iter()
    .collect();
    list_input.subagent_repos = [
        ("sa_child".to_owned(), parent_worktree.clone()),
        ("sa_parent".to_owned(), fixture.repo.clone()),
    ]
    .into_iter()
    .collect();
    let branches = git::h4rn_branches(&fixture.repo);
    assert!(branches.contains("h4rn/sa-parent") && branches.contains("h4rn/sa-child"));

    let (entries, _) = compute(&list_input, true);
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0].branch.as_deref(), Some("h4rn/sa-child"));
    assert_eq!(Path::new(&entries[0].repo), fixture.repo.as_path());
    assert_eq!(entries[0].status, WorktreeStatus::Settled);
}

#[test]
fn overlapping_turns_do_not_attribute_each_other_s_worktrees() {
    let mut snapshots: HashMap<String, super::Snapshot> = HashMap::new();
    let repo = Path::new("/repo");
    assert!(!open_snapshot(&mut snapshots, "th_a", repo, Vec::new()));
    assert!(!open_snapshot(
        &mut snapshots,
        "th_b",
        Path::new("/other"),
        Vec::new()
    ));
    assert!(
        !snapshots["th_a"].shared && !snapshots["th_b"].shared,
        "different repositories do not overlap"
    );

    assert!(
        open_snapshot(&mut snapshots, "th_c", repo, Vec::new()),
        "a second turn on the same repository overlaps the first"
    );
    assert!(snapshots["th_a"].shared && snapshots["th_c"].shared);
    assert!(!snapshots["th_b"].shared);
    assert!(!super::diff_attributable(&snapshots["th_a"]));
    assert!(!super::diff_attributable(&snapshots["th_c"]));
    assert!(super::diff_attributable(&snapshots["th_b"]));

    // Once both overlapping turns end, the next one is unambiguous again.
    snapshots.remove("th_a");
    snapshots.remove("th_c");
    assert!(!open_snapshot(&mut snapshots, "th_d", repo, Vec::new()));
    assert!(super::diff_attributable(&snapshots["th_d"]));
}

#[test]
fn a_thread_is_busy_only_when_it_can_write() {
    assert!(
        is_busy(ThreadTab::Working, AgentState::Idle, false),
        "a pinned or never-run thread in the Working tab can still write"
    );
    for state in [
        AgentState::Queued,
        AgentState::Scheduled,
        AgentState::Thinking,
        AgentState::Searching,
        AgentState::Reading,
        AgentState::Editing,
        AgentState::RunningTool,
        AgentState::AwaitingApproval,
        AgentState::RateLimited,
    ] {
        assert!(is_busy(ThreadTab::Settled, state, false), "{state:?}");
    }
    for state in [AgentState::Done, AgentState::Error, AgentState::Unreachable] {
        assert!(!is_busy(ThreadTab::Settled, state, false), "{state:?}");
        assert!(
            !is_busy(ThreadTab::Working, state, false),
            "the Working-tab exit grace is not busy: {state:?}"
        );
        assert!(
            is_busy(ThreadTab::Settled, state, true),
            "an in-flight turn is busy whatever the agent state says: {state:?}"
        );
    }
    assert!(
        !is_busy(ThreadTab::Settled, AgentState::Idle, false),
        "an idle thread outside the Working tab is not busy"
    );
}

#[tokio::test]
async fn clear_through_the_tracker_skips_a_busy_thread_and_keeps_the_checkout() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-busy", "busy-branch");
    // A script that never completes its turn keeps the thread busy while the
    // clear runs, without a wall-clock sleep.
    let (tracker, threads, _subagents) = managers(
        &fixture,
        vec![Arc::new(FakeHarness::with_events(Vec::new()))],
    )
    .await;
    let thread = start_thread(&threads, &linked, "work in the checkout").await;
    let title = wait_busy(&threads, &thread).await;

    let listed = tracker.list().await;
    let entry = listed
        .iter()
        .find(|entry| Path::new(&entry.path) == git::norm(&linked))
        .unwrap_or_else(|| panic!("the thread workspace is listed: {listed:?}"));
    assert_eq!(entry.status, WorktreeStatus::Active);

    let outcome = tracker.clear(Some(vec![entry.path.clone()]), false).await;
    assert!(outcome.removed.is_empty(), "{:?}", outcome.skipped);
    assert_eq!(outcome.skipped[0].reason, format!("in use by {title}"));
    assert!(linked.is_dir(), "the checkout is kept");

    let _ = threads.interrupt(&thread).await;
}

#[tokio::test]
async fn clear_through_the_tracker_sees_a_thread_the_listing_missed() {
    let fixture = Fixture::new();
    let (tracker, threads, subagents) = managers(
        &fixture,
        vec![
            Arc::new(FakeHarness::with_events(Vec::new())),
            Arc::new(FakeHarness::new().with_kind(HarnessKind::Codex)),
        ],
    )
    .await;
    let parent = start_thread(&threads, &fixture.repo, "parent").await;
    let view = subagents
        .spawn(h4rn_protocol::SpawnSubagentRequest {
            parent_thread: parent.clone(),
            task: "finish the thing".into(),
            harness: Some(HarnessKind::Codex),
            host: None,
            model: None,
            isolation: SubagentIsolation::Worktree,
            context: Default::default(),
            budget: Default::default(),
        })
        .await
        .expect("spawn subagent");
    let settled = wait_settled(&tracker, &view.subagent_id).await;
    assert!(!settled.exists, "the settled directory was removed");

    // Put the checkout back, the way a daemon restart leaves an orphan behind.
    let path = PathBuf::from(&settled.path);
    let branch = settled.branch.clone().expect("kept branch");
    run(
        &fixture.repo,
        &["worktree", "add", "-q", &path.to_string_lossy(), &branch],
    );
    assert!(path.is_dir());

    // A thread that works inside it was not part of the listing's accessors
    // (and thread workspaces under h4rn's data dir are never listed on their
    // own), so only the live re-check can see it.
    let nested = path.join("src");
    std::fs::create_dir_all(&nested).expect("nested workspace");
    let busy = start_thread(&threads, &nested, "keep working in here").await;
    let title = wait_busy(&threads, &busy).await;

    let listed = tracker.list().await;
    let entry = listed
        .iter()
        .find(|entry| Path::new(&entry.path) == path)
        .unwrap_or_else(|| panic!("the orphaned worktree is listed: {listed:?}"));
    assert_ne!(
        entry.status,
        WorktreeStatus::Active,
        "the listing only knows the finished subagent"
    );

    let outcome = tracker.clear(Some(vec![entry.path.clone()]), false).await;
    assert!(outcome.removed.is_empty(), "{:?}", outcome.skipped);
    assert_eq!(outcome.skipped[0].reason, format!("in use by {title}"));
    assert!(path.is_dir(), "the checkout is kept");

    let _ = threads.interrupt(&busy).await;
    let _ = threads.interrupt(&parent).await;
}

/// A harness whose first tool call fails and whose turn never completes, so
/// the thread sits at `AgentState::Error` with its turn still in flight.
fn failing_tool_call() -> FakeHarness {
    FakeHarness::with_events(vec![HarnessEvent::ToolCall {
        tool_call: ToolCall {
            tool_call_id: "tool-error".into(),
            name: "Bash".into(),
            status: ToolCallStatus::Failed,
            summary: "exit 1".into(),
            input: None,
            output: None,
            started_at: Utc::now(),
            completed_at: Some(Utc::now()),
            stats: None,
        },
    }])
}

#[tokio::test]
async fn a_failed_tool_call_during_a_turn_keeps_the_worktree_busy() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-error", "error-branch");
    let (tracker, threads, _subagents) =
        managers(&fixture, vec![Arc::new(failing_tool_call())]).await;
    let thread = start_thread(&threads, &linked, "hit the error").await;

    let mut title = None;
    for _ in 0..250 {
        if let Some(summary) = threads.thread_summary(&thread).await
            && summary.agent_state == AgentState::Error
        {
            // A failed tool call does not end the turn: the state is Error but
            // the thread is still able to write.
            assert!(
                threads.turn_in_flight(&thread).await,
                "the turn is still in flight"
            );
            title = Some(summary.title);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let title = title.expect("the thread reached Error while its turn ran");

    let listed = tracker.list().await;
    let entry = listed
        .iter()
        .find(|entry| Path::new(&entry.path) == git::norm(&linked))
        .unwrap_or_else(|| panic!("the thread workspace is listed: {listed:?}"));
    assert_eq!(
        entry.status,
        WorktreeStatus::Active,
        "a turn in flight keeps the entry active even at agent state Error"
    );

    let outcome = tracker.clear(Some(vec![entry.path.clone()]), false).await;
    assert!(outcome.removed.is_empty(), "{:?}", outcome.skipped);
    assert_eq!(outcome.skipped[0].reason, format!("in use by {title}"));
    assert!(linked.is_dir(), "the checkout is kept");

    let _ = threads.interrupt(&thread).await;
}

#[tokio::test]
async fn a_clear_rechecks_between_preflight_and_removal_and_keeps_the_checkout() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-late", "late-branch");
    let (tracker, threads, _subagents) = managers(
        &fixture,
        vec![Arc::new(FakeHarness::with_events(Vec::new()))],
    )
    .await;

    let entry = entry(&linked, &fixture.repo, Some("late-branch"), false);
    assert_eq!(entry.status, WorktreeStatus::Settled, "nothing uses it yet");

    // Once preflight has passed, a thread starts working in the checkout. The
    // live re-check, which runs before removal, must still catch it.
    let started: Arc<std::sync::Mutex<Option<(ThreadId, String)>>> =
        Arc::new(std::sync::Mutex::new(None));
    let hook = {
        let started = Arc::clone(&started);
        let threads = Arc::clone(&threads);
        let path = linked.clone();
        move |_path: String| {
            let started = Arc::clone(&started);
            let threads = Arc::clone(&threads);
            let path = path.clone();
            async move {
                let thread = start_thread(&threads, &path, "late user").await;
                let title = wait_busy(&threads, &thread).await;
                *started.lock().expect("lock") = Some((thread, title));
            }
        }
    };
    let outcome = run_clear_entry(&tracker.inner, entry, false, hook).await;

    let Err(reason) = outcome else {
        panic!("the entry became busy before removal");
    };
    let (thread, title) = started
        .lock()
        .expect("lock")
        .clone()
        .expect("the hook started a thread");
    assert_eq!(reason, format!("in use by {title}"));
    assert!(linked.is_dir(), "the checkout is kept");

    let _ = threads.interrupt(&thread).await;
}

#[test]
fn output_collection_is_bounded_when_a_descendant_holds_the_pipes() {
    let fixture = Fixture::new();
    // git exits at once, but the background sleep it started inherits stdout
    // and stderr, so the readers would block for as long as it lives.
    let started = std::time::Instant::now();
    let error = git::run_with_timeout(
        &fixture.repo,
        ["-c", "alias.bg=!sh -c \"sleep 5 &\"", "bg"],
        Duration::from_secs(1),
    )
    .expect_err("the output deadline must expire");
    assert_eq!(error, "git timed out after 1s");
    assert!(git::timed_out(&error));
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "bounded by the deadline plus the one-second grace: {:?}",
        started.elapsed()
    );
}

#[test]
fn git_refusing_a_dirty_checkout_reads_as_uncommitted_changes() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-refuse", "refuse-branch");
    std::fs::write(linked.join("scratch.txt"), "wip").expect("dirty file");
    let entries = [entry(&linked, &fixture.repo, Some("refuse-branch"), false)];
    // Preflight would catch this first; removal is called directly to prove
    // git's own refusal is reported in our words, not git's.
    assert_eq!(
        remove(&entries[0], false, &fixture.data_dir())
            .err()
            .as_deref(),
        Some("has uncommitted changes")
    );
    assert!(linked.is_dir(), "git's refusal keeps the checkout");
}
