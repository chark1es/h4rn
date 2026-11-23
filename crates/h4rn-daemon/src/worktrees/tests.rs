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
    ThreadTab, ToolCall, ToolCallStatus, TrackedWorktree, WorktreeAccess, WorktreeOrigin,
    WorktreeStatus,
};
use serde_json::json;

use super::store::{StoredAccess, StoredWorktree};
use super::{
    BEFORE_UPDATE_REF, ClearOne, ListInput, LiveUse, NESTED_SCAN_LIMIT, NESTED_SCAN_UNKNOWN,
    RECREATED, RepoCaches, ThreadInfo, ThreadWorkspace, WorktreeTracker, branch_tip, compute,
    compute_listing, delete_owned_branch, execute, git, is_busy, nested_checkout, open_snapshot,
    preflight, prepare, run_clear_entry, select_entries,
};
use crate::harnesses::StaticHarnessCatalog;
use crate::journal::{Journal, SqliteJournal};
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
    let commons = git::CommonDirs::default();
    let plan = preflight(entry, force, &commons)?;
    let prepared = prepare(entry, force, &commons)?;
    execute(entry, force, data_dir, &plan, &prepared, &commons)
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
            agent_id: None,
            workspace_path: workspace.to_string_lossy().into_owned(),
            model: "default".into(),
            prompt: prompt.to_owned(),
            attachments: Vec::new(),
            runtime_mode: RuntimeMode::ApprovalRequired,
            reasoning_effort: None,
            fast: false,
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

#[cfg(unix)]
#[test]
fn normalisation_resolves_symlinks_before_folding_parent_components() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-link", "link-branch");
    let subdir = linked.join("subdir");
    std::fs::create_dir_all(&subdir).expect("subdir");
    let link = fixture.root.path().join("link");
    std::os::unix::fs::symlink(&subdir, &link).expect("symlink");

    // The filesystem resolves `link` first and `..` then takes the checkout's
    // parent: `link/..` is the checkout, not the symlink's own directory.
    assert_eq!(git::norm(&link.join("..")), linked);
    assert_eq!(git::norm(&fixture.root.path().join("link/..")), linked);
    // A missing suffix still folds lexically below the existing ancestor.
    assert_eq!(
        git::norm(&linked.join("missing/../next")),
        linked.join("next")
    );
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
    // its own dirty check in preflight.
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

    // Clearing everything never removes a thread's workspace, and says so;
    // naming it does.
    let data = fixture.data_dir();
    let selection = select_entries(&entries, None, false, &[]);
    assert!(selection.targets.is_empty());
    assert_eq!(selection.skipped.len(), 1);
    assert_eq!(selection.skipped[0].reason, "is a thread workspace");
    assert!(linked.is_dir());
    let path = entries[0].path.clone();
    let selection = select_entries(&entries, Some(std::slice::from_ref(&path)), false, &[]);
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
    let selection = select_entries(&entries, None, false, &[]);
    assert!(selection.targets.is_empty() && selection.skipped.is_empty());

    // An explicit path is selected; whether it can go is the re-check's call.
    let selection = select_entries(
        &entries,
        Some(std::slice::from_ref(&entries[0].path)),
        false,
        &[],
    );
    assert_eq!(selection.targets.len(), 1);

    let selection = select_entries(&entries, Some(&["/nowhere".into()]), false, &[]);
    assert_eq!(selection.skipped[0].reason, "not tracked");
    assert_eq!(selection.skipped[0].path, "/nowhere");

    // Requests match listed paths exactly, up to a trailing separator; a
    // spelling that only normalises to the listed path does not.
    let path = &entries[0].path;
    let selection = select_entries(&entries, Some(&[format!("{path}/")]), false, &[]);
    assert_eq!(selection.targets.len(), 1);
    let name = linked.file_name().expect("name").to_string_lossy();
    let spelled = format!("{path}/../{name}");
    let selection = select_entries(&entries, Some(std::slice::from_ref(&spelled)), false, &[]);
    assert!(selection.targets.is_empty());
    assert_eq!(selection.skipped[0].reason, "not tracked");
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
fn a_symbolic_owned_branch_is_left_in_place_with_its_target() {
    let fixture = Fixture::new();
    run(&fixture.repo, &["branch", "release"]);
    run(
        &fixture.repo,
        &[
            "symbolic-ref",
            "refs/heads/h4rn/sa-sym",
            "refs/heads/release",
        ],
    );
    assert_eq!(
        git::run(&fixture.repo, ["symbolic-ref", "refs/heads/h4rn/sa-sym"]).expect("symref"),
        "refs/heads/release"
    );
    let missing = fixture.root.path().join("sa-sym-gone");
    let mut own = entry(&missing, &fixture.repo, Some("h4rn/sa-sym"), true);
    own.dirty = None;

    let outcome = clear_one(&own, false, &fixture.data_dir());
    assert!(
        matches!(
            &outcome,
            Ok(ClearOne::RemovedWithBranchFailure(reason))
                if reason.as_str() == "branch h4rn/sa-sym is a symbolic ref"
        ),
        "{outcome:?}"
    );
    assert!(
        git::branch_exists(&fixture.repo, "release"),
        "the symbolic ref's target survives"
    );
    assert_eq!(
        git::run(&fixture.repo, ["symbolic-ref", "refs/heads/h4rn/sa-sym"]).expect("symref"),
        "refs/heads/release",
        "the symbolic ref itself is kept and reported"
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

#[tokio::test]
async fn a_branch_that_gains_unmerged_work_after_preflight_is_kept_without_force() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("sa-moved", "h4rn/sa-moved");
    let (tracker, _threads, _subagents) = managers(&fixture, Vec::new()).await;
    let target = entry(&linked, &fixture.repo, Some("h4rn/sa-moved"), true);

    // The branch gains a commit between the merge check and removal, so the
    // branch is no longer merged when git is asked to delete it.
    let hook = {
        let linked = linked.clone();
        move |_path: String| {
            let linked = linked.clone();
            async move {
                run(&linked, &["commit", "-q", "--allow-empty", "-m", "moved"]);
            }
        }
    };
    let outcome = run_clear_entry(
        &tracker.inner,
        target.clone(),
        false,
        false,
        Default::default(),
        hook,
    )
    .await;
    let Err(reason) = outcome else {
        panic!("a branch that gained work is never deleted");
    };
    assert_eq!(reason, "branch h4rn/sa-moved has unmerged commits");
    assert!(!linked.exists(), "the checkout itself is removed");
    assert!(
        git::branch_exists(&fixture.repo, "h4rn/sa-moved"),
        "the unmerged branch is kept"
    );

    // A forced clear still deletes it.
    let no_hook = |_| std::future::ready(());
    assert!(matches!(
        run_clear_entry(
            &tracker.inner,
            target,
            true,
            false,
            Default::default(),
            no_hook
        )
        .await,
        Ok(ClearOne::Removed)
    ));
    assert!(!git::branch_exists(&fixture.repo, "h4rn/sa-moved"));
}

#[tokio::test]
async fn a_checkout_that_detaches_after_preflight_is_kept_without_force() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-late-detach", "late-detach-branch");
    let (tracker, _threads, _subagents) = managers(&fixture, Vec::new()).await;
    let target = entry(&linked, &fixture.repo, Some("late-detach-branch"), false);

    // Between preflight and removal the checkout is detached and given a
    // commit no branch reaches. Removal must re-check instead of trusting the
    // preflight's clean, on-branch verdict.
    let hook = {
        let linked = linked.clone();
        move |_path: String| {
            let linked = linked.clone();
            async move {
                run(&linked, &["checkout", "-q", "--detach"]);
                run(
                    &linked,
                    &["commit", "-q", "--allow-empty", "-m", "detached late"],
                );
            }
        }
    };
    let outcome = run_clear_entry(
        &tracker.inner,
        target,
        false,
        false,
        Default::default(),
        hook,
    )
    .await;
    assert_eq!(
        outcome.err().as_deref(),
        Some("detached HEAD has commits on no branch")
    );
    assert!(linked.is_dir(), "the checkout is kept");

    let sha = git::run(&linked, ["rev-parse", "HEAD"]).expect("head");
    let registrations = git::registrations(&fixture.repo).expect("list");
    assert!(
        registrations.iter().any(|registration| {
            registration.path == linked && registration.head == git::Head::Detached(sha.clone())
        }),
        "the registration and its commit stay: {registrations:?}"
    );
}

#[test]
fn an_owned_branch_in_a_rebase_elsewhere_is_not_deleted() {
    let fixture = Fixture::new();
    run(&fixture.repo, &["branch", "h4rn/sa-rebased"]);
    let elsewhere = fixture.add_worktree("sa-rebase-elsewhere", "rebase-elsewhere");
    // Git writes `rebase-merge/head-name` while a rebase is in progress; the
    // branch must survive even though the selected checkout's directory is gone.
    let git_dir =
        PathBuf::from(git::run(&elsewhere, ["rev-parse", "--absolute-git-dir"]).expect("git dir"));
    std::fs::create_dir_all(git_dir.join("rebase-merge")).expect("rebase dir");
    std::fs::write(
        git_dir.join("rebase-merge/head-name"),
        "refs/heads/h4rn/sa-rebased\n",
    )
    .expect("head-name");

    let missing = fixture.root.path().join("sa-rebased-gone");
    let mut own = entry(&missing, &fixture.repo, Some("h4rn/sa-rebased"), true);
    own.dirty = None;
    let outcome = clear_one(&own, false, &fixture.data_dir());
    assert!(
        matches!(
            &outcome,
            Ok(ClearOne::RemovedWithBranchFailure(reason))
                if reason.contains("in use by a rebase")
        ),
        "{outcome:?}"
    );
    assert!(
        git::branch_exists(&fixture.repo, "h4rn/sa-rebased"),
        "a branch a rebase is using survives"
    );
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
fn clearing_one_worktree_keeps_another_missing_worktree_registered() {
    let fixture = Fixture::new();
    let data = fixture.data_dir();
    let clearing = fixture.add_worktree("wt-clearing", "clearing-branch");
    let away = fixture.add_worktree("wt-away", "away-branch");
    let moved = git::norm(&fixture.root.path().join("wt-away-moved"));
    std::fs::rename(&away, &moved).expect("move the unrelated checkout away");
    // Git now marks the unrelated registration prunable, which is exactly the
    // state a repository-wide prune would silently forget.
    let registrations = git::registrations(&fixture.repo).expect("list");
    assert!(
        registrations
            .iter()
            .any(|registration| registration.path == away && registration.prunable),
        "{registrations:?}"
    );

    let target = entry(&clearing, &fixture.repo, Some("clearing-branch"), false);
    assert!(matches!(
        clear_one(&target, false, &data),
        Ok(ClearOne::Removed)
    ));
    assert!(!clearing.exists());

    // Clearing A dropped only A's registration; B is untouched.
    let registrations = git::registrations(&fixture.repo).expect("list");
    assert!(
        registrations
            .iter()
            .any(|registration| registration.path == away),
        "{registrations:?}"
    );

    // Restoring B's directory makes it usable again.
    std::fs::rename(&moved, &away).expect("restore the unrelated checkout");
    assert!(git::is_linked_worktree(&away));
    assert_eq!(
        git::run(&away, ["status", "--porcelain"]).expect("git works there"),
        ""
    );
}

#[test]
fn a_missing_checkouts_own_registration_is_dropped_without_pruning_others() {
    let fixture = Fixture::new();
    let gone = fixture.add_worktree("wt-gone", "gone-branch");
    let kept = fixture.add_worktree("wt-kept", "kept-branch");
    // Removing the directory by hand leaves the registration behind, marked
    // prunable. That is the state a restart or a failed cleanup can leave.
    std::fs::remove_dir_all(&gone).expect("remove the checkout");
    let registrations = git::registrations(&fixture.repo).expect("list");
    assert!(
        registrations
            .iter()
            .any(|registration| registration.path == gone && registration.prunable),
        "{registrations:?}"
    );

    let target = entry(&gone, &fixture.repo, Some("gone-branch"), false);
    assert!(matches!(
        clear_one(&target, false, &fixture.data_dir()),
        Ok(ClearOne::Removed)
    ));
    let registrations = git::registrations(&fixture.repo).expect("list");
    assert!(
        registrations
            .iter()
            .all(|registration| registration.path != gone),
        "only the selected registration is dropped: {registrations:?}"
    );
    assert!(
        registrations
            .iter()
            .any(|registration| registration.path == kept)
    );
}

#[test]
fn a_checkout_that_contains_another_worktree_is_never_removed() {
    let fixture = Fixture::new();
    std::fs::write(fixture.repo.join(".gitignore"), ".claude/\n").expect("gitignore");
    run(&fixture.repo, &["add", ".gitignore"]);
    run(
        &fixture.repo,
        &["commit", "-q", "-m", "ignore nested checkouts"],
    );
    let parent = fixture.add_worktree("parent-wt", "parent-branch");
    let child = parent.join(".claude/worktrees/child");
    std::fs::create_dir_all(child.parent().expect("child parent")).expect("child dir");
    run(
        &parent,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "child-branch",
            &child.to_string_lossy(),
        ],
    );
    let registrations = git::registrations(&fixture.repo).expect("list");
    assert!(
        registrations
            .iter()
            .any(|registration| registration.path == child),
        "{registrations:?}"
    );
    let target = entry(&parent, &fixture.repo, Some("parent-branch"), false);

    for force in [false, true] {
        assert_eq!(
            clear_one(&target, force, &fixture.data_dir())
                .err()
                .as_deref(),
            Some(format!("contains another worktree at {}", child.display()).as_str()),
            "force={force}"
        );
        assert!(parent.is_dir() && child.is_dir(), "both checkouts are kept");
    }
    let registrations = git::registrations(&fixture.repo).expect("list");
    assert!(
        registrations
            .iter()
            .any(|registration| registration.path == parent)
    );
    assert!(
        registrations
            .iter()
            .any(|registration| registration.path == child)
    );
}

#[cfg(unix)]
#[test]
fn a_nested_worktree_whose_name_contains_a_newline_is_detected_as_contained() {
    let fixture = Fixture::new();
    let parent = fixture.add_worktree("parent-newline", "parent-newline-branch");
    let child = parent.join("\nchild");
    std::fs::create_dir_all(child.parent().expect("child parent")).expect("child dir");
    run(
        &parent,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "newline-child-branch",
            &child.to_string_lossy(),
        ],
    );
    let child = git::norm(&child);
    let registrations = git::registrations(&fixture.repo).expect("list");
    assert!(
        registrations
            .iter()
            .any(|registration| registration.path == child),
        "the name is one path, not two lines: {registrations:?}"
    );

    let target = entry(&parent, &fixture.repo, Some("parent-newline-branch"), false);
    assert_eq!(
        clear_one(&target, false, &fixture.data_dir())
            .err()
            .as_deref(),
        Some(format!("contains another worktree at {}", child.display()).as_str())
    );
    assert!(parent.is_dir() && child.is_dir(), "both checkouts are kept");
}

/// Removes a parent whose directory is gone while its nested child's own
/// registration remains; only the parent's registration may go.
fn missing_nested_parent_case(force: bool) {
    let fixture = Fixture::new();
    let parent = fixture.add_worktree("parent-gone-nested", "parent-gone-branch");
    let child = parent.join("nested");
    std::fs::create_dir_all(&child).expect("child dir");
    run(
        &parent,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "nested-gone-branch",
            &child.to_string_lossy(),
        ],
    );
    let child = git::norm(&child);
    // The whole parent goes away; both registrations stay behind, prunable.
    let moved = git::norm(&fixture.root.path().join("parent-gone-nested-moved"));
    std::fs::rename(&parent, &moved).expect("move the parent away");
    assert!(!parent.exists() && !child.exists());

    let target = entry(&parent, &fixture.repo, Some("parent-gone-branch"), false);
    assert!(
        matches!(
            clear_one(&target, force, &fixture.data_dir()),
            Ok(ClearOne::Removed)
        ),
        "force={force}"
    );
    let registrations = git::registrations(&fixture.repo).expect("list");
    assert!(
        registrations
            .iter()
            .all(|registration| registration.path != parent),
        "force={force}: the parent registration is dropped: {registrations:?}"
    );
    assert!(
        registrations
            .iter()
            .any(|registration| registration.path == child),
        "force={force}: the nested registration is kept: {registrations:?}"
    );
}

#[test]
fn a_missing_parent_drops_only_its_own_registration_not_the_nested_one() {
    for force in [false, true] {
        missing_nested_parent_case(force);
    }
}

#[test]
fn a_missing_detached_worktree_whose_commit_no_ref_reaches_keeps_its_registration() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-detached-gone", "detached-gone-base");
    run(&linked, &["checkout", "-q", "--detach"]);
    std::fs::write(linked.join("work.txt"), "work").expect("work file");
    run(&linked, &["add", "work.txt"]);
    run(&linked, &["commit", "-q", "-m", "detached work"]);
    // Move the checkout away by hand: the registration stays, with its
    // detached HEAD, which is the state a failed cleanup leaves behind.
    let moved = git::norm(&fixture.root.path().join("wt-detached-gone-moved"));
    std::fs::rename(&linked, &moved).expect("move the checkout away");
    let registrations = git::registrations(&fixture.repo).expect("list");
    let registration = registrations
        .iter()
        .find(|registration| registration.path == linked)
        .unwrap_or_else(|| panic!("the registration is still listed: {registrations:?}"));
    assert!(registration.prunable);
    let git::Head::Detached(sha) = registration.head.clone() else {
        panic!("a detached registration: {registration:?}");
    };

    let detached = entry(&linked, &fixture.repo, None, false);
    assert_eq!(
        clear_one(&detached, false, &fixture.data_dir())
            .err()
            .as_deref(),
        Some("detached HEAD has commits on no branch")
    );
    let registrations = git::registrations(&fixture.repo).expect("list");
    assert!(
        registrations.iter().any(|registration| {
            registration.path == linked && registration.head == git::Head::Detached(sha.clone())
        }),
        "the registration and its commit stay: {registrations:?}"
    );

    // The same commit is now reachable from a branch, so the registration can
    // go without force.
    run(&fixture.repo, &["branch", "reached", &sha]);
    assert!(matches!(
        clear_one(&detached, false, &fixture.data_dir()),
        Ok(ClearOne::Removed)
    ));
    let registrations = git::registrations(&fixture.repo).expect("list");
    assert!(
        registrations
            .iter()
            .all(|registration| registration.path != linked)
    );
}

#[test]
fn a_replacement_checkout_on_another_branch_is_kept() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-replaced", "listed-branch");
    // Someone removes the listed checkout and recreates another one at the
    // same path on a different branch.
    run(
        &fixture.repo,
        &["worktree", "remove", &linked.to_string_lossy()],
    );
    run(&fixture.repo, &["branch", "replacement-branch"]);
    run(
        &fixture.repo,
        &[
            "worktree",
            "add",
            "-q",
            &linked.to_string_lossy(),
            "replacement-branch",
        ],
    );
    let entries = [entry(&linked, &fixture.repo, Some("listed-branch"), false)];
    assert_eq!(
        clear_one(&entries[0], false, &fixture.data_dir())
            .err()
            .as_deref(),
        Some("checkout is no longer on listed-branch")
    );
    assert!(linked.is_dir(), "the replacement checkout is kept");

    // With force the replacement checkout goes, but only the owned h4rn/
    // branch is ever deleted, and this entry owns none.
    assert!(matches!(
        clear_one(&entries[0], true, &fixture.data_dir()),
        Ok(ClearOne::Removed)
    ));
    assert!(!linked.exists());
    assert!(git::branch_exists(&fixture.repo, "replacement-branch"));
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

    // A listing taken before the file appeared says clean. The preflight
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
    let branches = git::h4rn_branches(&fixture.repo).expect("branches");
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
    let branches = git::h4rn_branches(&fixture.repo).expect("branches");
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

    let outcome = tracker
        .clear(Some(vec![entry.path.clone()]), false, false)
        .await;
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

    let outcome = tracker
        .clear(Some(vec![entry.path.clone()]), false, false)
        .await;
    assert!(outcome.removed.is_empty(), "{:?}", outcome.skipped);
    assert_eq!(outcome.skipped[0].reason, format!("in use by {title}"));
    assert!(path.is_dir(), "the checkout is kept");

    let _ = threads.interrupt(&busy).await;
    let _ = threads.interrupt(&parent).await;
}

#[cfg(unix)]
#[tokio::test]
async fn a_busy_thread_whose_workspace_goes_through_a_symlink_keeps_the_checkout() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-linkbusy", "link-busy-branch");
    let subdir = linked.join("subdir");
    std::fs::create_dir_all(&subdir).expect("subdir");
    let link = fixture.root.path().join("inside-link");
    std::os::unix::fs::symlink(&subdir, &link).expect("symlink");
    let (tracker, threads, _subagents) = managers(
        &fixture,
        vec![Arc::new(FakeHarness::with_events(Vec::new()))],
    )
    .await;

    // The workspace is spelled through the symlink; only resolving it the way
    // the filesystem does lands on the checkout.
    let workspace = link.join("..");
    assert_eq!(git::norm(&workspace), linked);
    let thread = start_thread(&threads, &workspace, "work through the link").await;
    let title = wait_busy(&threads, &thread).await;

    let target = entry(&linked, &fixture.repo, Some("link-busy-branch"), false);
    let outcome = tracker
        .clear(Some(vec![target.path.clone()]), false, false)
        .await;
    assert!(outcome.removed.is_empty(), "{:?}", outcome.skipped);
    assert_eq!(outcome.skipped[0].reason, format!("in use by {title}"));
    assert!(linked.is_dir(), "the checkout is kept");

    let _ = threads.interrupt(&thread).await;
}

#[tokio::test]
async fn a_busy_accessor_found_only_in_the_store_keeps_the_checkout() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-store", "store-branch");
    let elsewhere = fixture.add_worktree("wt-elsewhere", "elsewhere-branch");
    let (tracker, threads, _subagents) = managers(
        &fixture,
        vec![Arc::new(FakeHarness::with_events(Vec::new()))],
    )
    .await;
    // The busiest thread works somewhere else entirely; only its recorded
    // access ties it to this checkout.
    let thread = start_thread(&threads, &elsewhere, "busy elsewhere").await;
    let title = wait_busy(&threads, &thread).await;
    tracker
        .inner
        .store
        .record(
            &linked.to_string_lossy(),
            &fixture.repo.to_string_lossy(),
            Some("store-branch"),
            thread.as_str(),
            None,
            Utc::now(),
        )
        .await
        .expect("record the access");

    let target = entry(&linked, &fixture.repo, Some("store-branch"), false);
    let outcome = tracker
        .clear(Some(vec![target.path.clone()]), false, false)
        .await;
    assert!(outcome.removed.is_empty(), "{:?}", outcome.skipped);
    assert_eq!(outcome.skipped[0].reason, format!("in use by {title}"));
    assert!(linked.is_dir(), "the checkout is kept");

    let _ = threads.interrupt(&thread).await;
}

#[tokio::test]
async fn a_moved_harness_worktree_stays_listed_and_is_cleared_by_registration() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-harness-moved", "harness-moved-branch");
    let (tracker, _threads, _subagents) = managers(&fixture, Vec::new()).await;
    tracker
        .inner
        .store
        .record(
            &linked.to_string_lossy(),
            &fixture.repo.to_string_lossy(),
            Some("harness-moved-branch"),
            "th_gone",
            None,
            Utc::now(),
        )
        .await
        .expect("record the observation");

    // The checkout is moved away; its registration stays behind, prunable.
    let moved = git::norm(&fixture.root.path().join("wt-harness-moved-away"));
    std::fs::rename(&linked, &moved).expect("move the checkout away");

    let listed = tracker.list().await;
    let entry = listed
        .iter()
        .find(|entry| Path::new(&entry.path) == git::norm(&linked))
        .unwrap_or_else(|| panic!("the moved checkout is still listed: {listed:?}"))
        .clone();
    assert!(!entry.exists, "the directory is gone");
    assert_eq!(entry.origin, WorktreeOrigin::Harness);

    // A clear by path drops the registration rather than reporting "not tracked".
    let outcome = tracker
        .clear(Some(vec![entry.path.clone()]), false, false)
        .await;
    assert_eq!(
        outcome.removed,
        vec![entry.path.clone()],
        "skipped: {:?}",
        outcome.skipped
    );
    let registrations = git::registrations(&fixture.repo).expect("list");
    assert!(
        registrations
            .iter()
            .all(|registration| registration.path != linked),
        "{registrations:?}"
    );
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

    let outcome = tracker
        .clear(Some(vec![entry.path.clone()]), false, false)
        .await;
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
    let outcome = run_clear_entry(
        &tracker.inner,
        entry,
        false,
        false,
        Default::default(),
        hook,
    )
    .await;

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
    // git's own refusal is reported in our words, not git's. With no verified
    // branch there is nothing for removal to do beyond the checkout.
    let prepared = prepare(&entries[0], false, &Default::default()).expect("prepare");
    assert_eq!(
        execute(
            &entries[0],
            false,
            &fixture.data_dir(),
            &super::Preflight { branch: None },
            &prepared,
            &Default::default(),
        )
        .err()
        .as_deref(),
        Some("has uncommitted changes")
    );
    assert!(linked.is_dir(), "git's refusal keeps the checkout");
}

#[cfg(unix)]
#[test]
fn worktrees_with_paths_that_cannot_round_trip_are_skipped() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let fixture = Fixture::new();
    let valid = fixture.add_worktree("wt-utf8", "utf8-branch");
    let invalid = fixture
        .root
        .path()
        .join(OsString::from_vec(b"wt-\xff".to_vec()));
    let output = std::process::Command::new("git")
        .args(["worktree", "add", "-q", "-b", "invalid-branch"])
        .arg(&invalid)
        .current_dir(&fixture.repo)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The raw NUL-delimited snapshot still names the path as bytes.
    let registrations = git::registrations(&fixture.repo).expect("registrations");
    assert!(
        registrations
            .iter()
            .any(|registration| registration.path == invalid),
        "{registrations:?}"
    );
    // The protocol-facing list never offers the lossy string.
    let listed = git::list_worktrees(&fixture.repo).expect("list");
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].path, valid);
    assert!(
        listed
            .iter()
            .all(|worktree| worktree.path.to_str().is_some())
    );
}

#[test]
fn a_failed_registration_snapshot_keeps_a_missing_stored_row() {
    let fixture = Fixture::new();
    let missing = fixture.root.path().join("vanished-harness");
    // A directory that is not a repository: the snapshot fails rather than
    // proving the registration is gone.
    let plain = tempfile::tempdir().expect("plain dir");
    let stored = |repo: &Path| StoredWorktree {
        path: missing.to_string_lossy().into_owned(),
        repo: repo.to_string_lossy().into_owned(),
        branch: Some("gone-branch".into()),
        first_seen: Utc::now(),
        last_seen: Utc::now(),
        accesses: Vec::new(),
    };

    let mut list_input = input(&fixture, Vec::new());
    list_input.stored = vec![stored(plain.path())];
    let (entries, gone) = compute(&list_input, false);
    assert!(gone.is_empty(), "a failed snapshot never forgets the row");
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert!(!entries[0].exists);

    // A successful snapshot in a real repository that does not name the path
    // is proof of absence, so the row can go.
    list_input.stored = vec![stored(&fixture.repo)];
    let (entries, gone) = compute(&list_input, false);
    assert!(entries.is_empty(), "{entries:?}");
    assert_eq!(gone, vec![missing.to_string_lossy().into_owned()]);
}

#[tokio::test]
async fn a_nested_worktree_created_after_preflight_is_detected() {
    let fixture = Fixture::new();
    std::fs::write(fixture.repo.join(".gitignore"), ".claude/\n").expect("gitignore");
    run(&fixture.repo, &["add", ".gitignore"]);
    run(
        &fixture.repo,
        &["commit", "-q", "-m", "ignore nested checkouts"],
    );
    let parent = fixture.add_worktree("parent-late-nested", "parent-late-branch");
    let (tracker, _threads, _subagents) = managers(&fixture, Vec::new()).await;
    let target = entry(&parent, &fixture.repo, Some("parent-late-branch"), false);

    let hook = {
        let parent = parent.clone();
        move |_path: String| {
            let parent = parent.clone();
            async move {
                let child = parent.join(".claude/worktrees/late-child");
                std::fs::create_dir_all(child.parent().expect("child parent")).expect("child dir");
                run(
                    &parent,
                    &[
                        "worktree",
                        "add",
                        "-q",
                        "-b",
                        "late-child-branch",
                        &child.to_string_lossy(),
                    ],
                );
            }
        }
    };
    let outcome = run_clear_entry(
        &tracker.inner,
        target,
        false,
        false,
        Default::default(),
        hook,
    )
    .await;
    let Err(reason) = outcome else {
        panic!("a nested worktree created after preflight is never removed");
    };
    assert!(
        reason.starts_with("contains another worktree at "),
        "{reason}"
    );
    assert!(parent.is_dir(), "the parent checkout is kept");
    assert!(
        parent.join(".claude/worktrees/late-child").is_dir(),
        "the nested checkout is kept"
    );
}

#[tokio::test]
async fn a_checkout_detached_and_moved_away_after_preflight_keeps_its_registration() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-late-moved", "late-moved-branch");
    let (tracker, _threads, _subagents) = managers(&fixture, Vec::new()).await;
    let target = entry(&linked, &fixture.repo, Some("late-moved-branch"), false);
    let moved = git::norm(&fixture.root.path().join("wt-late-moved-away"));

    // After preflight the checkout is detached, given a commit no ref reaches,
    // and moved away: only the registration (and its HEAD) is left to inspect.
    let hook = {
        let linked = linked.clone();
        let moved = moved.clone();
        move |_path: String| {
            let linked = linked.clone();
            let moved = moved.clone();
            async move {
                run(&linked, &["checkout", "-q", "--detach"]);
                run(
                    &linked,
                    &["commit", "-q", "--allow-empty", "-m", "detached then moved"],
                );
                std::fs::rename(&linked, &moved).expect("move the checkout away");
            }
        }
    };
    let outcome = run_clear_entry(
        &tracker.inner,
        target,
        false,
        false,
        Default::default(),
        hook,
    )
    .await;
    assert_eq!(
        outcome.err().as_deref(),
        Some("detached HEAD has commits on no branch")
    );
    assert!(!linked.exists());
    let registrations = git::registrations(&fixture.repo).expect("list");
    assert!(
        registrations.iter().any(|registration| {
            registration.path == linked && matches!(registration.head, git::Head::Detached(_))
        }),
        "the detached registration stays: {registrations:?}"
    );
}

#[test]
fn a_branch_checked_out_after_the_checkout_is_removed_is_not_deleted() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("sa-occupied", "h4rn/sa-occupied");
    let target = entry(&linked, &fixture.repo, Some("h4rn/sa-occupied"), true);
    let plan = preflight(&target, false, &Default::default()).expect("preflight");

    // Execute's checkout removal.
    run(
        &fixture.repo,
        &["worktree", "remove", &linked.to_string_lossy()],
    );
    // A second worktree takes the branch over before the delete step.
    let elsewhere = fixture.root.path().join("sa-occupied-elsewhere");
    run(
        &fixture.repo,
        &[
            "worktree",
            "add",
            "-q",
            &elsewhere.to_string_lossy(),
            "h4rn/sa-occupied",
        ],
    );

    let outcome = delete_owned_branch(
        &fixture.repo,
        "h4rn/sa-occupied",
        &plan,
        false,
        &Default::default(),
    )
    .expect("a checked-out branch is a reported skip");
    assert!(
        outcome
            .as_deref()
            .is_some_and(|reason| reason.contains("it is checked out in")),
        "{outcome:?}"
    );
    assert!(
        git::branch_exists(&fixture.repo, "h4rn/sa-occupied"),
        "a branch another worktree has checked out survives"
    );
}

#[test]
fn a_registration_whose_head_is_unknown_blocks_the_branch_delete() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("sa-unknown-holder", "h4rn/sa-unknown-holder");
    let target = entry(&linked, &fixture.repo, Some("h4rn/sa-unknown-holder"), true);
    let plan = preflight(&target, false, &Default::default()).expect("preflight");
    run(
        &fixture.repo,
        &["worktree", "remove", &linked.to_string_lossy()],
    );
    // Another worktree whose HEAD cannot be read might hold the branch.
    let other = fixture.add_worktree("wt-unreadable-head", "unreadable-head-branch");
    let admin =
        PathBuf::from(git::run(&other, ["rev-parse", "--absolute-git-dir"]).expect("admin dir"));
    std::fs::write(admin.join("HEAD"), "not a head\n").expect("garbage HEAD");

    git::FORCE_METADATA_FALLBACK.with(|forced| forced.set(true));
    let outcome = delete_owned_branch(
        &fixture.repo,
        "h4rn/sa-unknown-holder",
        &plan,
        false,
        &Default::default(),
    );
    git::FORCE_METADATA_FALLBACK.with(|forced| forced.set(false));
    let outcome = outcome.expect("an unknown holder is a reported skip");
    assert!(
        outcome
            .as_deref()
            .is_some_and(|reason| reason.contains("could not be checked: the HEAD of")),
        "{outcome:?}"
    );
    assert!(git::branch_exists(&fixture.repo, "h4rn/sa-unknown-holder"));
}

#[tokio::test]
async fn a_branch_rewound_after_preflight_is_kept_and_reported_as_unmerged() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("sa-rewound", "h4rn/sa-rewound");
    run(
        &linked,
        &["commit", "-q", "--allow-empty", "-m", "child work"],
    );
    // Merge first, so preflight sees a merged tip; the hook then moves main's
    // HEAD back and the tip stops being an ancestor.
    run(
        &fixture.repo,
        &["merge", "-q", "--ff-only", "h4rn/sa-rewound"],
    );
    let (tracker, _threads, _subagents) = managers(&fixture, Vec::new()).await;
    let target = entry(&linked, &fixture.repo, Some("h4rn/sa-rewound"), true);

    let hook = {
        let repo = fixture.repo.clone();
        move |_path: String| {
            let repo = repo.clone();
            async move {
                run(&repo, &["reset", "-q", "--hard", "HEAD~1"]);
            }
        }
    };
    let outcome = run_clear_entry(
        &tracker.inner,
        target,
        false,
        false,
        Default::default(),
        hook,
    )
    .await;
    assert_eq!(
        outcome.err().as_deref(),
        Some("branch h4rn/sa-rewound has unmerged commits")
    );
    assert!(!linked.exists(), "the checkout itself is removed");
    assert!(
        git::branch_exists(&fixture.repo, "h4rn/sa-rewound"),
        "the no-longer-merged branch is kept"
    );
}

#[test]
fn rebase_files_match_whole_lines_not_substrings() {
    let fixture = Fixture::new();
    run(&fixture.repo, &["branch", "h4rn/sa-abc"]);
    let elsewhere = fixture.add_worktree("sa-rebase-longer", "rebase-longer");
    let git_dir =
        PathBuf::from(git::run(&elsewhere, ["rev-parse", "--absolute-git-dir"]).expect("git dir"));
    std::fs::create_dir_all(git_dir.join("rebase-merge")).expect("rebase dir");
    std::fs::write(
        git_dir.join("rebase-merge/head-name"),
        "refs/heads/h4rn/sa-abc2\n",
    )
    .expect("head-name");
    std::fs::write(
        git_dir.join("rebase-merge/update-refs"),
        "update refs/heads/h4rn/sa-abc2\n",
    )
    .expect("update-refs");

    let missing = fixture.root.path().join("sa-abc-gone");
    let mut own = entry(&missing, &fixture.repo, Some("h4rn/sa-abc"), true);
    own.dirty = None;
    assert!(
        matches!(
            clear_one(&own, false, &fixture.data_dir()),
            Ok(ClearOne::Removed)
        ),
        "a rebase on h4rn/sa-abc2 does not protect h4rn/sa-abc"
    );
    assert!(!git::branch_exists(&fixture.repo, "h4rn/sa-abc"));
}

#[test]
fn a_bisect_naming_the_branch_blocks_its_deletion() {
    let fixture = Fixture::new();
    run(&fixture.repo, &["branch", "h4rn/sa-bisected"]);
    let elsewhere = fixture.add_worktree("sa-bisect-elsewhere", "bisect-elsewhere");
    let git_dir =
        PathBuf::from(git::run(&elsewhere, ["rev-parse", "--absolute-git-dir"]).expect("git dir"));
    std::fs::write(git_dir.join("BISECT_START"), "h4rn/sa-bisected\n").expect("bisect start");

    let missing = fixture.root.path().join("sa-bisected-gone");
    let mut own = entry(&missing, &fixture.repo, Some("h4rn/sa-bisected"), true);
    own.dirty = None;
    let outcome = clear_one(&own, false, &fixture.data_dir());
    assert!(
        matches!(
            &outcome,
            Ok(ClearOne::RemovedWithBranchFailure(reason))
                if reason.contains("in use by a bisect")
        ),
        "{outcome:?}"
    );
    assert!(
        git::branch_exists(&fixture.repo, "h4rn/sa-bisected"),
        "a branch a bisect is using survives"
    );
}

#[test]
fn a_locked_worktree_is_reported_as_locked() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-locked", "locked-branch");
    run(
        &fixture.repo,
        &["worktree", "lock", &linked.to_string_lossy()],
    );
    let target = entry(&linked, &fixture.repo, Some("locked-branch"), false);
    assert_eq!(
        clear_one(&target, false, &fixture.data_dir())
            .err()
            .as_deref(),
        Some("is locked; unlock it first")
    );
    assert!(linked.is_dir(), "a locked checkout is kept");
}

#[test]
fn a_locked_missing_checkout_keeps_its_registration_and_branch() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("sa-locked-missing", "h4rn/sa-locked-missing");
    run(
        &fixture.repo,
        &["worktree", "lock", &linked.to_string_lossy()],
    );
    std::fs::remove_dir_all(&linked).expect("remove the checkout");
    let mut own = entry(&linked, &fixture.repo, Some("h4rn/sa-locked-missing"), true);
    own.dirty = None;

    let outcome = clear_one(&own, false, &fixture.data_dir());
    assert_eq!(outcome.err().as_deref(), Some("is locked; unlock it first"));
    let registrations = git::registrations(&fixture.repo).expect("list");
    assert!(
        registrations
            .iter()
            .any(|registration| registration.path == linked && registration.locked),
        "the locked registration stays: {registrations:?}"
    );
    assert!(
        git::branch_exists(&fixture.repo, "h4rn/sa-locked-missing"),
        "the branch is untouched while its registration remains"
    );
}

/// Writes an executable `git` wrapper that runs `body` before handing every
/// other call to the real git.
#[cfg(unix)]
///
/// The file is written, synced and closed before it is made executable, so no
/// write descriptor of ours is open when it is first run (`ETXTBSY`).
fn fake_git(dir: &Path, body: &str) -> PathBuf {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("fake-git");
    {
        let mut file = std::fs::File::create(&path).expect("fake git");
        file.write_all(format!("#!/bin/sh\n{body}\nexec git \"$@\"\n").as_bytes())
            .expect("write fake git");
        file.sync_all().expect("sync fake git");
    }
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

#[tokio::test]
async fn git_s_removal_backstop_counts_untracked_files_whatever_the_config() {
    let fixture = Fixture::new();
    run(
        &fixture.repo,
        &["config", "status.showUntrackedFiles", "no"],
    );
    let linked = fixture.add_worktree("wt-backstop", "backstop-branch");
    let (tracker, _threads, _subagents) = managers(&fixture, Vec::new()).await;
    let target = entry(&linked, &fixture.repo, Some("backstop-branch"), false);

    // The file appears after preflight's own dirty check, so only git's
    // refusal inside `git worktree remove` can catch it, and that refusal
    // runs `git status` under the user's config unless it is overridden.
    let hook = {
        let linked = linked.clone();
        move |_path: String| {
            let linked = linked.clone();
            async move {
                std::fs::write(linked.join("late.txt"), "late work").expect("late file");
            }
        }
    };
    let outcome = run_clear_entry(
        &tracker.inner,
        target,
        false,
        false,
        Default::default(),
        hook,
    )
    .await;
    assert_eq!(outcome.err().as_deref(), Some("has uncommitted changes"));
    assert!(
        linked.join("late.txt").is_file(),
        "the untracked file is kept"
    );
}

#[test]
fn a_checkout_switched_to_another_branch_before_execute_is_kept() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-switch", "switch-branch");
    let target = entry(&linked, &fixture.repo, Some("switch-branch"), false);
    let plan = preflight(&target, false, &Default::default()).expect("preflight");
    let prepared = prepare(&target, false, &Default::default()).expect("prepare");

    // Between the live activity check and the destructive call.
    run(&linked, &["checkout", "-q", "-b", "switched-late"]);
    assert_eq!(
        execute(
            &target,
            false,
            &fixture.data_dir(),
            &plan,
            &prepared,
            &Default::default()
        )
        .err()
        .as_deref(),
        Some("checkout is no longer on switch-branch")
    );
    assert!(linked.is_dir(), "the switched checkout is kept");
}

#[test]
fn a_checkout_detached_and_moved_away_before_execute_keeps_its_registration() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-flip", "flip-branch");
    let target = entry(&linked, &fixture.repo, Some("flip-branch"), false);
    let plan = preflight(&target, false, &Default::default()).expect("preflight");
    let prepared = prepare(&target, false, &Default::default()).expect("prepare");

    // Prepare saw an existing checkout; by execute it is detached onto a
    // commit no ref reaches and its directory is gone. The checks for the
    // state that exists now must run on a fresh snapshot.
    run(&linked, &["checkout", "-q", "--detach"]);
    run(
        &linked,
        &["commit", "-q", "--allow-empty", "-m", "detached late"],
    );
    let moved = git::norm(&fixture.root.path().join("wt-flip-moved"));
    std::fs::rename(&linked, &moved).expect("move the checkout away");
    assert_eq!(
        execute(
            &target,
            false,
            &fixture.data_dir(),
            &plan,
            &prepared,
            &Default::default()
        )
        .err()
        .as_deref(),
        Some("detached HEAD has commits on no branch")
    );
    let registrations = git::registrations(&fixture.repo).expect("list");
    assert!(
        registrations.iter().any(|registration| {
            registration.path == linked && matches!(registration.head, git::Head::Detached(_))
        }),
        "the detached registration stays: {registrations:?}"
    );
}

#[cfg(unix)]
#[test]
fn a_thread_in_a_checkout_named_with_a_trailing_space_is_attributed_to_it() {
    let fixture = Fixture::new();
    let plain = fixture.add_worktree("checkout", "plain-branch");
    let spaced = fixture.add_worktree("checkout ", "spaced-branch");
    assert_ne!(plain, spaced);
    let mut list_input = input(&fixture, Vec::new());
    list_input.threads = vec![ThreadInfo {
        thread_id: ThreadId::new("th_spaced"),
        title: "spaced".into(),
        workspace: spaced.to_string_lossy().into_owned(),
        working: false,
        last_activity: Utc::now(),
    }];
    let (entries, _) = compute(&list_input, false);
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(Path::new(&entries[0].path), spaced);
    assert_eq!(entries[0].branch.as_deref(), Some("spaced-branch"));
    assert_eq!(entries[0].accessed_by[0].thread_id.as_str(), "th_spaced");
}

#[cfg(unix)]
#[test]
fn the_metadata_fallback_detects_a_newline_named_nested_worktree() {
    use super::contained_worktree;

    let fixture = Fixture::new();
    let parent = fixture.add_worktree("parent-meta", "parent-meta-branch");
    let child = parent.join("\nchild");
    run(
        &parent,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "meta-child-branch",
            &child.to_string_lossy(),
        ],
    );
    let child = git::norm(&child);
    let registrations =
        git::registrations_from_metadata(&fixture.repo, Duration::from_secs(30)).expect("read");
    assert_eq!(
        contained_worktree(&registrations, &parent),
        Some(child.clone())
    );

    // The whole clear on this thread reads the metadata instead of `-z`.
    git::FORCE_METADATA_FALLBACK.with(|forced| forced.set(true));
    let target = entry(&parent, &fixture.repo, Some("parent-meta-branch"), false);
    let outcome = clear_one(&target, false, &fixture.data_dir());
    git::FORCE_METADATA_FALLBACK.with(|forced| forced.set(false));
    assert_eq!(
        outcome.err().as_deref(),
        Some(format!("contains another worktree at {}", child.display()).as_str())
    );
    assert!(parent.is_dir() && child.is_dir(), "both checkouts are kept");
}

#[test]
fn the_metadata_fallback_reads_detached_locked_and_prunable_registrations() {
    let fixture = Fixture::new();
    let detached = fixture.add_worktree("wt-meta-detached", "meta-detached-base");
    run(&detached, &["checkout", "-q", "--detach"]);
    run(
        &fixture.repo,
        &["worktree", "lock", &detached.to_string_lossy()],
    );
    let gone = fixture.add_worktree("wt-meta-gone", "meta-gone-branch");
    std::fs::remove_dir_all(&gone).expect("remove the checkout");
    let sha = git::run(&detached, ["rev-parse", "HEAD"]).expect("head");

    let limit = Duration::from_secs(30);
    let metadata = git::registrations_from_metadata(&fixture.repo, limit).expect("read");
    let find = |path: &Path| {
        metadata
            .iter()
            .find(|registration| registration.path == path)
            .unwrap_or_else(|| panic!("{} is listed: {metadata:?}", path.display()))
    };
    let main = find(&fixture.repo);
    assert_eq!(main.head, git::Head::Branch("main".into()));
    assert!(!main.locked && !main.prunable);
    let locked = find(&detached);
    assert!(locked.locked && !locked.prunable);
    assert_eq!(locked.head, git::Head::Detached(sha.clone()));
    let missing = find(&gone);
    assert_eq!(missing.head, git::Head::Branch("meta-gone-branch".into()));
    assert!(missing.prunable && !missing.locked);

    // Both sources agree, record for record.
    assert_metadata_matches_porcelain(&fixture.repo);
}

/// Asserts the metadata fallback reads exactly what `-z` reports for `repo`.
fn assert_metadata_matches_porcelain(repo: &Path) {
    let limit = Duration::from_secs(30);
    let sorted = |mut registrations: Vec<git::Registration>| {
        registrations.sort_by(|a, b| a.path.cmp(&b.path));
        registrations
    };
    let metadata = git::registrations_from_metadata(repo, limit).expect("read");
    assert!(
        metadata.complete,
        "every registration was read: {metadata:?}"
    );
    let porcelain = git::porcelain_z(repo, limit)
        .expect("run")
        .expect("this git supports -z");
    assert_eq!(sorted(metadata.registrations), sorted(porcelain));
}

#[cfg(unix)]
#[test]
fn a_git_that_rejects_porcelain_z_is_detected_strictly() {
    let fixture = Fixture::new();
    let tools = tempfile::tempdir().expect("tools");
    let limit = Duration::from_secs(30);
    let with_program = |program: PathBuf| {
        git::GIT_PROGRAM.with(|slot| *slot.borrow_mut() = Some(program));
        let result = git::porcelain_z(&fixture.repo, limit);
        git::GIT_PROGRAM.with(|slot| *slot.borrow_mut() = None);
        result
    };

    // Old git: `unknown switch` naming `z`, then its usage text, exit 129.
    let old = fake_git(
        tools.path(),
        r#"for arg in "$@"; do
  if [ "$arg" = "-z" ]; then
    echo "error: unknown switch \`z'" >&2
    echo "usage: git worktree list [<options>]" >&2
    exit 129
  fi
done"#,
    );
    assert_eq!(with_program(old), Ok(None));

    // Exit 129 with `worktree` in the usage text also counts.
    let usage_only = tools.path().join("usage-only");
    std::fs::create_dir_all(&usage_only).expect("dir");
    let usage = fake_git(
        &usage_only,
        r#"echo "usage: git worktree list [<options>]" >&2; exit 129"#,
    );
    assert_eq!(with_program(usage), Ok(None));

    // A bare `usage:` that is not about `worktree`, or any other failure, is
    // an error rather than a reason to fall back.
    let other = tools.path().join("other");
    std::fs::create_dir_all(&other).expect("dir");
    let unrelated = fake_git(&other, r#"echo "usage: git frobnicate" >&2; exit 1"#);
    assert!(matches!(with_program(unrelated), Err(error) if error.contains("frobnicate")));

    // Exit 129 alone is not enough: a usage text that does not mention
    // `worktree` is an error too.
    let bare_usage = tools.path().join("bare-usage");
    std::fs::create_dir_all(&bare_usage).expect("dir");
    let bare = fake_git(
        &bare_usage,
        r#"echo "usage: git [-v | --version] <command>" >&2; exit 129"#,
    );
    assert!(matches!(with_program(bare), Err(error) if error.starts_with("usage: git [-v")));
}

#[test]
fn a_nested_clone_of_another_repository_is_never_removed() {
    let fixture = Fixture::new();
    std::fs::write(fixture.repo.join(".gitignore"), "vendor/\n").expect("gitignore");
    run(&fixture.repo, &["add", ".gitignore"]);
    run(&fixture.repo, &["commit", "-q", "-m", "ignore vendor"]);
    let host = fixture.add_worktree("wt-clone-host", "clone-host-branch");
    let clone = host.join("vendor/other");
    std::fs::create_dir_all(&clone).expect("clone dir");
    run(&clone, &["init", "-q"]);
    std::fs::write(clone.join("lib.txt"), "v1").expect("file");
    run(&clone, &["add", "lib.txt"]);
    run(
        &clone,
        &[
            "-c",
            "user.email=wt@h4rn.invalid",
            "-c",
            "user.name=wt",
            "commit",
            "-q",
            "-m",
            "v1",
        ],
    );
    std::fs::write(clone.join("lib.txt"), "local edit").expect("local edit");
    let target = entry(&host, &fixture.repo, Some("clone-host-branch"), false);
    assert_eq!(target.dirty, Some(false), "git status does not see it");

    for force in [false, true] {
        assert_eq!(
            clear_one(&target, force, &fixture.data_dir())
                .err()
                .as_deref(),
            Some(format!("contains another git checkout at {}", clone.display()).as_str()),
            "force={force}"
        );
        assert_eq!(
            std::fs::read_to_string(clone.join("lib.txt")).expect("edit kept"),
            "local edit"
        );
        assert!(host.is_dir() && clone.join(".git").is_dir());
    }
}

#[tokio::test]
async fn an_owned_row_stays_listed_while_its_registration_remains() {
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
            task: "leave a registration".into(),
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
    let path = PathBuf::from(&settled.path);
    let branch = settled.branch.clone().expect("kept branch");

    // A registration of its own at the subagent's path, with the branch gone
    // and the directory moved away: only the registration is left to clear.
    run(
        &fixture.repo,
        &["worktree", "add", "-q", "--detach", &path.to_string_lossy()],
    );
    run(&fixture.repo, &["branch", "-D", &branch]);
    let moved = git::norm(&fixture.root.path().join("sa-registration-moved"));
    std::fs::rename(&path, &moved).expect("move the checkout away");

    let listed = tracker.list().await;
    let entry = listed
        .iter()
        .find(|entry| entry.subagent_id.as_ref() == Some(&view.subagent_id))
        .unwrap_or_else(|| panic!("the registered row is still listed: {listed:?}"))
        .clone();
    assert!(!entry.exists);
    assert_eq!(
        entry.branch, None,
        "the branch is known gone, so the row never promises it"
    );

    let outcome = tracker
        .clear(Some(vec![entry.path.clone()]), false, false)
        .await;
    assert_eq!(
        outcome.removed,
        vec![entry.path.clone()],
        "skipped: {:?}",
        outcome.skipped
    );
    let registrations = git::registrations(&fixture.repo).expect("list");
    assert!(
        registrations
            .iter()
            .all(|registration| registration.path != path),
        "{registrations:?}"
    );
    assert!(
        outcome
            .worktrees
            .iter()
            .all(|row| row.subagent_id.as_ref() != Some(&view.subagent_id)),
        "with nothing left the row is no longer listed"
    );
    let _ = threads.interrupt(&parent).await;
}

#[cfg(unix)]
#[test]
fn a_branch_lookup_failure_after_removal_is_reported() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("sa-lookup", "h4rn/sa-lookup");
    let own = entry(&linked, &fixture.repo, Some("h4rn/sa-lookup"), true);
    let prepared = prepare(&own, false, &Default::default()).expect("prepare");
    let tools = tempfile::tempdir().expect("tools");
    let failing = fake_git(
        tools.path(),
        r#"for arg in "$@"; do
  case "$arg" in *"%(objectname)"*) echo "fatal: ref lookup failed" >&2; exit 128;; esac
done"#,
    );
    git::GIT_PROGRAM.with(|slot| *slot.borrow_mut() = Some(failing));
    let outcome = execute(
        &own,
        false,
        &fixture.data_dir(),
        &super::Preflight { branch: None },
        &prepared,
        &Default::default(),
    );
    git::GIT_PROGRAM.with(|slot| *slot.borrow_mut() = None);
    assert!(
        matches!(
            &outcome,
            Ok(ClearOne::RemovedWithBranchFailure(reason)) if reason
                == "checkout removed, but branch h4rn/sa-lookup could not be checked: fatal: ref lookup failed"
        ),
        "{outcome:?}"
    );
    assert!(!linked.exists());
    assert!(git::branch_exists(&fixture.repo, "h4rn/sa-lookup"));
}

#[test]
fn update_ref_refusals_are_triaged_by_the_new_tip() {
    let fixture = Fixture::new();
    let repo = fixture.repo.clone();
    run(&repo, &["commit", "-q", "--allow-empty", "-m", "second"]);
    let branch = "h4rn/sa-triage";
    let missing = fixture.root.path().join("sa-triage-gone");
    let mut own = entry(&missing, &repo, Some(branch), true);
    own.dirty = None;

    let case = |hook: Box<dyn FnOnce()>| {
        run(&repo, &["branch", "-f", branch, "main"]);
        let plan = preflight(&own, false, &Default::default()).expect("preflight");
        BEFORE_UPDATE_REF.with(|slot| *slot.borrow_mut() = Some(hook));
        delete_owned_branch(&repo, branch, &plan, false, &Default::default())
    };

    // Deleted by someone else: the branch is gone, which is what we wanted.
    let deleting = repo.clone();
    let outcome = case(Box::new(move || {
        run(
            &deleting,
            &["update-ref", "-d", "refs/heads/h4rn/sa-triage"],
        );
    }));
    assert_eq!(outcome, Ok(None));
    assert!(!git::branch_exists(&repo, branch));

    // Moved to another commit already merged into HEAD.
    let rewinding = repo.clone();
    let outcome = case(Box::new(move || {
        run(
            &rewinding,
            &["update-ref", "refs/heads/h4rn/sa-triage", "main~1"],
        );
    }));
    assert_eq!(
        outcome,
        Err("branch h4rn/sa-triage changed during the clear".into())
    );
    assert!(git::branch_exists(&repo, branch));

    // Moved to a commit HEAD does not contain.
    let advancing = repo.clone();
    let outcome = case(Box::new(move || {
        let side = git::run(
            &advancing,
            ["commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "side"],
        )
        .expect("commit-tree");
        run(
            &advancing,
            &["update-ref", "refs/heads/h4rn/sa-triage", &side],
        );
    }));
    assert_eq!(
        outcome,
        Err("branch h4rn/sa-triage has unmerged commits".into())
    );
    assert!(git::branch_exists(&repo, branch));
}

#[test]
fn a_row_whose_repository_vanished_is_forgotten() {
    let fixture = Fixture::new();
    let gone_repo = fixture.root.path().join("deleted-repo");
    let path = fixture.root.path().join("deleted-repo-wt");
    std::fs::create_dir_all(&path).expect("leftover dir");
    let mut list_input = input(&fixture, Vec::new());
    list_input.stored = vec![StoredWorktree {
        path: path.to_string_lossy().into_owned(),
        repo: gone_repo.to_string_lossy().into_owned(),
        branch: None,
        first_seen: Utc::now(),
        last_seen: Utc::now(),
        accesses: Vec::new(),
    }];
    let (entries, gone) = compute(&list_input, false);
    assert!(entries.is_empty(), "{entries:?}");
    assert_eq!(gone, vec![path.to_string_lossy().into_owned()]);
    assert!(path.is_dir(), "only the store row is forgotten");
}

/// Commits `.gitignore` with `pattern` in the fixture's repository.
fn ignore(fixture: &Fixture, pattern: &str) {
    std::fs::write(fixture.repo.join(".gitignore"), format!("{pattern}\n")).expect("gitignore");
    run(&fixture.repo, &["add", ".gitignore"]);
    run(&fixture.repo, &["commit", "-q", "-m", "ignore"]);
}

/// A plain repository with one commit at `dir`.
fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).expect("repo dir");
    run(dir, &["init", "-q", "-b", "main"]);
    run(
        dir,
        &[
            "-c",
            "user.email=wt@h4rn.invalid",
            "-c",
            "user.name=wt",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ],
    );
}

#[test]
fn a_submodule_is_forceable_but_another_repository_s_gitfile_is_not() {
    let fixture = Fixture::new();
    let upstream = fixture.root.path().join("upstream");
    init_repo(&upstream);
    let host = fixture.add_worktree("wt-submodule", "submodule-branch");
    run(
        &host,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            &upstream.to_string_lossy(),
            "sub",
        ],
    );
    run(&host, &["commit", "-q", "-m", "add a submodule"]);
    assert!(host.join("sub/.git").is_file(), "an initialised submodule");
    let target = entry(&host, &fixture.repo, Some("submodule-branch"), false);
    assert_eq!(target.dirty, Some(false));

    // Git's own refusal, mapped to a forceable reason; nothing is touched.
    assert_eq!(
        clear_one(&target, false, &fixture.data_dir())
            .err()
            .as_deref(),
        Some("contains submodules")
    );
    assert!(host.join("sub/.git").is_file());
    // Forced, git removes the checkout and its submodule.
    assert!(matches!(
        clear_one(&target, true, &fixture.data_dir()),
        Ok(ClearOne::Removed)
    ));
    assert!(!host.exists());

    // A `.git` file pointing outside this repository's common dir is another
    // repository's checkout: refused even with force.
    let other = fixture.add_worktree("wt-foreign-gitfile", "foreign-gitfile-branch");
    let clone = other.join("vendor/other");
    std::fs::create_dir_all(&clone).expect("clone dir");
    let separate = fixture.root.path().join("separate.git");
    run(
        &clone,
        &[
            "init",
            "-q",
            "--separate-git-dir",
            &separate.to_string_lossy(),
        ],
    );
    assert!(clone.join(".git").is_file());
    let foreign = entry(&other, &fixture.repo, Some("foreign-gitfile-branch"), false);
    assert_eq!(
        clear_one(&foreign, true, &fixture.data_dir())
            .err()
            .as_deref(),
        Some(
            format!(
                "contains another git checkout at {}",
                git::norm(&clone).display()
            )
            .as_str()
        )
    );
    assert!(clone.join(".git").is_file() && separate.is_dir());
}

#[test]
fn an_unreadable_registration_head_is_unknown_in_both_sources() {
    // `-z`: a record with neither `branch` nor `detached` (a bare one too),
    // `detached` without its sha, or a detached HEAD that is not a full hex
    // object id, is unknown.
    let full = "abcd".repeat(10);
    let out = format!(
        "worktree /wt-a\0HEAD 1234\0\0worktree /wt-b\0HEAD abcd\0detached\0\0\
         worktree /wt-c\0bare\0\0worktree /wt-d\0detached\0\0\
         worktree /wt-e\0HEAD 1234\0detached\0\0worktree /wt-f\0HEAD {}\0detached\0\0\
         worktree /wt-g\0HEAD {full}\0detached\0\0",
        "g".repeat(40),
    );
    let parsed = git::parse_registrations(out.as_bytes());
    let heads: Vec<git::Head> = parsed.into_iter().map(|r| r.head).collect();
    assert_eq!(
        heads,
        vec![
            git::Head::Unknown,
            git::Head::Unknown,
            git::Head::Unknown,
            git::Head::Unknown,
            git::Head::Unknown,
            git::Head::Unknown,
            git::Head::Detached(full.clone()),
        ]
    );

    // Metadata: a `HEAD` file that does not parse.
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-unknown-head", "unknown-head-branch");
    let admin =
        PathBuf::from(git::run(&linked, ["rev-parse", "--absolute-git-dir"]).expect("admin dir"));
    std::fs::remove_dir_all(&linked).expect("remove the checkout");
    for garbage in ["not a head\n", "1234\n"] {
        std::fs::write(admin.join("HEAD"), garbage).expect("garbage HEAD");
        let registrations =
            git::registrations_from_metadata(&fixture.repo, Duration::from_secs(30)).expect("read");
        let registration = registrations
            .iter()
            .find(|registration| registration.path == linked)
            .unwrap_or_else(|| panic!("listed: {registrations:?}"));
        assert_eq!(registration.head, git::Head::Unknown, "{garbage:?}");
    }

    // Without force, a missing checkout whose HEAD is unknown keeps its
    // registration.
    git::FORCE_METADATA_FALLBACK.with(|forced| forced.set(true));
    let target = entry(&linked, &fixture.repo, Some("unknown-head-branch"), false);
    let outcome = clear_one(&target, false, &fixture.data_dir());
    git::FORCE_METADATA_FALLBACK.with(|forced| forced.set(false));
    assert_eq!(
        outcome.err().as_deref(),
        Some("could not read the registration's HEAD")
    );
    assert!(admin.is_dir(), "the registration stays");
}

#[test]
fn an_assume_unchanged_edit_after_preflight_is_kept() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-hidden", "hidden-branch");
    let target = entry(&linked, &fixture.repo, Some("hidden-branch"), false);
    preflight(&target, false, &Default::default()).expect("preflight");

    run(
        &linked,
        &["update-index", "--assume-unchanged", "README.md"],
    );
    std::fs::write(linked.join("README.md"), "hidden edit").expect("edit");
    assert_eq!(git::is_dirty(&linked), Some(false), "status hides the edit");
    assert_eq!(
        prepare(&target, false, &Default::default())
            .err()
            .as_deref(),
        Some("has files hidden from git status")
    );
    assert_eq!(
        std::fs::read_to_string(linked.join("README.md")).expect("edit kept"),
        "hidden edit"
    );

    // A skip-worktree file that is absent on disk (a sparse checkout) hides
    // nothing.
    let sparse = fixture.add_worktree("wt-sparse", "sparse-branch");
    run(&sparse, &["update-index", "--skip-worktree", "README.md"]);
    std::fs::remove_file(sparse.join("README.md")).expect("sparse file");
    let sparse_entry = entry(&sparse, &fixture.repo, Some("sparse-branch"), false);
    assert!(matches!(
        clear_one(&sparse_entry, false, &fixture.data_dir()),
        Ok(ClearOne::Removed)
    ));
}

#[test]
fn a_nested_worktree_registered_just_before_execute_is_detected() {
    let fixture = Fixture::new();
    ignore(&fixture, ".claude/");
    let parent = fixture.add_worktree("parent-exec-nested", "parent-exec-branch");
    let target = entry(&parent, &fixture.repo, Some("parent-exec-branch"), false);
    let plan = preflight(&target, false, &Default::default()).expect("preflight");
    let prepared = prepare(&target, false, &Default::default()).expect("prepare");

    // Between the live activity check and the destructive call.
    let child = parent.join(".claude/worktrees/exec-child");
    std::fs::create_dir_all(child.parent().expect("child parent")).expect("child dir");
    run(
        &parent,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "exec-child-branch",
            &child.to_string_lossy(),
        ],
    );
    let outcome = execute(
        &target,
        false,
        &fixture.data_dir(),
        &plan,
        &prepared,
        &Default::default(),
    );
    let Err(reason) = outcome else {
        panic!("a nested worktree registered before execute is never removed");
    };
    assert!(
        reason.starts_with("contains another worktree at "),
        "{reason}"
    );
    assert!(parent.is_dir() && child.is_dir(), "both checkouts are kept");
}

#[cfg(unix)]
#[test]
fn a_missing_owned_row_is_kept_while_git_cannot_answer() {
    let fixture = Fixture::new();
    let path = fixture
        .root
        .path()
        .join("data/subagents/sa_unknown/worktree");
    let view = subagent_view(
        "sa_unknown",
        "th_parent",
        &path,
        "h4rn/sa-unknown",
        SubagentState::Completed,
    );
    let list_input = input(&fixture, vec![view]);
    // With git answering, nothing is left: no directory, branch or
    // registration.
    let (entries, _) = compute(&list_input, false);
    assert!(entries.is_empty(), "{entries:?}");

    let tools = tempfile::tempdir().expect("tools");
    let failing = |name: &str, command: &str| {
        let dir = tools.path().join(name);
        std::fs::create_dir_all(&dir).expect("dir");
        fake_git(
            &dir,
            &format!(
                r#"for arg in "$@"; do
  if [ "$arg" = "{command}" ]; then echo "fatal: cannot answer" >&2; exit 128; fi
done"#
            ),
        )
    };
    for program in [failing("refs", "for-each-ref"), failing("list", "worktree")] {
        git::GIT_PROGRAM.with(|slot| *slot.borrow_mut() = Some(program));
        let (entries, _) = compute(&list_input, false);
        git::GIT_PROGRAM.with(|slot| *slot.borrow_mut() = None);
        assert_eq!(entries.len(), 1, "the row is kept: {entries:?}");
        assert!(!entries[0].exists);
        assert_eq!(entries[0].branch.as_deref(), Some("h4rn/sa-unknown"));
    }
}

#[test]
fn a_broken_branch_ref_is_an_error_not_absent() {
    let fixture = Fixture::new();
    let refs = fixture.repo.join(".git/refs/heads/h4rn");
    std::fs::create_dir_all(&refs).expect("refs dir");
    std::fs::write(refs.join("sa-broken"), "garbage\n").expect("broken ref");

    let error = branch_tip(&fixture.repo, "h4rn/sa-broken").expect_err("inconclusive");
    assert!(error.contains("broken ref"), "{error}");
    assert!(git::h4rn_branches(&fixture.repo).is_err());

    let missing = fixture.root.path().join("sa-broken-gone");
    let mut own = entry(&missing, &fixture.repo, Some("h4rn/sa-broken"), true);
    own.dirty = None;
    let outcome = clear_one(&own, false, &fixture.data_dir());
    assert!(
        matches!(&outcome, Err(reason) if reason.starts_with("branch h4rn/sa-broken could not be checked: ")),
        "{outcome:?}"
    );
    assert_eq!(
        std::fs::read_to_string(refs.join("sa-broken")).expect("ref kept"),
        "garbage\n"
    );
}

#[test]
fn the_nested_scan_deadline_is_checked_per_entry() {
    let fixture = Fixture::new();
    let common = git::common_dir(&fixture.repo, Duration::from_secs(30)).expect("common");
    let many = fixture.root.path().join("many");
    std::fs::create_dir_all(&many).expect("dir");
    for index in 0..500 {
        std::fs::write(many.join(format!("file-{index}")), "x").expect("file");
    }
    assert_eq!(
        nested_checkout(&many, &common, Duration::ZERO),
        Err(NESTED_SCAN_UNKNOWN.to_owned())
    );
    // Even with nothing to enumerate, "clean" is only answered in time.
    let empty = fixture.root.path().join("empty");
    std::fs::create_dir_all(&empty).expect("dir");
    assert_eq!(
        nested_checkout(&empty, &common, Duration::ZERO),
        Err(NESTED_SCAN_UNKNOWN.to_owned())
    );
    assert_eq!(
        nested_checkout(&many, &common, Duration::from_secs(30)),
        Ok(None)
    );

    // Through a clear, with a test-only short deadline.
    let linked = fixture.add_worktree("wt-many", "many-branch");
    for index in 0..500 {
        std::fs::write(linked.join(format!("file-{index}")), "x").expect("file");
    }
    let target = entry(&linked, &fixture.repo, Some("many-branch"), false);
    NESTED_SCAN_LIMIT.with(|limit| limit.set(Some(Duration::ZERO)));
    let outcome = clear_one(&target, true, &fixture.data_dir());
    NESTED_SCAN_LIMIT.with(|limit| limit.set(None));
    assert_eq!(outcome.err().as_deref(), Some(NESTED_SCAN_UNKNOWN));
    assert!(linked.is_dir());
}

#[test]
fn the_metadata_fallback_lists_a_bare_repository_like_porcelain() {
    let fixture = Fixture::new();
    run(
        fixture.root.path(),
        &[
            "clone",
            "-q",
            "--bare",
            &fixture.repo.to_string_lossy(),
            "bare.git",
        ],
    );
    let bare = git::norm(&fixture.root.path().join("bare.git"));
    let linked = fixture.root.path().join("bare-wt");
    run(
        &bare,
        &["worktree", "add", "-q", &linked.to_string_lossy(), "main"],
    );
    let registrations =
        git::registrations_from_metadata(&bare, Duration::from_secs(30)).expect("read");
    assert!(
        registrations.iter().any(
            |registration| registration.path == bare && registration.head == git::Head::Unknown
        ),
        "{registrations:?}"
    );
    assert_metadata_matches_porcelain(&bare);
}

#[test]
fn a_checkout_that_reappears_before_execute_gets_every_check() {
    let fixture = Fixture::new();
    ignore(&fixture, "vendor/");
    let linked = fixture.add_worktree("wt-reappear", "reappear-branch");
    let away = fixture.root.path().join("wt-reappear-away");
    std::fs::rename(&linked, &away).expect("move away");
    let target = entry(&linked, &fixture.repo, Some("reappear-branch"), false);
    let plan = preflight(&target, false, &Default::default()).expect("preflight");
    let prepared = prepare(&target, false, &Default::default()).expect("prepare saw it missing");

    // By execute the checkout is back, now holding another repository's clone.
    std::fs::rename(&away, &linked).expect("move back");
    let clone = linked.join("vendor/other");
    init_repo(&clone);
    assert_eq!(
        execute(
            &target,
            false,
            &fixture.data_dir(),
            &plan,
            &prepared,
            &Default::default()
        )
        .err()
        .as_deref(),
        Some(
            format!(
                "contains another git checkout at {}",
                git::norm(&clone).display()
            )
            .as_str()
        )
    );
    assert!(linked.is_dir() && clone.join(".git").is_dir());
    assert!(
        git::registrations(&fixture.repo)
            .expect("list")
            .iter()
            .any(|registration| registration.path == linked),
        "the registration stays"
    );
}

#[test]
fn an_assume_unchanged_edit_just_before_execute_is_kept() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-hidden-exec", "hidden-exec-branch");
    let target = entry(&linked, &fixture.repo, Some("hidden-exec-branch"), false);
    let plan = preflight(&target, false, &Default::default()).expect("preflight");
    let prepared = prepare(&target, false, &Default::default()).expect("prepare");

    // Between the live activity check and the destructive call.
    run(
        &linked,
        &["update-index", "--assume-unchanged", "README.md"],
    );
    std::fs::write(linked.join("README.md"), "hidden late edit").expect("edit");
    assert_eq!(git::is_dirty(&linked), Some(false), "status hides the edit");
    assert_eq!(
        execute(
            &target,
            false,
            &fixture.data_dir(),
            &plan,
            &prepared,
            &Default::default()
        )
        .err()
        .as_deref(),
        Some("has files hidden from git status")
    );
    assert_eq!(
        std::fs::read_to_string(linked.join("README.md")).expect("edit kept"),
        "hidden late edit"
    );
}

#[test]
fn a_missing_registration_whose_last_ref_goes_before_execute_is_kept() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-temp-ref", "temp-ref-base");
    run(&linked, &["checkout", "-q", "--detach"]);
    run(
        &linked,
        &["commit", "-q", "--allow-empty", "-m", "detached work"],
    );
    // A temporary branch reaches the detached commit while prepare looks.
    run(&linked, &["branch", "temp-ref"]);
    std::fs::remove_dir_all(&linked).expect("remove the checkout");
    let target = entry(&linked, &fixture.repo, None, false);
    let plan = preflight(&target, false, &Default::default()).expect("preflight");
    let prepared = prepare(&target, false, &Default::default()).expect("prepare");

    // Gone again before the destructive step.
    run(&fixture.repo, &["branch", "-D", "temp-ref"]);
    assert_eq!(
        execute(
            &target,
            false,
            &fixture.data_dir(),
            &plan,
            &prepared,
            &Default::default()
        )
        .err()
        .as_deref(),
        Some("detached HEAD has commits on no branch")
    );
    let registrations = git::registrations(&fixture.repo).expect("list");
    assert!(
        registrations
            .iter()
            .any(|registration| registration.path == linked),
        "the registration stays: {registrations:?}"
    );
}

#[cfg(unix)]
#[test]
fn an_owned_branch_checked_out_at_a_non_utf8_path_is_not_deleted() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    for fallback in [false, true] {
        let fixture = Fixture::new();
        let branch = "h4rn/sa-rawpath";
        run(&fixture.repo, &["branch", branch]);
        let holder = fixture
            .root
            .path()
            .join(OsString::from_vec(b"holder-\xfe".to_vec()));
        let output = std::process::Command::new("git")
            .args(["worktree", "add", "-q"])
            .arg(&holder)
            .arg(branch)
            .current_dir(&fixture.repo)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let missing = git::norm(&fixture.root.path().join("sa-rawpath-gone"));
        let own = entry(&missing, &fixture.repo, Some(branch), true);

        git::FORCE_METADATA_FALLBACK.with(|forced| forced.set(fallback));
        let registrations = git::registrations(&fixture.repo);
        let outcome = clear_one(&own, false, &fixture.data_dir());
        git::FORCE_METADATA_FALLBACK.with(|forced| forced.set(false));

        assert!(
            registrations
                .expect("registrations")
                .iter()
                .any(|registration| registration.path == git::norm(&holder)),
            "fallback={fallback}: the raw path is in the snapshot"
        );
        assert!(
            matches!(
                &outcome,
                Ok(ClearOne::RemovedWithBranchFailure(reason)) if reason.starts_with(
                    "checkout removed, but branch h4rn/sa-rawpath could not be deleted: \
                     it is checked out in "
                )
            ),
            "fallback={fallback}: {outcome:?}"
        );
        assert!(
            git::branch_exists(&fixture.repo, branch),
            "fallback={fallback}"
        );
        assert!(holder.is_dir());
    }
}

#[test]
fn only_a_real_submodule_checkout_is_exempt_from_the_nested_scan() {
    let fixture = Fixture::new();
    ignore(&fixture, "vendor/");
    let upstream = fixture.root.path().join("upstream-wt");
    init_repo(&upstream);
    let host = fixture.add_worktree("wt-sub-wt", "sub-wt-branch");
    run(
        &host,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            &upstream.to_string_lossy(),
            "sub",
        ],
    );
    run(&host, &["commit", "-q", "-m", "add a submodule"]);
    let target = entry(&host, &fixture.repo, Some("sub-wt-branch"), false);
    let refused = |nested: &Path| {
        for force in [false, true] {
            assert_eq!(
                clear_one(&target, force, &fixture.data_dir())
                    .err()
                    .as_deref(),
                Some(
                    format!(
                        "contains another git checkout at {}",
                        git::norm(nested).display()
                    )
                    .as_str()
                ),
                "force={force}"
            );
            assert!(host.join("sub/.git").is_file() && nested.join(".git").is_file());
        }
    };

    // A linked worktree of the submodule, inside the checkout: its gitdir is
    // under `modules/` but names `worktrees` after it, and it is no gitlink.
    let sub_worktree = host.join("vendor/sub-worktree");
    run(
        &host.join("sub"),
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            &sub_worktree.to_string_lossy(),
        ],
    );
    assert!(sub_worktree.join(".git").is_file());
    refused(&sub_worktree);
    run(
        &host.join("sub"),
        &["worktree", "remove", &sub_worktree.to_string_lossy()],
    );

    // A gitfile pointing at the submodule's own repository from a path that
    // is not a gitlink in the index.
    let target_dir = git::gitfile_target(&host.join("sub/.git")).expect("submodule gitdir");
    let copy = host.join("vendor/copy");
    std::fs::create_dir_all(&copy).expect("copy dir");
    std::fs::write(
        copy.join(".git"),
        format!("gitdir: {}\n", target_dir.display()),
    )
    .expect("gitfile");
    refused(&copy);
    std::fs::remove_dir_all(&copy).expect("remove the copy");

    // The real submodule alone is git's forceable refusal.
    assert_eq!(
        clear_one(&target, false, &fixture.data_dir())
            .err()
            .as_deref(),
        Some("contains submodules")
    );
}

#[test]
fn a_submodule_of_a_submodule_is_judged_by_its_own_superproject() {
    let fixture = Fixture::new();
    // `inner` is a submodule of `middle`, which is a submodule of the host.
    let inner = fixture.root.path().join("inner-up");
    init_repo(&inner);
    let middle = fixture.root.path().join("middle-up");
    init_repo(&middle);
    run(
        &middle,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            &inner.to_string_lossy(),
            "inner",
        ],
    );
    run(
        &middle,
        &[
            "-c",
            "user.email=wt@h4rn.invalid",
            "-c",
            "user.name=wt",
            "commit",
            "-q",
            "-m",
            "add inner",
        ],
    );
    let host = fixture.add_worktree("wt-recursive", "recursive-branch");
    run(
        &host,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            &middle.to_string_lossy(),
            "middle",
        ],
    );
    run(
        &host,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "-q",
            "--init",
            "--recursive",
        ],
    );
    run(&host, &["commit", "-q", "-m", "add middle"]);
    assert!(
        host.join("middle/inner/.git").is_file(),
        "an initialised nested submodule"
    );
    let target = entry(&host, &fixture.repo, Some("recursive-branch"), false);

    // Nested submodules are this repository's own: git's forceable refusal,
    // never the foreign-checkout refusal.
    assert_eq!(
        clear_one(&target, false, &fixture.data_dir())
            .err()
            .as_deref(),
        Some("contains submodules")
    );
    assert!(host.join("middle/inner/.git").is_file());
    assert!(matches!(
        clear_one(&target, true, &fixture.data_dir()),
        Ok(ClearOne::Removed)
    ));
    assert!(!host.exists());
}

#[test]
fn a_gitfile_larger_than_four_kib_is_not_read() {
    let dir = tempfile::tempdir().expect("dir");
    let file = dir.path().join(".git");
    let target = dir.path().join("target");
    std::fs::write(&file, format!("gitdir: {}\n", target.display())).expect("gitfile");
    assert_eq!(git::gitfile_target(&file), Some(git::norm(&target)));
    let padded = format!("gitdir: {}{}\n", target.display(), "/x".repeat(4096));
    std::fs::write(&file, padded).expect("large gitfile");
    assert_eq!(git::gitfile_target(&file), None);
}

#[test]
fn a_recreated_owned_checkout_without_a_branch_is_refused_even_with_force() {
    for force in [false, true] {
        let fixture = Fixture::new();
        let path = git::norm(&fixture.root.path().join("sa-recreated"));
        let own = entry(&path, &fixture.repo, None, true);
        let plan = preflight(&own, force, &Default::default()).expect("preflight");
        let prepared = prepare(&own, force, &Default::default()).expect("prepare saw it missing");

        // Someone else's checkout at the same path, before execute.
        run(
            &fixture.repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "someone-else",
                &path.to_string_lossy(),
            ],
        );
        assert_eq!(
            execute(
                &own,
                force,
                &fixture.data_dir(),
                &plan,
                &prepared,
                &Default::default()
            )
            .err()
            .as_deref(),
            Some(RECREATED),
            "force={force}"
        );
        assert!(path.is_dir(), "force={force}");
        assert!(git::branch_exists(&fixture.repo, "someone-else"));

        // Recreated before prepare: the selected row said nothing was there.
        let fixture = Fixture::new();
        let path = git::norm(&fixture.root.path().join("sa-recreated-early"));
        let own = entry(&path, &fixture.repo, None, true);
        preflight(&own, force, &Default::default()).expect("preflight");
        run(
            &fixture.repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "someone-else-early",
                &path.to_string_lossy(),
            ],
        );
        assert_eq!(
            prepare(&own, force, &Default::default()).err().as_deref(),
            Some(RECREATED),
            "force={force}"
        );
        assert!(path.is_dir(), "force={force}");
        assert!(git::branch_exists(&fixture.repo, "someone-else-early"));
    }
}

#[test]
fn an_owned_row_whose_repository_cannot_be_found_is_kept() {
    let fixture = Fixture::new();
    let path = fixture.root.path().join("data/subagents/sa_lost/worktree");
    let view = subagent_view(
        "sa_lost",
        "th_parent",
        &path,
        "h4rn/sa-lost",
        SubagentState::Completed,
    );
    let mut list_input = input(&fixture, vec![view]);
    // The parent's workspace is gone too, and no repository was stored.
    list_input.parent_workspaces.insert(
        "th_parent".into(),
        fixture
            .root
            .path()
            .join("gone-parent")
            .to_string_lossy()
            .into_owned(),
    );
    let (entries, _) = compute(&list_input, false);
    assert_eq!(entries.len(), 1, "the row is kept: {entries:?}");
    let row = &entries[0];
    assert!(!row.exists);
    assert_eq!(row.repo, "");
    assert_eq!(row.branch.as_deref(), Some("h4rn/sa-lost"));
    for force in [false, true] {
        assert_eq!(
            preflight(row, force, &Default::default()).err().as_deref(),
            Some("its repository could not be found")
        );
    }
}

/// Every variable that makes git trace, set for a re-run of this binary.
const GIT_TRACE_VARS: &[&str] = &[
    "GIT_TRACE",
    "GIT_TRACE2",
    "GIT_TRACE2_EVENT",
    "GIT_TRACE2_PERF",
    "GIT_TRACE_PACKET",
    "GIT_TRACE_PERFORMANCE",
    "GIT_TRACE_SETUP",
    "GIT_TRACE_REFS",
    "GIT_TRACE_FSMONITOR",
    "GIT_TRACE_SHALLOW",
    "GIT_CURL_VERBOSE",
];

/// Re-runs [`branch_lookups_under_git_trace`] in a child process whose
/// environment sets every trace variable, so this process's environment is
/// never changed under its other tests.
#[test]
fn trace_variables_do_not_make_lookups_inconclusive() {
    let mut command = std::process::Command::new(std::env::current_exe().expect("test binary"));
    command.args([
        "--exact",
        "worktrees::tests::branch_lookups_under_git_trace",
        "--ignored",
        "--test-threads=1",
    ]);
    for name in GIT_TRACE_VARS {
        command.env(name, "1");
    }
    let output = command.output().expect("run the traced test");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "run by trace_variables_do_not_make_lookups_inconclusive with GIT_TRACE set"]
fn branch_lookups_under_git_trace() {
    assert!(
        std::env::var_os("GIT_TRACE").is_some(),
        "run through trace_variables_do_not_make_lookups_inconclusive"
    );
    let fixture = Fixture::new();
    run(&fixture.repo, &["branch", "h4rn/sa-traced"]);
    // The environment really does make a plain git call trace.
    let traced = std::process::Command::new("git")
        .args(["for-each-ref", "refs/heads/h4rn/"])
        .current_dir(&fixture.repo)
        .output()
        .expect("run git");
    assert!(
        !traced.stderr.is_empty(),
        "git traces under this environment"
    );

    let branches = git::h4rn_branches(&fixture.repo).expect("branch lookup");
    assert!(branches.contains("h4rn/sa-traced"), "{branches:?}");
    assert!(
        branch_tip(&fixture.repo, "h4rn/sa-traced")
            .expect("tip lookup")
            .is_some()
    );
}

#[test]
fn an_inherited_index_file_does_not_hide_staged_work() {
    // A real, clean index: every fixture commits the same README, so this
    // index matches the child's HEAD tree exactly. An unstripped
    // `diff-index --cached` reading it would answer "clean".
    let clean = Fixture::new();
    let mut command = std::process::Command::new(std::env::current_exe().expect("test binary"));
    command.args([
        "--exact",
        "worktrees::tests::staged_check_under_an_inherited_index_file",
        "--ignored",
        "--test-threads=1",
    ]);
    command.env("GIT_INDEX_FILE", clean.repo.join(".git/index"));
    let output = command.output().expect("run the child test");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "run by an_inherited_index_file_does_not_hide_staged_work with GIT_INDEX_FILE set"]
fn staged_check_under_an_inherited_index_file() {
    let inherited = std::env::var_os("GIT_INDEX_FILE")
        .expect("run through an_inherited_index_file_does_not_hide_staged_work");
    // The fixture helpers run plain git, so build the scenario without the
    // inherited index. This test runs alone in its own process.
    // SAFETY: single-threaded child process (`--test-threads=1`).
    unsafe { std::env::remove_var("GIT_INDEX_FILE") };
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-inherited-index", "inherited-index-branch");
    let admin =
        PathBuf::from(git::run(&linked, ["rev-parse", "--absolute-git-dir"]).expect("admin dir"));
    std::fs::write(linked.join("README.md"), "staged change\n").expect("edit");
    run(&linked, &["add", "README.md"]);
    assert!(
        admin.join("index").is_file(),
        "the worktree's own index exists"
    );
    // SAFETY: as above.
    unsafe { std::env::set_var("GIT_INDEX_FILE", &inherited) };
    assert!(
        !git::index_matches(
            &admin,
            "refs/heads/inherited-index-branch",
            Duration::from_secs(30)
        )
        .expect("index check"),
        "the worktree's own index holds staged work, whatever the daemon inherited"
    );
}

/// An fsmonitor hook that reports nothing changed: it prints its token and no
/// paths, so git trusts its index for every file.
#[cfg(unix)]
fn silent_fsmonitor(fixture: &Fixture) {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let hook = fixture.root.path().join("fsmonitor-hook");
    {
        let mut file = std::fs::File::create(&hook).expect("hook");
        file.write_all(b"#!/bin/sh\nprintf 'token\\0'\n")
            .expect("write hook");
        file.sync_all().expect("sync hook");
    }
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    run(
        &fixture.repo,
        &["config", "core.fsmonitor", &hook.to_string_lossy()],
    );
}

/// Primes `linked`'s index under the hook, edits a tracked file, and checks
/// that a plain `git status` (as the tracker runs it, without index writes)
/// really does not see the edit.
#[cfg(unix)]
fn edit_behind_fsmonitor(linked: &Path, text: &str) {
    run(linked, &["status", "--porcelain"]);
    run(linked, &["status", "--porcelain"]);
    std::fs::write(linked.join("README.md"), text).expect("edit");
    let plain = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .current_dir(linked)
        .output()
        .expect("run git");
    assert!(plain.status.success());
    assert!(
        plain.stdout.is_empty(),
        "the hook hides the edit from a plain status: {}",
        String::from_utf8_lossy(&plain.stdout)
    );
}

#[cfg(unix)]
#[test]
fn a_filesystem_monitor_that_reports_nothing_hides_no_edit() {
    let fixture = Fixture::new();
    silent_fsmonitor(&fixture);

    // Preflight's own dirty check.
    let linked = fixture.add_worktree("wt-fsmonitor", "fsmonitor-branch");
    edit_behind_fsmonitor(&linked, "edited behind the monitor");
    assert_eq!(git::is_dirty(&linked), Some(true));
    let target = entry(&linked, &fixture.repo, Some("fsmonitor-branch"), false);
    assert_eq!(
        preflight(&target, false, &Default::default())
            .err()
            .as_deref(),
        Some("has uncommitted changes")
    );

    // Git's removal backstop, for an edit made after prepare.
    let late = fixture.add_worktree("wt-fsmonitor-late", "fsmonitor-late-branch");
    let target = entry(&late, &fixture.repo, Some("fsmonitor-late-branch"), false);
    let plan = preflight(&target, false, &Default::default()).expect("preflight");
    let prepared = prepare(&target, false, &Default::default()).expect("prepare");
    edit_behind_fsmonitor(&late, "late edit behind the monitor");
    assert_eq!(
        execute(
            &target,
            false,
            &fixture.data_dir(),
            &plan,
            &prepared,
            &Default::default()
        )
        .err()
        .as_deref(),
        Some("has uncommitted changes")
    );
    assert_eq!(
        std::fs::read_to_string(late.join("README.md")).expect("edit kept"),
        "late edit behind the monitor"
    );
}

#[test]
fn a_deleted_assume_unchanged_file_blocks_an_ordinary_clear() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-hidden-delete", "hidden-delete-branch");
    run(
        &linked,
        &["update-index", "--assume-unchanged", "README.md"],
    );
    std::fs::remove_file(linked.join("README.md")).expect("delete");
    assert_eq!(
        git::is_dirty(&linked),
        Some(false),
        "status hides the deletion"
    );
    let target = entry(&linked, &fixture.repo, Some("hidden-delete-branch"), false);
    assert_eq!(
        clear_one(&target, false, &fixture.data_dir())
            .err()
            .as_deref(),
        Some("has files hidden from git status")
    );
    assert!(linked.is_dir());
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_that_replaced_a_tracked_checkout_is_never_followed() {
    let fixture = Fixture::new();
    let tracked = git::norm(&fixture.root.path().join("wt-tracked"));
    let other = git::norm(&fixture.root.path().join("wt-other"));
    for path in [&tracked, &other] {
        run(
            &fixture.repo,
            &["worktree", "add", "-q", "--detach", &path.to_string_lossy()],
        );
    }
    std::fs::write(other.join("work.txt"), "the other checkout's work").expect("work");
    let (tracker, _threads, _subagents) = managers(&fixture, Vec::new()).await;
    tracker
        .inner
        .store
        .record(
            &tracked.to_string_lossy(),
            &fixture.repo.to_string_lossy(),
            None,
            "th_gone",
            None,
            Utc::now(),
        )
        .await
        .expect("record the observation");

    // The tracked checkout moves away, and its old path becomes a symlink to
    // a different, untracked worktree of the same repository.
    std::fs::rename(&tracked, fixture.root.path().join("wt-tracked-away")).expect("move away");
    std::os::unix::fs::symlink(&other, &tracked).expect("symlink");

    let listed = tracker.list().await;
    let row = listed
        .iter()
        .find(|entry| Path::new(&entry.path) == tracked)
        .unwrap_or_else(|| panic!("listed under its stored path: {listed:?}"));
    assert!(!row.exists, "a symlink is not a checkout");
    assert!(listed.iter().all(|entry| Path::new(&entry.path) != other));

    for force in [false, true] {
        let outcome = tracker
            .clear(
                Some(vec![tracked.to_string_lossy().into_owned()]),
                force,
                false,
            )
            .await;
        assert!(outcome.removed.is_empty(), "force={force}: {outcome:?}");
        assert_eq!(outcome.skipped.len(), 1, "force={force}");
        assert_eq!(outcome.skipped[0].reason, "not a registered worktree");
    }
    let outcome = tracker.clear(None, true, false).await;
    assert!(outcome.removed.is_empty(), "{outcome:?}");

    assert!(
        tracked
            .symlink_metadata()
            .expect("still there")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_to_string(other.join("work.txt")).expect("work kept"),
        "the other checkout's work"
    );
    assert!(
        git::registrations(&fixture.repo)
            .expect("list")
            .iter()
            .any(|registration| registration.path == other),
        "the other worktree stays registered"
    );
}

#[test]
fn an_existing_directory_that_is_not_registered_is_not_removed() {
    let fixture = Fixture::new();
    let clone = git::norm(&fixture.root.path().join("not-registered"));
    init_repo(&clone);
    let target = entry(&clone, &fixture.repo, None, false);
    for force in [false, true] {
        assert_eq!(
            clear_one(&target, force, &fixture.data_dir())
                .err()
                .as_deref(),
            Some("not a registered worktree"),
            "force={force}"
        );
    }
    assert!(clone.join(".git").is_dir());
}

/// One unrelated thread working at each of `paths`.
fn workspaces(paths: &[PathBuf]) -> Vec<ThreadWorkspace> {
    paths
        .iter()
        .map(|path| ThreadWorkspace {
            thread_id: ThreadId::new("th_other"),
            forms: vec![path.clone()],
        })
        .collect()
}

#[test]
fn clear_all_leaves_a_thread_workspace_alone_whatever_its_origin() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-thread-harness", "thread-harness-branch");
    let nested = linked.join("src");
    std::fs::create_dir_all(&nested).expect("nested workspace");
    let mut list_input = input(&fixture, Vec::new());
    list_input.threads = vec![ThreadInfo {
        thread_id: ThreadId::new("th_idle"),
        title: "idle".into(),
        workspace: nested.to_string_lossy().into_owned(),
        working: false,
        last_activity: Utc::now(),
    }];
    let (_, _, thread_workspaces) = compute_listing(&list_input, false);
    assert_eq!(thread_workspaces.len(), 1);
    assert!(thread_workspaces[0].forms.contains(&git::norm(&nested)));
    assert!(
        thread_workspaces[0].forms.contains(&linked),
        "the workspace's checkout top level: {thread_workspaces:?}"
    );

    // A harness-origin row at that path is still a thread's workspace.
    let harness = entry(&linked, &fixture.repo, Some("thread-harness-branch"), false);
    assert_eq!(harness.origin, WorktreeOrigin::Harness);
    let entries = vec![harness];
    let selection = select_entries(&entries, None, false, &thread_workspaces);
    assert!(selection.targets.is_empty());
    assert_eq!(selection.skipped.len(), 1, "reported, not dropped");
    assert_eq!(selection.skipped[0].path, entries[0].path);
    assert_eq!(selection.skipped[0].reason, "is a thread workspace");
    let selection = select_entries(&entries, None, false, &[]);
    assert_eq!(selection.targets.len(), 1);
    // A workspace anywhere inside the row protects it.
    let gone = workspaces(&[linked.join("gone").join("deeper")]);
    let selection = select_entries(&entries, None, false, &gone);
    assert!(selection.targets.is_empty());
    assert_eq!(selection.skipped[0].reason, "is a thread workspace");
    // Only component-wise: a sibling whose name merely starts the same does
    // not.
    let sibling = workspaces(&[PathBuf::from(format!("{}-sibling", linked.display()))]);
    let selection = select_entries(&entries, None, false, &sibling);
    assert_eq!(selection.targets.len(), 1);
    // Naming it still selects it.
    let selection = select_entries(
        &entries,
        Some(std::slice::from_ref(&entries[0].path)),
        false,
        &thread_workspaces,
    );
    assert_eq!(selection.targets.len(), 1);
    // Clear all sends the paths it showed with `bulk`: the same exclusion
    // applies to them, and the row is reported as skipped.
    let selection = select_entries(
        &entries,
        Some(std::slice::from_ref(&entries[0].path)),
        true,
        &thread_workspaces,
    );
    assert!(selection.targets.is_empty());
    assert_eq!(selection.skipped.len(), 1);
    assert_eq!(selection.skipped[0].reason, "is a thread workspace");
    let mut thread_row = entries[0].clone();
    thread_row.origin = WorktreeOrigin::Thread;
    let thread_rows = vec![thread_row];
    let selection = select_entries(
        &thread_rows,
        Some(std::slice::from_ref(&thread_rows[0].path)),
        true,
        &[],
    );
    assert!(
        selection.targets.is_empty(),
        "thread-origin rows are excluded too"
    );
    assert_eq!(selection.skipped[0].reason, "is a thread workspace");
}

#[test]
fn oversized_registration_metadata_is_not_read() {
    let fixture = Fixture::new();
    let big_head = fixture.add_worktree("wt-big-head", "big-head-branch");
    let big_gitdir = fixture.add_worktree("wt-big-gitdir", "big-gitdir-branch");
    let admin = |name: &str| fixture.repo.join(".git/worktrees").join(name);
    std::fs::write(
        admin("wt-big-head").join("HEAD"),
        format!("ref: refs/heads/big-head-branch{}\n", " ".repeat(4096)),
    )
    .expect("big HEAD");
    std::fs::write(
        admin("wt-big-gitdir").join("gitdir"),
        format!("{}/.git{}\n", big_gitdir.display(), "/".repeat(4096)),
    )
    .expect("big gitdir");

    let registrations =
        git::registrations_from_metadata(&fixture.repo, Duration::from_secs(30)).expect("read");
    let head = registrations
        .iter()
        .find(|registration| registration.path == big_head)
        .unwrap_or_else(|| panic!("listed: {registrations:?}"));
    assert_eq!(head.head, git::Head::Unknown);
    assert!(
        registrations
            .iter()
            .all(|registration| registration.path != big_gitdir),
        "{registrations:?}"
    );
}

/// Waits until the thread has finished its turn and is no longer busy.
async fn wait_idle(threads: &ThreadManager, thread: &ThreadId) {
    for _ in 0..250 {
        if let Some(summary) = threads.thread_summary(thread).await
            && !is_busy(
                summary.tab,
                summary.agent_state,
                threads.turn_in_flight(thread).await,
            )
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("thread {thread:?} never settled");
}

#[cfg(unix)]
#[tokio::test]
async fn an_owned_row_whose_recorded_path_became_a_symlink_is_never_followed() {
    let fixture = Fixture::new();
    let other = fixture.add_worktree("wt-owned-target", "owned-target-branch");
    std::fs::write(other.join("work.txt"), "the other checkout's work").expect("work");
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
            task: "settle and leave the branch".into(),
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
    assert!(!settled.exists);
    let recorded = PathBuf::from(&view.workspace_path);
    assert_eq!(Path::new(&settled.path), recorded, "listed as recorded");

    // The recorded path becomes a symlink to another registered worktree.
    std::fs::create_dir_all(recorded.parent().expect("parent")).expect("parent dir");
    std::os::unix::fs::symlink(&other, &recorded).expect("symlink");

    let listed = tracker.list().await;
    let row = listed
        .iter()
        .find(|entry| entry.subagent_id.as_ref() == Some(&view.subagent_id))
        .unwrap_or_else(|| panic!("the owned row is listed: {listed:?}"))
        .clone();
    assert_eq!(Path::new(&row.path), recorded, "never the symlink's target");
    assert!(!row.exists, "a symlink is not a checkout");
    // The child thread's workspace resolves through the symlink, so the target
    // may show up as that thread's workspace, but never as an owned row.
    assert!(
        listed
            .iter()
            .all(|entry| !(entry.owned && Path::new(&entry.path) == other)),
        "{listed:?}"
    );

    for (force, bulk) in [(false, false), (true, false), (false, true), (true, true)] {
        let outcome = tracker
            .clear(Some(vec![row.path.clone()]), force, bulk)
            .await;
        assert!(
            outcome.removed.is_empty(),
            "force={force} bulk={bulk}: {outcome:?}"
        );
        assert_eq!(outcome.skipped.len(), 1, "force={force} bulk={bulk}");
        assert_eq!(outcome.skipped[0].reason, "not a registered worktree");
    }
    let outcome = tracker.clear(None, true, false).await;
    assert!(outcome.removed.is_empty(), "{outcome:?}");

    assert!(
        recorded
            .symlink_metadata()
            .expect("still there")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_to_string(other.join("work.txt")).expect("work kept"),
        "the other checkout's work"
    );
    assert!(
        git::registrations(&fixture.repo)
            .expect("list")
            .iter()
            .any(|registration| registration.path == other),
        "the other worktree stays registered"
    );
    assert!(
        git::branch_exists(&fixture.repo, "owned-target-branch"),
        "the target's branch is untouched"
    );
    let _ = threads.interrupt(&parent).await;
}

#[tokio::test]
async fn a_bulk_clear_skips_a_row_that_became_a_thread_workspace() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-bulk", "bulk-branch");
    let (tracker, threads, _subagents) =
        managers(&fixture, vec![Arc::new(FakeHarness::new())]).await;
    tracker
        .inner
        .store
        .record(
            &linked.to_string_lossy(),
            &fixture.repo.to_string_lossy(),
            Some("bulk-branch"),
            "th_gone",
            None,
            Utc::now(),
        )
        .await
        .expect("record the observation");

    // Settings shows a harness row and captures its path for Clear all.
    let listed = tracker.list().await;
    let row = listed
        .iter()
        .find(|entry| Path::new(&entry.path) == linked)
        .unwrap_or_else(|| panic!("listed: {listed:?}"))
        .clone();
    assert_eq!(row.origin, WorktreeOrigin::Harness);

    // Behind the dialog, a thread starts in it and settles. The fresh listing
    // the clear selects from now has the row at that path as the thread's
    // workspace (origin `thread`, the stored access merged into it), not the
    // harness row Settings showed.
    let thread = start_thread(&threads, &linked, "work here briefly").await;
    wait_idle(&threads, &thread).await;
    let fresh = tracker.list().await;
    let fresh_row = fresh
        .iter()
        .find(|entry| entry.path == row.path)
        .unwrap_or_else(|| panic!("listed: {fresh:?}"));
    assert_eq!(fresh_row.origin, WorktreeOrigin::Thread);
    assert_ne!(
        fresh_row.status,
        WorktreeStatus::Active,
        "the thread settled"
    );

    let outcome = tracker
        .clear(Some(vec![row.path.clone()]), false, true)
        .await;
    assert!(outcome.removed.is_empty(), "{outcome:?}");
    assert_eq!(
        outcome.skipped.len(),
        1,
        "reported like Clear all reports it"
    );
    assert_eq!(outcome.skipped[0].path, row.path);
    assert_eq!(outcome.skipped[0].reason, "is a thread workspace");
    assert!(linked.is_dir(), "the thread workspace is kept");

    // Naming it on its own still removes it.
    let outcome = tracker
        .clear(Some(vec![row.path.clone()]), false, false)
        .await;
    assert_eq!(outcome.removed, vec![row.path.clone()], "{outcome:?}");
    assert!(!linked.exists());
}

#[cfg(unix)]
#[test]
fn a_fifo_registration_head_is_never_opened() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-fifo-head", "fifo-head-branch");
    let admin =
        PathBuf::from(git::run(&linked, ["rev-parse", "--absolute-git-dir"]).expect("admin dir"));
    // Only the registration is left, and its `HEAD` is a FIFO: opening it would
    // block until a writer appears, which never happens.
    std::fs::remove_dir_all(&linked).expect("remove the checkout");
    let head = admin.join("HEAD");
    std::fs::remove_file(&head).expect("remove HEAD");
    let status = std::process::Command::new("mkfifo")
        .arg(&head)
        .status()
        .expect("run mkfifo");
    if !status.success() {
        // FIFOs cannot exist on this filesystem (e.g. WSL drvfs).
        return;
    }

    let repo = fixture.repo.clone();
    let data_dir = fixture.data_dir();
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        git::FORCE_METADATA_FALLBACK.with(|forced| forced.set(true));
        let registrations =
            git::registrations_from_metadata(&repo, Duration::from_secs(30)).expect("read");
        let list_input = ListInput {
            subagents: Vec::new(),
            parent_workspaces: HashMap::new(),
            subagent_repos: HashMap::new(),
            threads: Vec::new(),
            stored: vec![StoredWorktree {
                path: linked.to_string_lossy().into_owned(),
                repo: repo.to_string_lossy().into_owned(),
                branch: Some("fifo-head-branch".into()),
                first_seen: Utc::now(),
                last_seen: Utc::now(),
                accesses: Vec::new(),
            }],
            subagents_root: git::norm(&data_dir.join("subagents")),
        };
        let (listed, _) = compute(&list_input, true);
        let target = entry(&linked, &repo, Some("fifo-head-branch"), false);
        let cleared = clear_one(&target, false, &data_dir);
        let _ = done.send((registrations, listed, cleared, linked));
    });
    let (registrations, listed, cleared, linked) = finished
        .recv_timeout(Duration::from_secs(20))
        .expect("listing and clearing return promptly");
    let registration = registrations
        .iter()
        .find(|registration| registration.path == linked)
        .unwrap_or_else(|| panic!("listed: {registrations:?}"));
    assert_eq!(registration.head, git::Head::Unknown);
    assert!(
        listed
            .iter()
            .any(|row| Path::new(&row.path) == linked && !row.exists),
        "{listed:?}"
    );
    assert_eq!(
        cleared.err().as_deref(),
        Some("could not read the registration's HEAD")
    );
    assert!(admin.is_dir(), "the registration stays");
}

#[cfg(unix)]
#[test]
fn rebase_and_bisect_files_that_are_not_regular_count_as_in_progress() {
    let fixture = Fixture::new();
    let git_dir = fixture.repo.join(".git");
    let needles = ["refs/heads/h4rn/sa-x", "h4rn/sa-x"];
    let bisect = git_dir.join("BISECT_START");
    let status = std::process::Command::new("mkfifo")
        .arg(&bisect)
        .status()
        .expect("run mkfifo");
    if !status.success() {
        // FIFOs cannot exist on this filesystem (e.g. WSL drvfs).
        return;
    }
    let (done, finished) = std::sync::mpsc::channel();
    let repo = fixture.repo.clone();
    std::thread::spawn(move || {
        let _ = done.send(super::branch_in_progress(&repo.join(".git"), "h4rn/sa-x"));
    });
    let answer = finished
        .recv_timeout(Duration::from_secs(20))
        .expect("returns promptly");
    assert_eq!(answer, Ok(Some("it is in use by a bisect".to_owned())));
    std::fs::remove_file(&bisect).expect("remove the FIFO");

    // An oversized state file is not read and counts as in progress too.
    std::fs::create_dir_all(git_dir.join("rebase-merge")).expect("rebase dir");
    let head_name = git_dir.join("rebase-merge/head-name");
    std::fs::write(&head_name, "x".repeat(5000)).expect("big head-name");
    assert!(super::file_has_line(&head_name, &needles));
    // A small one that names another branch does not.
    std::fs::write(&head_name, "refs/heads/other\n").expect("head-name");
    assert!(!super::file_has_line(&head_name, &needles));
    // A missing one does not.
    assert!(!super::file_has_line(
        &git_dir.join("BISECT_START"),
        &needles
    ));
}

#[cfg(unix)]
#[test]
fn a_symlink_where_a_checkout_was_is_prunable_in_the_metadata_fallback() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-prune-link", "prune-link-branch");
    let moved = fixture.root.path().join("wt-prune-link-moved");
    std::fs::rename(&linked, &moved).expect("move away");
    std::os::unix::fs::symlink(&moved, &linked).expect("symlink");
    let registrations =
        git::registrations_from_metadata(&fixture.repo, Duration::from_secs(30)).expect("read");
    let registration = registrations
        .iter()
        .find(|registration| registration.path == linked)
        .unwrap_or_else(|| panic!("listed: {registrations:?}"));
    assert!(registration.prunable, "a symlink is not a live checkout");
}

#[cfg(unix)]
#[test]
fn is_dirty_gives_up_after_its_deadline() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-slow-status", "slow-status-branch");
    let bin = tempfile::tempdir().expect("bin dir");
    let fake = fake_git(bin.path(), "exec sleep 30");
    git::GIT_PROGRAM.with(|program| *program.borrow_mut() = Some(fake));
    git::OBSERVATION_LIMIT.with(|limit| limit.set(Some(Duration::from_millis(200))));
    let started = std::time::Instant::now();
    let dirty = git::is_dirty(&linked);
    let elapsed = started.elapsed();
    git::OBSERVATION_LIMIT.with(|limit| limit.set(None));
    git::GIT_PROGRAM.with(|program| *program.borrow_mut() = None);
    assert_eq!(dirty, None);
    assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
}

#[cfg(unix)]
#[test]
fn an_owned_row_under_a_symlinked_parent_stays_clearable() {
    let fixture = Fixture::new();
    let checkout = fixture.add_worktree("sa-linked", "h4rn/sa-linked");
    // The recorded path reaches the checkout through a symlinked parent, as a
    // data directory behind a symlink would.
    let link = fixture.root.path().join("linked-parent");
    std::os::unix::fs::symlink(checkout.parent().expect("parent"), &link).expect("symlink");
    let recorded = link.join(checkout.file_name().expect("name"));
    let views = vec![subagent_view(
        "sa_linked",
        "th_parent",
        &recorded,
        "h4rn/sa-linked",
        SubagentState::Failed,
    )];
    let (entries, _) = compute(&input(&fixture, views), true);
    let row = entries
        .iter()
        .find(|entry| entry.subagent_id.as_ref().map(SubagentId::as_str) == Some("sa_linked"))
        .unwrap_or_else(|| panic!("listed: {entries:?}"));
    assert_eq!(
        Path::new(&row.path),
        git::norm(&checkout),
        "parent resolved"
    );
    assert!(row.exists);
    assert_eq!(row.status, WorktreeStatus::Orphaned);
    assert!(matches!(
        clear_one(row, false, &fixture.data_dir()),
        Ok(ClearOne::Removed)
    ));
    assert!(!checkout.exists());
}

#[tokio::test]
async fn a_bulk_clear_skips_a_checkout_holding_a_vanished_thread_workspace() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-src", "with-src");
    let src = linked.join("src");
    std::fs::create_dir_all(&src).expect("src");
    std::fs::write(src.join("lib.rs"), "// lib\n").expect("lib");
    run(&linked, &["add", "src/lib.rs"]);
    run(&linked, &["commit", "-q", "-m", "add src"]);
    let (tracker, threads, _subagents) =
        managers(&fixture, vec![Arc::new(FakeHarness::new())]).await;
    let thread = start_thread(&threads, &src, "work in src").await;
    wait_idle(&threads, &thread).await;

    // A branch switch removes the thread's workspace; the checkout stays.
    run(&linked, &["switch", "-q", "-c", "no-src", "main"]);
    assert!(!src.exists() && linked.is_dir());
    tracker
        .inner
        .store
        .record(
            &linked.to_string_lossy(),
            &fixture.repo.to_string_lossy(),
            Some("no-src"),
            "th_gone",
            None,
            Utc::now(),
        )
        .await
        .expect("record the observation");
    let listed = tracker.list().await;
    let row = listed
        .iter()
        .find(|entry| Path::new(&entry.path) == linked)
        .unwrap_or_else(|| panic!("listed: {listed:?}"))
        .clone();
    assert_eq!(
        row.origin,
        WorktreeOrigin::Harness,
        "a vanished workspace makes no thread row"
    );

    // Clear all, with and without the paths it showed, skips it and says why.
    for paths in [None, Some(vec![row.path.clone()])] {
        let outcome = tracker.clear(paths.clone(), false, true).await;
        assert!(outcome.removed.is_empty(), "{paths:?}: {outcome:?}");
        assert_eq!(outcome.skipped.len(), 1, "{paths:?}: {outcome:?}");
        assert_eq!(outcome.skipped[0].path, row.path);
        assert_eq!(outcome.skipped[0].reason, "is a thread workspace");
        assert!(linked.is_dir(), "{paths:?}: the checkout is kept");
    }

    // Naming it on its own still removes the settled thread's checkout.
    let outcome = tracker
        .clear(Some(vec![row.path.clone()]), false, false)
        .await;
    assert_eq!(outcome.removed, vec![row.path.clone()], "{outcome:?}");
    assert!(!linked.exists());
}

#[tokio::test]
async fn a_thread_that_starts_inside_a_later_entry_during_a_bulk_clear_keeps_it() {
    let fixture = Fixture::new();
    let first = fixture.add_worktree("wt-bulk-first", "bulk-first");
    let second = fixture.add_worktree("wt-bulk-second", "bulk-second");
    let workspace = second.join("src");
    std::fs::create_dir_all(&workspace).expect("src");
    let (tracker, threads, _subagents) =
        managers(&fixture, vec![Arc::new(FakeHarness::new())]).await;
    for (path, branch) in [(&first, "bulk-first"), (&second, "bulk-second")] {
        tracker
            .inner
            .store
            .record(
                &path.to_string_lossy(),
                &fixture.repo.to_string_lossy(),
                Some(branch),
                "th_gone",
                None,
                Utc::now(),
            )
            .await
            .expect("record the observation");
    }
    let first_path = first.to_string_lossy().into_owned();
    let second_path = second.to_string_lossy().into_owned();

    // While the first entry is being cleared, a thread is assigned a
    // workspace inside the second and settles before the second's turn.
    let started: Arc<std::sync::Mutex<Option<ThreadId>>> = Arc::new(std::sync::Mutex::new(None));
    let hook = {
        let threads = Arc::clone(&threads);
        let started = Arc::clone(&started);
        let first_path = first_path.clone();
        move |path: String| {
            let threads = Arc::clone(&threads);
            let started = Arc::clone(&started);
            let workspace = workspace.clone();
            let now = path == first_path;
            async move {
                if now {
                    let thread = start_thread(&threads, &workspace, "settle in src").await;
                    wait_idle(&threads, &thread).await;
                    *started.lock().expect("lock") = Some(thread);
                }
            }
        }
    };
    let outcome = tracker
        .clear_with(
            Some(vec![first_path.clone(), second_path.clone()]),
            false,
            true,
            hook,
        )
        .await;
    assert!(
        started.lock().expect("lock").is_some(),
        "the hook ran for the first entry"
    );
    assert_eq!(outcome.removed, vec![first_path], "{outcome:?}");
    assert_eq!(outcome.skipped.len(), 1, "{outcome:?}");
    assert_eq!(outcome.skipped[0].path, second_path);
    assert_eq!(outcome.skipped[0].reason, "is a thread workspace");
    assert!(second.is_dir(), "the second checkout is kept");
    assert!(!first.exists());
}

/// Moves `file` aside under `<root>/<name>` and leaves a symlink to it in its
/// place.
#[cfg(unix)]
fn symlink_in_place(file: &Path, root: &Path, name: &str) {
    let target = root.join(name);
    std::fs::rename(file, &target).expect("move aside");
    std::os::unix::fs::symlink(&target, file).expect("symlink");
}

#[cfg(unix)]
#[test]
fn the_metadata_fallback_follows_a_symlinked_gitdir_and_keeps_its_branch() {
    let fixture = Fixture::new();
    let holder = fixture.add_worktree("wt-sym-holder", "h4rn/sa-sym");
    let admin =
        PathBuf::from(git::run(&holder, ["rev-parse", "--absolute-git-dir"]).expect("admin dir"));
    symlink_in_place(&admin.join("gitdir"), fixture.root.path(), "gitdir-target");
    symlink_in_place(&admin.join("HEAD"), fixture.root.path(), "head-target");

    git::FORCE_METADATA_FALLBACK.with(|forced| forced.set(true));
    let snapshot = git::registrations(&fixture.repo).expect("read");
    let gone = git::norm(&fixture.root.path().join("sa-sym-gone"));
    let target = entry(&gone, &fixture.repo, Some("h4rn/sa-sym"), true);
    let outcome = clear_one(&target, false, &fixture.data_dir());
    git::FORCE_METADATA_FALLBACK.with(|forced| forced.set(false));

    assert!(snapshot.complete, "{snapshot:?}");
    let registration = snapshot
        .iter()
        .find(|registration| registration.path == holder)
        .unwrap_or_else(|| panic!("the symlinked registration is listed: {snapshot:?}"));
    assert_eq!(registration.head, git::Head::Branch("h4rn/sa-sym".into()));
    match outcome {
        Ok(ClearOne::RemovedWithBranchFailure(reason)) => {
            assert!(reason.contains("it is checked out in"), "{reason}");
        }
        other => panic!("the branch is kept: {other:?}"),
    }
    assert!(git::branch_exists(&fixture.repo, "h4rn/sa-sym"));
}

#[test]
fn an_unreadable_registration_makes_the_snapshot_incomplete() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-incomplete", "incomplete-branch");
    // An admin entry whose `gitdir` is a directory, not a file git wrote.
    let junk = fixture.repo.join(".git/worktrees/junk/gitdir");
    std::fs::create_dir_all(&junk).expect("junk admin entry");
    let branch = "h4rn/sa-incomplete";
    run(&fixture.repo, &["branch", branch, "main"]);
    let sha = git::run(&fixture.repo, ["rev-parse", "main"]).expect("sha");
    let gone = git::norm(&fixture.root.path().join("sa-incomplete-gone"));
    let owned = entry(&gone, &fixture.repo, Some(branch), true);
    let existing = entry(&linked, &fixture.repo, Some("incomplete-branch"), false);
    let missing_harness = gone.join("harness");

    // `-z` skips the unreadable admin entry and exits 0, so its answer is not
    // complete either.
    assert!(!git::registrations(&fixture.repo).expect("-z").complete);

    git::FORCE_METADATA_FALLBACK.with(|forced| forced.set(true));
    let snapshot = git::registrations(&fixture.repo).expect("read");
    let plan = super::Preflight {
        branch: Some(super::BranchDelete::Verified { sha }),
    };
    let deleted = delete_owned_branch(&fixture.repo, branch, &plan, false, &Default::default());
    let owned_outcome = clear_one(&owned, false, &fixture.data_dir());
    let forced_outcome = clear_one(&owned, true, &fixture.data_dir());
    let existing_outcome = clear_one(&existing, true, &fixture.data_dir());
    let mut list_input = input(&fixture, Vec::new());
    list_input.stored = vec![StoredWorktree {
        path: missing_harness.to_string_lossy().into_owned(),
        repo: fixture.repo.to_string_lossy().into_owned(),
        branch: None,
        first_seen: Utc::now(),
        last_seen: Utc::now(),
        accesses: Vec::new(),
    }];
    let (listed, forgotten) = compute(&list_input, false);
    git::FORCE_METADATA_FALLBACK.with(|forced| forced.set(false));

    assert!(!snapshot.complete, "{snapshot:?}");
    assert!(
        snapshot
            .iter()
            .any(|registration| registration.path == linked),
        "readable registrations are still listed: {snapshot:?}"
    );
    assert_eq!(
        deleted,
        Ok(Some(format!(
            "checkout removed, but branch {branch} could not be checked: the worktree list is \
             incomplete"
        )))
    );
    for outcome in [owned_outcome, forced_outcome, existing_outcome] {
        assert_eq!(
            outcome.err().as_deref(),
            Some("could not read every worktree registration")
        );
    }
    assert!(
        git::branch_exists(&fixture.repo, branch),
        "the branch is kept"
    );
    assert!(linked.is_dir(), "the checkout is kept");
    assert!(
        forgotten.is_empty()
            && listed
                .iter()
                .any(|row| Path::new(&row.path) == missing_harness),
        "a partial snapshot never proves a stored row gone: {listed:?} {forgotten:?}"
    );
}

#[cfg(unix)]
#[test]
fn metadata_reads_follow_symlinks_only_where_git_does() {
    let dir = tempfile::tempdir().expect("dir");
    let file = dir.path().join("file");
    std::fs::write(&file, "ref: refs/heads/main\n").expect("file");
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&file, &link).expect("symlink");
    assert!(git::read_bounded(&file).is_some());
    assert_eq!(git::read_bounded(&link), None, "never followed");
    assert_eq!(
        git::read_bounded_following(&link).as_deref(),
        Some(&b"ref: refs/heads/main\n"[..])
    );

    // A symlink to a FIFO is followed only to find it is not a regular file.
    let fifo = dir.path().join("fifo");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("run mkfifo");
    if !status.success() {
        // FIFOs cannot exist on this filesystem (e.g. WSL drvfs).
        return;
    }
    let to_fifo = dir.path().join("to-fifo");
    std::os::unix::fs::symlink(&fifo, &to_fifo).expect("symlink");
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send((
            git::read_bounded_following(&to_fifo),
            git::read_bounded(&fifo),
        ));
    });
    assert_eq!(
        finished
            .recv_timeout(Duration::from_secs(20))
            .expect("returns promptly"),
        (None, None)
    );
}

#[cfg(unix)]
#[test]
fn a_dangling_locked_symlink_reads_as_locked() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-lock-link", "lock-link-branch");
    let admin =
        PathBuf::from(git::run(&linked, ["rev-parse", "--absolute-git-dir"]).expect("admin dir"));
    std::os::unix::fs::symlink(fixture.root.path().join("nowhere"), admin.join("locked"))
        .expect("dangling symlink");
    let snapshot =
        git::registrations_from_metadata(&fixture.repo, Duration::from_secs(30)).expect("read");
    let registration = snapshot
        .iter()
        .find(|registration| registration.path == linked)
        .unwrap_or_else(|| panic!("listed: {snapshot:?}"));
    assert!(registration.locked);
}

#[test]
fn an_emptied_gitdir_makes_the_porcelain_snapshot_incomplete() {
    let fixture = Fixture::new();
    let branch = "h4rn/sa-empty-gitdir";
    let holder = fixture.add_worktree("wt-empty-gitdir", branch);
    let sha = git::run(&fixture.repo, ["rev-parse", branch]).expect("sha");
    assert!(git::registrations(&fixture.repo).expect("-z").complete);

    // Git skips an admin entry whose `gitdir` is empty and still exits 0, so
    // the worktree holding the branch vanishes from its answer.
    let admin =
        PathBuf::from(git::run(&holder, ["rev-parse", "--absolute-git-dir"]).expect("admin dir"));
    std::fs::write(admin.join("gitdir"), "").expect("empty gitdir");
    let porcelain = git::porcelain_z(&fixture.repo, Duration::from_secs(30))
        .expect("run")
        .expect("this git supports -z");
    assert!(
        porcelain
            .iter()
            .all(|registration| registration.path != holder),
        "git omits it: {porcelain:?}"
    );
    let snapshot = git::registrations(&fixture.repo).expect("-z");
    assert!(!snapshot.complete, "{snapshot:?}");

    let plan = super::Preflight {
        branch: Some(super::BranchDelete::Verified { sha }),
    };
    let deleted = delete_owned_branch(&fixture.repo, branch, &plan, false, &Default::default());
    assert_eq!(
        deleted,
        Ok(Some(format!(
            "checkout removed, but branch {branch} could not be checked: the worktree list is \
             incomplete"
        )))
    );
    assert!(
        git::branch_exists(&fixture.repo, branch),
        "the branch is kept"
    );
}

#[test]
fn an_all_zero_detached_head_is_unknown_in_both_sources() {
    let zeros = |len: usize| "0".repeat(len);
    let out = format!(
        "worktree /wt-a\0HEAD {}\0detached\0\0worktree /wt-b\0HEAD {}\0detached\0\0\
         worktree /wt-c\0HEAD 0000\0detached\0\0worktree /wt-d\0HEAD {}\0detached\0\0",
        zeros(40),
        zeros(64),
        "a".repeat(40),
    );
    let heads: Vec<git::Head> = git::parse_registrations(out.as_bytes())
        .into_iter()
        .map(|registration| registration.head)
        .collect();
    assert_eq!(
        heads,
        vec![
            git::Head::Unknown,
            git::Head::Unknown,
            git::Head::Unknown,
            git::Head::Detached("a".repeat(40)),
        ]
    );

    // Metadata: a `HEAD` file holding the all-zero id, SHA-1 or SHA-256 long.
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-zero-head", "zero-head-branch");
    let admin =
        PathBuf::from(git::run(&linked, ["rev-parse", "--absolute-git-dir"]).expect("admin dir"));
    std::fs::remove_dir_all(&linked).expect("remove the checkout");
    for len in [40, 64] {
        std::fs::write(admin.join("HEAD"), format!("{}\n", zeros(len))).expect("zero HEAD");
        let registrations =
            git::registrations_from_metadata(&fixture.repo, Duration::from_secs(30)).expect("read");
        let registration = registrations
            .iter()
            .find(|registration| registration.path == linked)
            .unwrap_or_else(|| panic!("listed: {registrations:?}"));
        assert_eq!(registration.head, git::Head::Unknown, "{len} zeros");
    }

    // Without force, the missing checkout keeps its registration.
    git::FORCE_METADATA_FALLBACK.with(|forced| forced.set(true));
    let target = entry(&linked, &fixture.repo, Some("zero-head-branch"), false);
    let outcome = clear_one(&target, false, &fixture.data_dir());
    git::FORCE_METADATA_FALLBACK.with(|forced| forced.set(false));
    assert_eq!(
        outcome.err().as_deref(),
        Some("could not read the registration's HEAD")
    );
    assert!(admin.is_dir(), "the registration stays");
}

#[test]
fn a_bare_main_registration_does_not_block_an_owned_branch_delete() {
    let fixture = Fixture::new();
    run(
        fixture.root.path(),
        &[
            "clone",
            "-q",
            "--bare",
            &fixture.repo.to_string_lossy(),
            "bare.git",
        ],
    );
    let bare = git::norm(&fixture.root.path().join("bare.git"));
    for (fallback, name) in [(false, "sa-bare-z"), (true, "sa-bare-meta")] {
        let branch = format!("h4rn/{name}");
        run(&bare, &["branch", &branch, "main"]);
        let linked = fixture.root.path().join(name);
        run(
            &bare,
            &["worktree", "add", "-q", &linked.to_string_lossy(), &branch],
        );
        let linked = git::norm(&linked);
        let target = entry(&linked, &bare, Some(&branch), true);

        git::FORCE_METADATA_FALLBACK.with(|forced| forced.set(fallback));
        let snapshot = git::registrations(&bare).expect("read");
        let outcome = clear_one(&target, false, &fixture.data_dir());
        git::FORCE_METADATA_FALLBACK.with(|forced| forced.set(false));

        assert!(snapshot.complete, "fallback={fallback}: {snapshot:?}");
        assert!(
            snapshot.iter().any(|registration| registration.path == bare
                && registration.bare
                && registration.head == git::Head::Unknown),
            "fallback={fallback}: {snapshot:?}"
        );
        assert!(
            matches!(outcome, Ok(ClearOne::Removed)),
            "fallback={fallback}: {outcome:?}"
        );
        assert!(!linked.exists(), "fallback={fallback}");
        assert!(
            !git::branch_exists(&bare, &branch),
            "fallback={fallback}: the merged owned branch is deleted"
        );
    }
}

#[tokio::test]
async fn an_independent_thread_inside_an_orphaned_subagent_checkout_keeps_it_out_of_clear_all() {
    let fixture = Fixture::new();
    let checkout = fixture.add_worktree("data/subagents/sa_orphan/worktree", "h4rn/sa-orphan");
    let src = checkout.join("src");
    std::fs::create_dir_all(&src).expect("src");
    let views = vec![subagent_view(
        "sa_orphan",
        "th_parent",
        &checkout,
        "h4rn/sa-orphan",
        SubagentState::Failed,
    )];
    let thread_at = |id: &str, workspace: &Path| ThreadInfo {
        thread_id: ThreadId::new(id),
        title: id.into(),
        workspace: workspace.to_string_lossy().into_owned(),
        working: false,
        last_activity: Utc::now(),
    };
    let mut list_input = input(&fixture, views);
    // The subagent's own child, settled, still works in the checkout.
    list_input.threads = vec![thread_at("th_sa_orphan", &checkout)];
    let (entries, _, workspaces) = compute_listing(&list_input, false);
    let row = entries
        .iter()
        .find(|entry| entry.owned)
        .unwrap_or_else(|| panic!("listed: {entries:?}"))
        .clone();
    assert_eq!(row.status, WorktreeStatus::Orphaned);
    let selection = select_entries(&entries, None, false, &workspaces);
    assert_eq!(
        selection
            .targets
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>(),
        vec![row.path.as_str()],
        "its own child does not protect it"
    );
    assert!(selection.skipped.is_empty(), "{:?}", selection.skipped);

    // An independent thread opened inside it does, settled or not.
    list_input.threads.push(thread_at("th_independent", &src));
    let (entries, _, workspaces) = compute_listing(&list_input, false);
    for paths in [None, Some(vec![row.path.clone()])] {
        let selection = select_entries(&entries, paths.as_deref(), true, &workspaces);
        assert!(selection.targets.is_empty(), "{paths:?}");
        assert_eq!(selection.skipped.len(), 1, "{paths:?}");
        assert_eq!(selection.skipped[0].path, row.path);
        assert_eq!(selection.skipped[0].reason, "is a thread workspace");
    }
    // Naming it on its own still selects it.
    let selection = select_entries(
        &entries,
        Some(std::slice::from_ref(&row.path)),
        false,
        &workspaces,
    );
    assert_eq!(selection.targets.len(), 1);

    // The live scan of a bulk clear: a real settled thread inside the
    // subagent checkout protects it; the same thread as the row's own child
    // does not.
    let (tracker, threads, _subagents) =
        managers(&fixture, vec![Arc::new(FakeHarness::new())]).await;
    let thread = start_thread(&threads, &src, "settle inside the subagent checkout").await;
    wait_idle(&threads, &thread).await;
    assert!(matches!(
        tracker.inner.still_active(&row, true).await,
        Some(LiveUse::ThreadWorkspace)
    ));
    assert!(
        tracker.inner.still_active(&row, false).await.is_none(),
        "only a bulk clear is protected from a settled thread"
    );
    let mut own = row.clone();
    own.accessed_by = vec![WorktreeAccess {
        thread_id: thread.clone(),
        title: String::new(),
        subagent_id: own.subagent_id.clone(),
        working: false,
        last_seen: Utc::now(),
    }];
    assert!(
        tracker.inner.still_active(&own, true).await.is_none(),
        "the subagent's own child is exempt"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_bulk_clear_compares_live_workspaces_like_the_selection() {
    let fixture = Fixture::new();
    // The symlink the hook leaves in `later` is ignored, so git's own removal
    // would not refuse it as untracked.
    ignore(&fixture, "link");
    let first = fixture.add_worktree("wt-cmp-first", "cmp-first");
    let later = fixture.add_worktree("wt-cmp-later", "cmp-later");
    let outside = fixture.root.path().join("outside");
    std::fs::create_dir_all(&outside).expect("outside");
    let link = later.join("link");
    let workspace = link.join("src");
    let (tracker, threads, _subagents) =
        managers(&fixture, vec![Arc::new(FakeHarness::new())]).await;
    for (path, branch) in [(&first, "cmp-first"), (&later, "cmp-later")] {
        tracker
            .inner
            .store
            .record(
                &path.to_string_lossy(),
                &fixture.repo.to_string_lossy(),
                Some(branch),
                "th_gone",
                None,
                Utc::now(),
            )
            .await
            .expect("record the observation");
    }
    let first_path = first.to_string_lossy().into_owned();
    let later_path = later.to_string_lossy().into_owned();

    // After selection, while the first entry is being cleared, a thread
    // settles in `later/link/src`; then `src` disappears and `link` becomes a
    // symlink pointing outside `later`.
    let started: Arc<std::sync::Mutex<Option<ThreadId>>> = Arc::new(std::sync::Mutex::new(None));
    let hook = {
        let threads = Arc::clone(&threads);
        let started = Arc::clone(&started);
        let first_path = first_path.clone();
        move |path: String| {
            let threads = Arc::clone(&threads);
            let started = Arc::clone(&started);
            let (workspace, link, outside) = (workspace.clone(), link.clone(), outside.clone());
            let now = path == first_path;
            async move {
                if now {
                    std::fs::create_dir_all(&workspace).expect("workspace");
                    let thread = start_thread(&threads, &workspace, "settle in link/src").await;
                    wait_idle(&threads, &thread).await;
                    std::fs::remove_dir_all(&link).expect("remove link/src");
                    std::os::unix::fs::symlink(&outside, &link).expect("symlink");
                    *started.lock().expect("lock") = Some(thread);
                }
            }
        }
    };
    let outcome = tracker
        .clear_with(
            Some(vec![first_path.clone(), later_path.clone()]),
            false,
            true,
            hook,
        )
        .await;
    assert!(
        started.lock().expect("lock").is_some(),
        "the hook ran for the first entry"
    );
    // Resolved now, the workspace is outside `later`; as recorded, it is not.
    let recorded = later.join("link").join("src");
    assert!(!git::norm(&recorded).starts_with(&later));
    assert_eq!(outcome.removed, vec![first_path], "{outcome:?}");
    assert_eq!(outcome.skipped.len(), 1, "{outcome:?}");
    assert_eq!(outcome.skipped[0].path, later_path);
    assert_eq!(outcome.skipped[0].reason, "is a thread workspace");
    assert!(later.is_dir(), "the later checkout is kept");
    assert!(!first.exists());
}

/// Pauses an interactive rebase with `--autostash` at a `break` in `checkout`,
/// after an uncommitted edit so the autostash exists. Returns the checkout's
/// admin dir.
fn pause_rebase_with_autostash(checkout: &Path) -> PathBuf {
    std::fs::write(checkout.join("README.md"), "# edited, then stashed\n").expect("edit");
    let output = std::process::Command::new("git")
        .args(["rebase", "-i", "--autostash", "HEAD"])
        .env("GIT_SEQUENCE_EDITOR", "printf 'break\\n' >")
        .current_dir(checkout)
        .output()
        .expect("run git rebase");
    assert!(
        output.status.success(),
        "rebase: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let admin =
        PathBuf::from(git::run(checkout, ["rev-parse", "--absolute-git-dir"]).expect("admin dir"));
    assert!(
        admin.join("rebase-merge/autostash").is_file(),
        "the rebase is paused with an autostash"
    );
    admin
}

#[test]
fn a_paused_rebase_keeps_the_worktree_and_its_autostash() {
    for gone in [false, true] {
        let fixture = Fixture::new();
        let branch = if gone { "rebase-gone" } else { "rebase-here" };
        let linked = fixture.add_worktree(&format!("wt-{branch}"), branch);
        let admin = pause_rebase_with_autostash(&linked);
        if gone {
            std::fs::remove_dir_all(&linked).expect("remove the checkout");
        }
        let target = entry(&linked, &fixture.repo, Some(branch), false);
        let data = fixture.data_dir();

        let outcome = clear_one(&target, false, &data);
        assert_eq!(
            outcome.err().as_deref(),
            Some("has a git operation in progress"),
            "gone={gone}"
        );
        assert_eq!(linked.is_dir(), !gone, "gone={gone}: the checkout is kept");
        assert!(
            admin.join("rebase-merge/autostash").is_file(),
            "gone={gone}: the rebase state and its autostash are kept"
        );

        let forced = clear_one(&target, true, &data);
        assert!(
            matches!(forced, Ok(ClearOne::Removed)),
            "gone={gone}: {forced:?}"
        );
        assert!(!linked.exists(), "gone={gone}");
        assert!(!admin.exists(), "gone={gone}: the registration is removed");
    }
}

#[cfg(unix)]
#[test]
fn an_operation_started_after_prepare_is_caught_before_removal() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-late-merge", "late-merge");
    let target = entry(&linked, &fixture.repo, Some("late-merge"), false);
    let commons = git::CommonDirs::default();
    let plan = preflight(&target, false, &commons).expect("preflight");
    let prepared = prepare(&target, false, &commons).expect("prepare");
    let admin =
        PathBuf::from(git::run(&linked, ["rev-parse", "--absolute-git-dir"]).expect("admin dir"));
    // Presence alone counts, without following a symlink: a dangling one too.
    std::os::unix::fs::symlink(
        fixture.root.path().join("nowhere"),
        admin.join("MERGE_HEAD"),
    )
    .expect("dangling MERGE_HEAD");
    let outcome = execute(
        &target,
        false,
        &fixture.data_dir(),
        &plan,
        &prepared,
        &commons,
    );
    assert_eq!(
        outcome.err().as_deref(),
        Some("has a git operation in progress")
    );
    assert!(linked.is_dir());
}

#[test]
fn a_linked_checkout_of_a_bare_repository_belongs_to_it() {
    let fixture = Fixture::new();
    run(
        fixture.root.path(),
        &[
            "clone",
            "-q",
            "--bare",
            &fixture.repo.to_string_lossy(),
            "project.git",
        ],
    );
    let bare = git::norm(&fixture.root.path().join("project.git"));
    let linked = fixture.root.path().join("project-wt");
    run(
        &bare,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "bare-feature",
            &linked.to_string_lossy(),
            "main",
        ],
    );
    let linked = git::norm(&linked);
    assert_eq!(git::repo_of(&linked), Some(bare.clone()));
    assert_eq!(git::repo_of(&bare), Some(bare.clone()));

    // A thread working in it lists a thread row of the bare repository.
    let mut list_input = input(&fixture, Vec::new());
    list_input.threads = vec![ThreadInfo {
        thread_id: ThreadId::new("th_bare"),
        title: "bare".into(),
        workspace: linked.to_string_lossy().into_owned(),
        working: false,
        last_activity: Utc::now(),
    }];
    let (entries, _) = compute(&list_input, true);
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0].origin, WorktreeOrigin::Thread);
    assert_eq!(Path::new(&entries[0].path), linked);
    assert_eq!(Path::new(&entries[0].repo), bare);
    assert_eq!(entries[0].branch.as_deref(), Some("bare-feature"));
}

#[test]
fn a_separate_git_dir_repository_has_no_bogus_bare_main_in_the_fallback() {
    let fixture = Fixture::new();
    let gitdir = fixture.root.path().join("sep.gitdir");
    let main = fixture.root.path().join("sep");
    run(
        fixture.root.path(),
        &[
            "init",
            "-q",
            "-b",
            "main",
            "--separate-git-dir",
            &gitdir.to_string_lossy(),
            &main.to_string_lossy(),
        ],
    );
    let (gitdir, main) = (git::norm(&gitdir), git::norm(&main));
    run(&main, &["config", "user.email", "wt@h4rn.invalid"]);
    run(&main, &["config", "user.name", "h4rn worktrees"]);
    run(&main, &["commit", "-q", "--allow-empty", "-m", "init"]);
    let linked = fixture.root.path().join("sep-linked");
    run(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "sep-feature",
            &linked.to_string_lossy(),
        ],
    );
    let linked = git::norm(&linked);
    assert_eq!(git::repo_of(&linked), None, "not bare, not named .git");

    git::FORCE_METADATA_FALLBACK.with(|forced| forced.set(true));
    let without = git::registrations(&main);
    run(&main, &["config", "core.worktree", &main.to_string_lossy()]);
    let with = git::registrations(&main);
    git::FORCE_METADATA_FALLBACK.with(|forced| forced.set(false));

    // Without `core.worktree` the main worktree is unknown: not registered,
    // and the snapshot is incomplete.
    let without = without.expect("read");
    assert!(!without.complete, "{without:?}");
    assert!(
        without
            .iter()
            .all(|registration| !registration.bare && registration.path != gitdir),
        "{without:?}"
    );
    assert!(
        without
            .iter()
            .any(|registration| registration.path == linked
                && registration.head == git::Head::Branch("sep-feature".into())),
        "{without:?}"
    );

    // With it, the main worktree is registered there, not bare.
    let with = with.expect("read");
    assert!(with.complete, "{with:?}");
    assert!(
        with.iter().any(|registration| registration.path == main
            && !registration.bare
            && registration.head == git::Head::Branch("main".into())),
        "{with:?}"
    );
    assert!(with.iter().all(|registration| !registration.bare));
}

#[test]
fn a_gitdir_without_a_dot_git_suffix_names_the_checkout_like_git() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-no-suffix", "no-suffix");
    let admin =
        PathBuf::from(git::run(&linked, ["rev-parse", "--absolute-git-dir"]).expect("admin dir"));
    // Git strips a trailing `/.git` only when it is there.
    std::fs::write(admin.join("gitdir"), format!("{}\n", linked.display())).expect("gitdir");
    let porcelain = git::porcelain_z(&fixture.repo, Duration::from_secs(30))
        .expect("run")
        .expect("this git supports -z");
    assert!(
        porcelain
            .iter()
            .any(|registration| registration.path == linked),
        "git names the target verbatim: {porcelain:?}"
    );
    let snapshot = git::registrations(&fixture.repo).expect("-z");
    assert!(snapshot.complete, "{snapshot:?}");

    let metadata =
        git::registrations_from_metadata(&fixture.repo, Duration::from_secs(30)).expect("read");
    assert!(metadata.complete, "{metadata:?}");
    assert!(
        metadata
            .iter()
            .any(|registration| registration.path == linked),
        "{metadata:?}"
    );
}

#[cfg(unix)]
#[test]
fn a_clear_resolves_the_common_dir_once() {
    let fixture = Fixture::new();
    let branch = "h4rn/sa-once";
    let linked = fixture.add_worktree("wt-once", branch);
    let target = entry(&linked, &fixture.repo, Some(branch), true);
    let log = fixture.root.path().join("common-dir.log");
    let counting = fake_git(
        fixture.root.path(),
        &format!(
            "for arg; do [ \"$arg\" = --git-common-dir ] && echo x >> '{}'; done",
            log.display()
        ),
    );
    git::GIT_PROGRAM.with(|slot| *slot.borrow_mut() = Some(counting));
    let outcome = clear_one(&target, false, &fixture.data_dir());
    git::GIT_PROGRAM.with(|slot| *slot.borrow_mut() = None);
    assert!(matches!(outcome, Ok(ClearOne::Removed)), "{outcome:?}");
    assert!(!git::branch_exists(&fixture.repo, branch));
    let lookups = std::fs::read_to_string(&log).unwrap_or_default();
    assert_eq!(lookups.lines().count(), 1, "one rev-parse --git-common-dir");
}

/// The admin dir of the linked checkout at `checkout`.
fn admin_dir_of(checkout: &Path) -> PathBuf {
    PathBuf::from(git::run(checkout, ["rev-parse", "--absolute-git-dir"]).expect("admin dir"))
}

#[test]
fn staged_work_in_a_moved_checkouts_index_keeps_its_registration() {
    for detached in [false, true] {
        let fixture = Fixture::new();
        let branch = if detached {
            "staged-detached"
        } else {
            "staged"
        };
        let linked = fixture.add_worktree(&format!("wt-{branch}"), branch);
        if detached {
            run(&linked, &["checkout", "-q", "--detach"]);
        }
        let admin = admin_dir_of(&linked);
        // A new version of a tracked file, staged and never committed.
        std::fs::write(linked.join("README.md"), "# staged only\n").expect("edit");
        run(&linked, &["add", "README.md"]);
        // The checkout is moved aside: git skips its cleanliness check for a
        // missing directory and would delete the admin dir, index included.
        let aside = fixture.root.path().join(format!("aside-{branch}"));
        std::fs::rename(&linked, &aside).expect("move the checkout aside");
        let target = entry(&linked, &fixture.repo, (!detached).then_some(branch), false);
        let data = fixture.data_dir();

        let outcome = clear_one(&target, false, &data);
        assert_eq!(
            outcome.err().as_deref(),
            Some("has staged changes"),
            "detached={detached}"
        );
        assert!(admin.is_dir(), "detached={detached}: the admin dir is kept");
        assert!(
            admin.join("index").is_file(),
            "detached={detached}: the index with the staged work is kept"
        );

        let forced = clear_one(&target, true, &data);
        assert!(
            matches!(forced, Ok(ClearOne::Removed)),
            "detached={detached}: {forced:?}"
        );
        assert!(!admin.exists(), "detached={detached}: registration removed");
    }
}

#[test]
fn a_missing_checkout_with_nothing_staged_still_clears() {
    for without_index in [false, true] {
        let fixture = Fixture::new();
        let branch = if without_index {
            "no-index"
        } else {
            "clean-index"
        };
        let linked = fixture.add_worktree(&format!("wt-{branch}"), branch);
        let admin = admin_dir_of(&linked);
        std::fs::remove_dir_all(&linked).expect("remove the checkout");
        if without_index {
            // No index at all: nothing is staged.
            std::fs::remove_file(admin.join("index")).expect("remove the index");
        }
        let target = entry(&linked, &fixture.repo, Some(branch), false);
        let outcome = clear_one(&target, false, &fixture.data_dir());
        assert!(
            matches!(outcome, Ok(ClearOne::Removed)),
            "without_index={without_index}: {outcome:?}"
        );
        assert!(!admin.exists(), "without_index={without_index}");
    }
}

#[test]
fn a_missing_checkout_on_an_unborn_branch_is_not_read_as_clean() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-unborn", "unborn-base");
    let admin = admin_dir_of(&linked);
    std::fs::remove_dir_all(&linked).expect("remove the checkout");
    // The registration's branch has no commit: its index cannot be compared.
    std::fs::write(admin.join("HEAD"), "ref: refs/heads/never-born\n").expect("HEAD");
    let target = entry(&linked, &fixture.repo, Some("never-born"), false);
    let outcome = clear_one(&target, false, &fixture.data_dir());
    assert_eq!(outcome.err().as_deref(), Some("has staged changes"));
    assert!(admin.is_dir(), "the registration is kept");
}

#[test]
fn the_operation_check_uses_the_snapshots_admin_mapping() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-mapping", "mapping");
    let snapshot = git::registrations(&fixture.repo).expect("snapshot");
    assert!(
        matches!(
            &snapshot.admins,
            git::AdminDirs::Read { by_checkout, complete: true }
                if by_checkout.get(&linked) == Some(&vec![admin_dir_of(&linked)])
        ),
        "{snapshot:?}"
    );
    assert_eq!(super::verify_no_operation(&snapshot, &linked), Ok(()));

    let with = |admins: git::AdminDirs| {
        let mut changed = snapshot.clone();
        changed.admins = admins;
        changed
    };
    // Listed, but no longer in a complete mapping.
    let lost = with(git::AdminDirs::Read {
        by_checkout: HashMap::new(),
        complete: true,
    });
    assert_eq!(
        super::verify_no_operation(&lost, &linked).err().as_deref(),
        Some("the registration could no longer be located")
    );
    // Not in a mapping that could not read everything: the operation reason.
    let partial = with(git::AdminDirs::Read {
        by_checkout: HashMap::new(),
        complete: false,
    });
    assert_eq!(
        super::verify_no_operation(&partial, &linked)
            .err()
            .as_deref(),
        Some("has a git operation in progress")
    );
    assert_eq!(
        super::verify_no_operation(&with(git::AdminDirs::Unreadable), &linked)
            .err()
            .as_deref(),
        Some("has a git operation in progress")
    );
}

#[test]
fn a_timed_out_admin_enumeration_is_a_blocked_timeout() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-enum-timeout", "enum-timeout");
    let commons = git::CommonDirs::default();
    let common = commons
        .get(&fixture.repo, Duration::from_secs(30))
        .expect("common dir");
    // Cached bareness means the fallback needs no git call, so a zero deadline
    // is spent in the admin enumeration itself.
    assert_eq!(commons.is_bare(&common, Duration::from_secs(30)), Ok(false));
    git::FORCE_METADATA_FALLBACK.with(|forced| forced.set(true));
    let snapshot = git::registrations_in(&fixture.repo, &common, Duration::ZERO, &commons);
    git::FORCE_METADATA_FALLBACK.with(|forced| forced.set(false));
    let snapshot = snapshot.expect("snapshot");
    assert!(!snapshot.complete, "{snapshot:?}");
    assert!(
        matches!(&snapshot.admins, git::AdminDirs::TimedOut(_)),
        "{snapshot:?}"
    );
    let reason = super::verify_no_operation(&snapshot, &linked).expect_err("refused");
    assert!(git::timed_out(&reason), "{reason}");
}

#[cfg(unix)]
#[test]
fn bareness_is_asked_once_per_common_dir() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-bare-once", "h4rn/sa-bare-once");
    let target = entry(&linked, &fixture.repo, Some("h4rn/sa-bare-once"), true);
    let log = fixture.root.path().join("is-bare.log");
    let counting = fake_git(
        fixture.root.path(),
        &format!(
            "for arg; do [ \"$arg\" = --is-bare-repository ] && echo x >> '{}'; done",
            log.display()
        ),
    );
    git::GIT_PROGRAM.with(|slot| *slot.borrow_mut() = Some(counting));
    // `repo_of` through one cache, twice.
    let commons = git::CommonDirs::default();
    let first = git::repo_of_with(&linked, &commons);
    let second = git::repo_of_with(&linked, &commons);
    // A whole clear under the metadata fallback: four snapshots, one answer.
    git::FORCE_METADATA_FALLBACK.with(|forced| forced.set(true));
    let outcome = clear_one(&target, false, &fixture.data_dir());
    git::FORCE_METADATA_FALLBACK.with(|forced| forced.set(false));
    git::GIT_PROGRAM.with(|slot| *slot.borrow_mut() = None);
    assert_eq!(first, Some(fixture.repo.clone()));
    assert_eq!(second, first);
    assert!(matches!(outcome, Ok(ClearOne::Removed)), "{outcome:?}");
    let asked = std::fs::read_to_string(&log).unwrap_or_default();
    assert_eq!(
        asked.lines().count(),
        2,
        "once for the repo_of cache, once for the clear"
    );
}

#[test]
fn reduced_stat_checks_do_not_hide_a_same_size_edit() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-reduced-stat", "reduced-stat-branch");
    run(&linked, &["config", "core.trustctime", "false"]);
    run(&linked, &["config", "core.checkStat", "minimal"]);
    let file = linked.join("README.md");
    let past = std::time::SystemTime::now() - Duration::from_secs(120);
    let backdate = |file: &Path| {
        std::fs::File::options()
            .write(true)
            .open(file)
            .expect("open")
            .set_modified(past)
            .expect("set mtime");
    };
    // The index records the backdated mtime, well clear of git's racy window.
    backdate(&file);
    run(&linked, &["update-index", "--refresh"]);
    // Git compares ctime in whole seconds: let the edit's ctime land in a
    // later second than the one the index recorded.
    std::thread::sleep(Duration::from_millis(1100));
    // A same-size edit that keeps the indexed mtime.
    std::fs::write(&file, "# WT\n").expect("edit");
    backdate(&file);
    let plain = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&linked)
        .output()
        .expect("plain status");
    assert!(
        plain.stdout.is_empty(),
        "with reduced stat checks plain git misses the edit"
    );

    let target = entry(&linked, &fixture.repo, Some("reduced-stat-branch"), false);
    assert_eq!(
        clear_one(&target, false, &fixture.data_dir())
            .err()
            .as_deref(),
        Some("has uncommitted changes")
    );
    assert_eq!(
        std::fs::read_to_string(&file).expect("still there"),
        "# WT\n"
    );
}

#[test]
fn a_replacement_ref_does_not_make_an_unmerged_branch_look_merged() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("sa-replaced", "h4rn/sa-replaced");
    std::fs::write(linked.join("work.txt"), "child work\n").expect("work");
    run(&linked, &["add", "work.txt"]);
    run(&linked, &["commit", "-q", "-m", "child work"]);
    let tip = git::run(&linked, ["rev-parse", "HEAD"]).expect("tip");
    let main_head = git::run(&fixture.repo, ["rev-parse", "HEAD"]).expect("main head");
    // Replace main's HEAD commit with one whose parent is the child's tip:
    // history now appears to contain the unmerged commit.
    let fake = git::run(
        &fixture.repo,
        [
            "-c",
            "user.email=wt@h4rn.invalid",
            "-c",
            "user.name=wt",
            "commit-tree",
            "HEAD^{tree}",
            "-p",
            &tip,
            "-m",
            "replacement",
        ],
    )
    .expect("fake commit");
    run(&fixture.repo, &["replace", &main_head, &fake]);
    let plain = std::process::Command::new("git")
        .args(["merge-base", "--is-ancestor", &tip, "HEAD"])
        .current_dir(&fixture.repo)
        .status()
        .expect("plain merge-base");
    assert!(plain.success(), "plain git is fooled by the replacement");

    let target = entry(&linked, &fixture.repo, Some("h4rn/sa-replaced"), true);
    assert_eq!(
        clear_one(&target, false, &fixture.data_dir())
            .err()
            .as_deref(),
        Some("branch h4rn/sa-replaced has unmerged commits")
    );
    assert!(linked.is_dir());
    assert!(git::branch_exists(&fixture.repo, "h4rn/sa-replaced"));
}

/// Writes `<repo>/.git/info/grafts` so `child` appears to have `parent` as its
/// only parent.
fn graft(repo: &Path, child: &str, parent: &str) {
    let info = repo.join(".git/info");
    std::fs::create_dir_all(&info).expect("info dir");
    std::fs::write(info.join("grafts"), format!("{child} {parent}\n")).expect("grafts");
}

#[test]
fn grafts_do_not_make_an_unmerged_branch_look_merged() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("sa-grafted", "h4rn/sa-grafted");
    std::fs::write(linked.join("work.txt"), "child work\n").expect("work");
    run(&linked, &["add", "work.txt"]);
    run(&linked, &["commit", "-q", "-m", "child work"]);
    let tip = git::run(&linked, ["rev-parse", "HEAD"]).expect("tip");
    let main_head = git::run(&fixture.repo, ["rev-parse", "HEAD"]).expect("main head");
    graft(&fixture.repo, &main_head, &tip);
    let plain = std::process::Command::new("git")
        .args(["merge-base", "--is-ancestor", &tip, "HEAD"])
        .current_dir(&fixture.repo)
        .status()
        .expect("plain merge-base");
    assert!(plain.success(), "plain git is fooled by the graft");

    let target = entry(&linked, &fixture.repo, Some("h4rn/sa-grafted"), true);
    assert_eq!(
        clear_one(&target, false, &fixture.data_dir())
            .err()
            .as_deref(),
        Some("branch h4rn/sa-grafted has unmerged commits")
    );
    assert!(linked.is_dir());
    assert!(git::branch_exists(&fixture.repo, "h4rn/sa-grafted"));
}

#[test]
fn grafts_do_not_make_a_detached_commit_look_reachable() {
    let fixture = Fixture::new();
    let linked = git::norm(&fixture.root.path().join("wt-grafted-detached"));
    run(
        &fixture.repo,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            &linked.to_string_lossy(),
        ],
    );
    std::fs::write(linked.join("work.txt"), "detached work\n").expect("work");
    run(&linked, &["add", "work.txt"]);
    run(
        &linked,
        &[
            "-c",
            "user.email=wt@h4rn.invalid",
            "-c",
            "user.name=wt",
            "commit",
            "-q",
            "-m",
            "detached work",
        ],
    );
    let detached = git::run(&linked, ["rev-parse", "HEAD"]).expect("detached tip");
    let main_head = git::run(&fixture.repo, ["rev-parse", "HEAD"]).expect("main head");
    graft(&fixture.repo, &main_head, &detached);
    let plain = std::process::Command::new("git")
        .args([
            "for-each-ref",
            "--contains",
            &detached,
            "--format=%(refname)",
            "refs/heads",
        ])
        .current_dir(&fixture.repo)
        .output()
        .expect("plain for-each-ref");
    assert!(
        !plain.stdout.is_empty(),
        "plain git thinks a branch reaches the detached commit"
    );

    let target = entry(&linked, &fixture.repo, None, false);
    assert_eq!(
        clear_one(&target, false, &fixture.data_dir())
            .err()
            .as_deref(),
        Some("detached HEAD has commits on no branch")
    );
    assert!(linked.is_dir());
}

#[test]
fn ignore_case_does_not_hide_an_untracked_case_variant() {
    let fixture = Fixture::new();
    // The scenario needs a case-sensitive filesystem (Linux; not the default
    // on macOS or Windows, where `NOTES` would be `notes`).
    let probe = fixture.root.path().join("case-probe");
    std::fs::write(&probe, "a").expect("probe");
    if fixture.root.path().join("CASE-PROBE").exists() {
        return;
    }
    let linked = fixture.add_worktree("wt-ignore-case", "ignore-case-branch");
    std::fs::write(linked.join("notes"), "tracked\n").expect("notes");
    run(&linked, &["add", "notes"]);
    run(&linked, &["commit", "-q", "-m", "notes"]);
    run(&linked, &["config", "core.ignoreCase", "true"]);
    // An untracked file that differs from a tracked one only by case.
    std::fs::write(linked.join("NOTES"), "untracked work\n").expect("NOTES");
    let plain = std::process::Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=all"])
        .current_dir(&linked)
        .output()
        .expect("plain status");
    assert!(
        plain.stdout.is_empty(),
        "with core.ignoreCase=true plain git misses the untracked file"
    );

    let target = entry(&linked, &fixture.repo, Some("ignore-case-branch"), false);
    assert_eq!(
        clear_one(&target, false, &fixture.data_dir())
            .err()
            .as_deref(),
        Some("has uncommitted changes")
    );
    assert!(linked.join("NOTES").is_file());
}

#[test]
fn a_redirected_global_config_does_not_hide_untracked_work() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let ignore_all = scratch.path().join("ignore-all");
    std::fs::write(&ignore_all, "*\n").expect("ignore file");
    let global = scratch.path().join("global-config");
    std::fs::write(
        &global,
        format!("[core]\n\texcludesFile = {}\n", ignore_all.display()),
    )
    .expect("global config");
    let mut command = std::process::Command::new(std::env::current_exe().expect("test binary"));
    command.args([
        "--exact",
        "worktrees::tests::untracked_work_under_a_redirected_global_config",
        "--ignored",
        "--test-threads=1",
    ]);
    command.env("GIT_CONFIG_GLOBAL", &global);
    let output = command.output().expect("run the child test");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "run by a_redirected_global_config_does_not_hide_untracked_work with GIT_CONFIG_GLOBAL set"]
fn untracked_work_under_a_redirected_global_config() {
    let global = std::env::var_os("GIT_CONFIG_GLOBAL")
        .expect("run through a_redirected_global_config_does_not_hide_untracked_work");
    // The fixture helpers run plain git, which would refuse to add ignored
    // files; build the scenario without the redirect.
    // SAFETY: single-threaded child process (`--test-threads=1`).
    unsafe { std::env::remove_var("GIT_CONFIG_GLOBAL") };
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-redirected-global", "redirected-global-branch");
    std::fs::write(linked.join("work.txt"), "untracked work\n").expect("work");
    // SAFETY: as above.
    unsafe { std::env::set_var("GIT_CONFIG_GLOBAL", &global) };
    let plain = std::process::Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=all"])
        .current_dir(&linked)
        .output()
        .expect("plain status");
    assert!(
        plain.stdout.is_empty(),
        "the redirected global config hides the untracked file from plain git"
    );

    let target = entry(
        &linked,
        &fixture.repo,
        Some("redirected-global-branch"),
        false,
    );
    assert_eq!(
        clear_one(&target, false, &fixture.data_dir())
            .err()
            .as_deref(),
        Some("has uncommitted changes")
    );
    assert!(linked.join("work.txt").is_file());
}

#[test]
fn the_result_card_lists_untracked_files_whatever_the_config() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-result-card", "result-card-branch");
    run(&linked, &["config", "status.showUntrackedFiles", "no"]);
    std::fs::write(linked.join("left-behind.txt"), "child work\n").expect("work");
    let changes = crate::subagents::worktree::changes(&linked);
    assert!(
        changes
            .iter()
            .any(|change| change.path == "left-behind.txt" && change.status == "??"),
        "{changes:?}"
    );
}

#[cfg(unix)]
fn chmod_x(file: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(file).expect("metadata").permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    std::fs::set_permissions(file, permissions).expect("chmod +x");
}

#[cfg(unix)]
#[test]
fn an_executable_bit_change_on_a_foreign_filesystem_is_kept() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-file-mode", "file-mode-branch");
    run(&linked, &["config", "core.fileMode", "false"]);
    chmod_x(&linked.join("README.md"));
    let plain = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&linked)
        .output()
        .expect("plain status");
    assert!(
        plain.stdout.is_empty(),
        "with core.fileMode=false plain git misses the mode change"
    );

    let target = entry(&linked, &fixture.repo, Some("file-mode-branch"), false);
    git::FORCE_FOREIGN_FILESYSTEM.with(|forced| forced.set(true));
    let outcome = clear_one(&target, false, &fixture.data_dir());
    git::FORCE_FOREIGN_FILESYSTEM.with(|forced| forced.set(false));
    assert_eq!(outcome.err().as_deref(), Some("has uncommitted changes"));
    assert!(linked.join("README.md").is_file());
}

#[cfg(unix)]
#[test]
fn a_symlink_replaced_by_a_file_on_a_foreign_filesystem_is_kept() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-symlinks", "symlinks-branch");
    std::os::unix::fs::symlink("README.md", linked.join("link")).expect("symlink");
    run(&linked, &["add", "link"]);
    run(&linked, &["commit", "-q", "-m", "a symlink"]);
    run(&linked, &["config", "core.symlinks", "false"]);
    // Replace the symlink with a plain file holding the same target text.
    std::fs::remove_file(linked.join("link")).expect("remove symlink");
    std::fs::write(linked.join("link"), "README.md").expect("plain file");
    let plain = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&linked)
        .output()
        .expect("plain status");
    assert!(
        plain.stdout.is_empty(),
        "with core.symlinks=false plain git misses the type change"
    );

    let target = entry(&linked, &fixture.repo, Some("symlinks-branch"), false);
    git::FORCE_FOREIGN_FILESYSTEM.with(|forced| forced.set(true));
    let outcome = clear_one(&target, false, &fixture.data_dir());
    git::FORCE_FOREIGN_FILESYSTEM.with(|forced| forced.set(false));
    assert_eq!(outcome.err().as_deref(), Some("has uncommitted changes"));
    assert!(linked.join("link").is_file());
}

#[cfg(unix)]
#[test]
fn an_executable_bit_change_is_kept_where_the_filesystem_supports_it() {
    // Same filesystem as the repository, but `core.fileMode=false` set by
    // hand: the probe finds executable bits supported, so the change counts.
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-own-fs", "own-fs-branch");
    run(&linked, &["config", "core.fileMode", "false"]);
    chmod_x(&linked.join("README.md"));
    if !executable_bits_stick(&linked.join("README.md")) {
        // A filesystem that ignores chmod (WSL drvfs, FAT): nothing to keep.
        return;
    }
    let target = entry(&linked, &fixture.repo, Some("own-fs-branch"), false);
    assert_eq!(
        clear_one(&target, false, &fixture.data_dir())
            .err()
            .as_deref(),
        Some("has uncommitted changes")
    );
    assert!(linked.join("README.md").is_file());
}

#[cfg(unix)]
#[test]
fn a_filesystem_without_executable_bits_keeps_gits_file_mode() {
    // On a filesystem that genuinely lacks executable bits (simulated), git's
    // `core.fileMode=false` stands, so ordinary clears still work there.
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-no-exec-fs", "no-exec-branch");
    run(&linked, &["config", "core.fileMode", "false"]);
    chmod_x(&linked.join("README.md"));
    let target = entry(&linked, &fixture.repo, Some("no-exec-branch"), false);
    git::FORCE_NO_MODE_SUPPORT.with(|forced| forced.set(true));
    assert!(git::mode_config(&linked).is_empty());
    let outcome = clear_one(&target, false, &fixture.data_dir());
    git::FORCE_NO_MODE_SUPPORT.with(|forced| forced.set(false));
    assert!(matches!(outcome, Ok(ClearOne::Removed)), "{outcome:?}");
}

#[test]
fn a_nested_clone_whose_git_dir_is_upper_case_is_never_removed() {
    // On a case-insensitive volume `.GIT` is a working git dir; the scan must
    // recognise it whatever the case (on a case-sensitive one it can only
    // over-report, which keeps the checkout).
    let fixture = Fixture::new();
    ignore(&fixture, "vendor/");
    let host = fixture.add_worktree("wt-upper-git-host", "upper-git-branch");
    let clone = host.join("vendor/other");
    init_repo(&clone);
    std::fs::rename(clone.join(".git"), clone.join(".GIT")).expect("rename to .GIT");
    let target = entry(&host, &fixture.repo, Some("upper-git-branch"), false);
    for force in [false, true] {
        assert_eq!(
            clear_one(&target, force, &fixture.data_dir())
                .err()
                .as_deref(),
            Some(format!("contains another git checkout at {}", clone.display()).as_str()),
            "force={force}"
        );
        assert!(clone.join(".GIT").is_dir());
    }
}

#[cfg(unix)]
#[test]
fn a_separate_upper_case_git_dir_at_the_root_is_another_checkout() {
    let fixture = Fixture::new();
    let host = fixture.add_worktree("wt-root-upper-git", "root-upper-git-branch");
    // Only possible on a case-sensitive volume: the checkout's own `.git`
    // file and a different `.GIT` directory side by side.
    if host.join(".GIT").exists() {
        return;
    }
    std::fs::create_dir(host.join(".GIT")).expect(".GIT dir");
    std::fs::write(host.join(".gitignore"), ".GIT/\n").expect("ignore it");
    run(&host, &["add", ".gitignore"]);
    run(&host, &["commit", "-q", "-m", "ignore .GIT"]);
    let target = entry(&host, &fixture.repo, Some("root-upper-git-branch"), false);
    assert_eq!(
        clear_one(&target, false, &fixture.data_dir())
            .err()
            .as_deref(),
        Some(format!("contains another git checkout at {}", host.display()).as_str())
    );
    assert!(host.join(".GIT").is_dir());
}

#[cfg(unix)]
#[test]
fn a_symlink_replaced_by_a_file_is_kept_where_the_filesystem_supports_it() {
    // Same filesystem, `core.symlinks=false` set by hand: the probe finds
    // symlinks supported, so the type change counts.
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-own-symlinks", "own-symlinks-branch");
    std::os::unix::fs::symlink("README.md", linked.join("link")).expect("symlink");
    run(&linked, &["add", "link"]);
    run(&linked, &["commit", "-q", "-m", "a symlink"]);
    run(&linked, &["config", "core.symlinks", "false"]);
    std::fs::remove_file(linked.join("link")).expect("remove symlink");
    std::fs::write(linked.join("link"), "README.md").expect("plain file");
    let target = entry(&linked, &fixture.repo, Some("own-symlinks-branch"), false);
    assert_eq!(
        clear_one(&target, false, &fixture.data_dir())
            .err()
            .as_deref(),
        Some("has uncommitted changes")
    );
    assert!(linked.join("link").is_file());
}

#[cfg(unix)]
#[test]
fn an_inconclusive_mode_probe_forces_both_comparisons_and_is_not_cached() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-readonly-admin", "readonly-admin-branch");
    let admin =
        PathBuf::from(git::run(&linked, ["rev-parse", "--absolute-git-dir"]).expect("admin dir"));
    let original = std::fs::metadata(&admin).expect("admin").permissions();
    std::fs::set_permissions(&admin, std::fs::Permissions::from_mode(0o555)).expect("read-only");
    // Root can write anyway; then the probe is conclusive and this premise
    // does not hold.
    let writable = std::fs::write(admin.join("h4rn-write-check"), b"").is_ok();
    let _ = std::fs::remove_file(admin.join("h4rn-write-check"));
    let read_only = git::mode_config(&linked).to_vec();
    std::fs::set_permissions(&admin, original).expect("restore");
    if writable {
        return;
    }
    assert_eq!(
        read_only,
        ["-c", "core.fileMode=true", "-c", "core.symlinks=true"],
        "an inconclusive probe forces both"
    );
    // Not cached as "unsupported": once writable, the real answer is found
    // (this filesystem supports both).
    assert_eq!(
        git::mode_config(&linked),
        ["-c", "core.fileMode=true", "-c", "core.symlinks=true"]
    );
}

#[cfg(unix)]
#[test]
fn a_filesystem_that_ignores_chmod_keeps_gits_file_mode() {
    // WSL drvfs without `metadata` or FAT: chmod succeeds but changes nothing
    // and every file reports 0777. Executable bits are not supported there,
    // so `core.fileMode` must not be forced (ordinary clears keep working).
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-synth-mode", "synth-mode-branch");
    git::FORCE_SYNTHESIZED_MODE.with(|forced| forced.set(true));
    let config = git::mode_config(&linked).to_vec();
    git::FORCE_SYNTHESIZED_MODE.with(|forced| forced.set(false));
    assert!(
        !config.contains(&"core.fileMode=true"),
        "a synthesised 0777 is not executable-bit support: {config:?}"
    );
    if symlinks_work(fixture.root.path()) {
        assert!(config.contains(&"core.symlinks=true"), "{config:?}");
    }
}

/// Whether a `chmod +x` on `file` actually stuck (and restores it).
#[cfg(unix)]
fn executable_bits_stick(file: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(file)
        .expect("metadata")
        .permissions()
        .mode();
    let probe = file.with_extension("h4rn-exec-probe");
    std::fs::write(&probe, b"").expect("probe");
    std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let off = std::fs::metadata(&probe)
        .expect("probe")
        .permissions()
        .mode()
        & 0o111
        == 0;
    let _ = std::fs::remove_file(&probe);
    off && mode & 0o111 != 0
}

/// Whether symlinks can be created under `dir`.
#[cfg(unix)]
fn symlinks_work(dir: &Path) -> bool {
    let link = dir.join("h4rn-symlink-check");
    let works = std::os::unix::fs::symlink("target", &link).is_ok();
    let _ = std::fs::remove_file(&link);
    works
}

#[cfg(unix)]
#[test]
fn only_a_definite_symlink_refusal_counts_as_unsupported_and_is_cached() {
    let fixture = Fixture::new();
    let linked = fixture.add_worktree("wt-symlink-errno", "symlink-errno-branch");
    let config_with = |errno: i32| {
        git::FORCE_SYMLINK_ERRNO.with(|forced| forced.set(Some(errno)));
        let config = git::mode_config(&linked).to_vec();
        git::FORCE_SYMLINK_ERRNO.with(|forced| forced.set(None));
        config
    };
    // A transient error after the scratch file was written, and `EPERM`
    // (which a security policy denying symlinks also returns): inconclusive,
    // so the comparison is forced and nothing is cached.
    let transient = config_with(libc::ENOSPC);
    let denied = config_with(libc::EPERM);
    // The definite refusal of a filesystem without symlinks: unsupported. That
    // it takes effect now proves neither earlier answer was cached.
    let refused = config_with(libc::EOPNOTSUPP);
    assert!(transient.contains(&"core.symlinks=true"), "{transient:?}");
    assert!(denied.contains(&"core.symlinks=true"), "{denied:?}");
    assert!(!refused.contains(&"core.symlinks=true"), "{refused:?}");
    // The conclusive answer is cached for this admin dir.
    assert_eq!(git::mode_config(&linked), refused.as_slice());
}

#[test]
fn a_staged_submodule_update_counts_even_when_the_submodule_is_ignored() {
    let fixture = Fixture::new();
    let upstream = fixture.root.path().join("upstream-staged-sub");
    init_repo(&upstream);
    let host = fixture.add_worktree("wt-staged-sub", "staged-sub-branch");
    run(
        &host,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            &upstream.to_string_lossy(),
            "sub",
        ],
    );
    run(&host, &["commit", "-q", "-m", "add a submodule"]);
    // A new submodule commit, staged in the superproject.
    let sub = host.join("sub");
    std::fs::write(sub.join("more.txt"), "more\n").expect("sub file");
    run(&sub, &["add", "more.txt"]);
    run(
        &sub,
        &[
            "-c",
            "user.email=wt@h4rn.invalid",
            "-c",
            "user.name=wt",
            "commit",
            "-q",
            "-m",
            "sub work",
        ],
    );
    run(&host, &["add", "sub"]);
    run(&host, &["config", "submodule.sub.ignore", "all"]);
    let plain = std::process::Command::new("git")
        .args(["diff-index", "--cached", "--quiet", "HEAD", "--"])
        .current_dir(&host)
        .status()
        .expect("plain diff-index");
    assert!(
        plain.success(),
        "with ignore = all plain git misses the staged update"
    );

    let admin =
        PathBuf::from(git::run(&host, ["rev-parse", "--absolute-git-dir"]).expect("admin dir"));
    assert!(
        !git::index_matches(
            &admin,
            "refs/heads/staged-sub-branch",
            Duration::from_secs(30)
        )
        .expect("index check"),
        "the staged submodule update is staged work"
    );
}

/// A scheduled draft that runs in `path` and keeps one thread.
fn scheduled_draft(path: &Path) -> h4rn_protocol::ScheduleDraft {
    h4rn_protocol::ScheduleDraft {
        name: "Keep this checkout".into(),
        host: HostId::new("host_local"),
        workspace_path: path.to_string_lossy().into_owned(),
        harness: HarnessKind::ClaudeCode,
        agent_id: None,
        model: "default".into(),
        runtime_mode: RuntimeMode::ApprovalRequired,
        prompt: "Keep working here".into(),
        schedule: h4rn_protocol::ScheduleSpec::Every {
            count: 1,
            unit: h4rn_protocol::IntervalUnit::Hours,
        },
        timezone: "UTC".into(),
        destination: h4rn_protocol::ScheduleDestination::ContinueThread,
        enabled: true,
    }
}

/// Runs a scheduled task once and waits for its thread to settle.
async fn run_scheduled(
    schedules: &crate::schedules::ScheduleService,
    threads: &ThreadManager,
    task: &h4rn_protocol::ScheduleTaskId,
) -> ThreadId {
    let run = schedules.run_now(task).await.expect("run now");
    let thread = run.thread_id.expect("thread");
    tokio::time::timeout(Duration::from_secs(5), async {
        while threads.thread_is_busy(&thread).await {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the scheduled run settles");
    thread
}

#[tokio::test]
async fn a_clear_keeps_a_thread_with_an_enabled_scheduled_task() {
    let fixture = Fixture::new();
    let first = fixture.add_worktree("wt-scheduled", "scheduled-branch");
    let (tracker, threads, _subagents) =
        managers(&fixture, vec![Arc::new(FakeHarness::new())]).await;
    let schedules = Arc::new(
        crate::schedules::ScheduleService::open(
            Arc::new(SqliteJournal::in_memory().await.expect("schedule journal")),
            Arc::clone(&threads),
            HostId::new("host_local"),
            Arc::new(crate::threads::SystemClock),
        )
        .await
        .expect("schedule service"),
    );
    let task = schedules
        .create(scheduled_draft(&first))
        .await
        .expect("create scheduled task");
    run_scheduled(&schedules, &threads, &task.task_id).await;
    tracker.set_schedule_service(Arc::clone(&schedules));

    let listed = tracker.list().await;
    let entry = listed
        .iter()
        .find(|entry| Path::new(&entry.path) == git::norm(&first))
        .unwrap_or_else(|| panic!("the scheduled thread workspace is listed: {listed:?}"))
        .clone();
    let outcome = tracker
        .clear(Some(vec![entry.path.clone()]), false, false)
        .await;
    assert!(outcome.removed.is_empty(), "{:?}", outcome.skipped);
    assert_eq!(outcome.skipped[0].reason, "has an enabled scheduled task");
    assert!(first.is_dir(), "the scheduled checkout is kept");

    // Pausing the task releases its worktree, as the docs promise.
    let paused = schedules
        .set_enabled(&task.task_id, false)
        .await
        .expect("pause the task");
    assert!(paused.next_run_at.is_none());
    let outcome = tracker
        .clear(Some(vec![entry.path.clone()]), false, false)
        .await;
    assert_eq!(outcome.removed, vec![entry.path.clone()], "{outcome:?}");
    assert!(
        !first.exists(),
        "a paused task no longer keeps the checkout"
    );

    // Deleting the task releases its worktree too.
    let second = fixture.add_worktree("wt-scheduled-deleted", "scheduled-deleted-branch");
    let task = schedules
        .create(scheduled_draft(&second))
        .await
        .expect("create a second scheduled task");
    run_scheduled(&schedules, &threads, &task.task_id).await;
    let listed = tracker.list().await;
    let entry = listed
        .iter()
        .find(|entry| Path::new(&entry.path) == git::norm(&second))
        .unwrap_or_else(|| panic!("the second workspace is listed: {listed:?}"))
        .clone();
    let outcome = tracker
        .clear(Some(vec![entry.path.clone()]), false, false)
        .await;
    assert!(outcome.removed.is_empty(), "{:?}", outcome.skipped);
    assert_eq!(outcome.skipped[0].reason, "has an enabled scheduled task");

    schedules
        .delete(&task.task_id)
        .await
        .expect("delete the task");
    let outcome = tracker
        .clear(Some(vec![entry.path.clone()]), false, false)
        .await;
    assert_eq!(outcome.removed, vec![entry.path.clone()], "{outcome:?}");
    assert!(
        !second.exists(),
        "a deleted task no longer keeps the checkout"
    );
}

#[tokio::test]
async fn a_disarmed_scheduled_task_releases_its_checkout() {
    let fixture = Fixture::new();
    let checkout = fixture.add_worktree("wt-disarmed", "disarmed-branch");
    let (tracker, threads, _subagents) =
        managers(&fixture, vec![Arc::new(FakeHarness::new())]).await;
    let journal: Arc<dyn Journal> =
        Arc::new(SqliteJournal::in_memory().await.expect("schedule journal"));
    let open = |journal: Arc<dyn Journal>, threads: Arc<ThreadManager>| {
        crate::schedules::ScheduleService::open(
            journal,
            threads,
            HostId::new("host_local"),
            Arc::new(crate::threads::SystemClock),
        )
    };
    let schedules = Arc::new(
        open(Arc::clone(&journal), Arc::clone(&threads))
            .await
            .expect("schedule service"),
    );
    let task = schedules
        .create(scheduled_draft(&checkout))
        .await
        .expect("create scheduled task");
    run_scheduled(&schedules, &threads, &task.task_id).await;
    tracker.set_schedule_service(Arc::clone(&schedules));
    let listed = tracker.list().await;
    let entry = listed
        .iter()
        .find(|entry| Path::new(&entry.path) == git::norm(&checkout))
        .unwrap_or_else(|| panic!("the scheduled thread workspace is listed: {listed:?}"))
        .clone();
    let outcome = tracker
        .clear(Some(vec![entry.path.clone()]), false, false)
        .await;
    assert_eq!(outcome.skipped[0].reason, "has an enabled scheduled task");

    // A stored schedule that cannot name its next slot disarms and pauses the
    // task; its checkout must be released with it.
    let mut broken = schedules.task(&task.task_id).await.expect("the task");
    broken.timezone = "Mars/Olympus_Mons".into();
    broken.next_run_at = Some(Utc::now() - chrono::Duration::minutes(1));
    journal
        .upsert_schedule(&broken)
        .await
        .expect("write the corrupted row");
    let reopened = Arc::new(
        open(journal, Arc::clone(&threads))
            .await
            .expect("reopen the schedule service"),
    );
    tracker.set_schedule_service(Arc::clone(&reopened));
    reopened.run_due().await;
    let disarmed = reopened.task(&task.task_id).await.expect("the task");
    assert!(!disarmed.enabled, "the disarmed task is paused");
    assert!(disarmed.next_run_at.is_none());
    let outcome = tracker
        .clear(Some(vec![entry.path.clone()]), false, false)
        .await;
    assert_eq!(outcome.removed, vec![entry.path.clone()], "{outcome:?}");
    assert!(
        !checkout.exists(),
        "a disarmed task no longer keeps the checkout"
    );
}
