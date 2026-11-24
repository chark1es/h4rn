//! Creating a git worktree for a new thread.
//!
//! A thread that asks for isolation gets a checkout of its project's current
//! `HEAD` on its own branch, and the thread's workspace becomes that checkout.
//! The main checkout is only read: every git command here runs `-C` against
//! it, and `git worktree add` never touches its index or working tree.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use h4rn_protocol::WorktreeRequest;

use super::git;
use super::settings::{WorktreeSettingsStore, expand_location, validate_location};

/// How long `git worktree add` and submodule initialisation may take.
const CREATE_TIMEOUT: Duration = Duration::from_secs(120);

/// A worktree created for a thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatedWorktree {
    /// Absolute checkout path.
    pub path: PathBuf,
    /// Main worktree of the repository.
    pub repo: PathBuf,
    /// The branch checked out there.
    pub branch: String,
    /// Whether h4rn made the branch, and so may delete it when rolling back.
    pub created_branch: bool,
}

impl CreatedWorktree {
    /// Best-effort removal after the thread this was made for never started.
    pub fn rollback(&self) {
        let branch = self.created_branch.then_some(self.branch.as_str());
        crate::subagents::worktree::remove(&self.repo, &self.path, branch);
    }
}

/// Creates thread worktrees with this machine's saved settings.
#[derive(Clone)]
pub struct WorktreeCreator {
    data_dir: PathBuf,
    settings: Arc<WorktreeSettingsStore>,
}

impl WorktreeCreator {
    /// A creator rooted at the daemon's data directory.
    pub fn new(data_dir: PathBuf, settings: Arc<WorktreeSettingsStore>) -> Self {
        Self { data_dir, settings }
    }

    /// Creates the requested worktree, off the async runtime.
    pub async fn create_for_thread(
        &self,
        workspace: &str,
        request: &WorktreeRequest,
        prompt: &str,
    ) -> Result<CreatedWorktree, String> {
        let workspace = PathBuf::from(workspace);
        let settings = self.settings.get();
        let data_dir = self.data_dir.clone();
        let prompt = prompt.to_owned();
        let request = request.clone();
        // Every probe and `git worktree add` shells out and blocks.
        tokio::task::spawn_blocking(move || {
            create_blocking(
                &workspace,
                &request,
                &prompt,
                &settings,
                &data_dir,
                dirs::home_dir().as_deref(),
            )
        })
        .await
        .map_err(|error| format!("the worktree setup task failed: {error}"))?
    }
}

/// [`WorktreeCreator::create_for_thread`] as a blocking call.
///
/// `home` expands a `~` location; tests pass their own so no real home folder
/// is touched.
pub fn create_blocking(
    workspace: &Path,
    request: &WorktreeRequest,
    prompt: &str,
    settings: &h4rn_protocol::WorktreeSettings,
    data_dir: &Path,
    home: Option<&Path>,
) -> Result<CreatedWorktree, String> {
    if !workspace.is_dir() {
        return Err(format!(
            "the workspace {} does not exist on this host",
            workspace.display()
        ));
    }
    // Distinguish "git is missing" from "this is not a repository": both
    // probes fail, but only one is something the person can fix by installing
    // git.
    if let Err(error) = git::run_with_timeout(workspace, ["--version"], CREATE_TIMEOUT)
        && error.contains("could not run git")
    {
        return Err("git is not installed or could not be run on this host".to_owned());
    }
    if !git::is_git_repo(workspace) {
        return Err(format!("{} is not a git repository", workspace.display()));
    }
    let repo = git::toplevel(workspace)
        .ok_or_else(|| format!("{} is not a git repository", workspace.display()))?;

    let branches = git::run_with_timeout(
        &repo,
        ["for-each-ref", "--format=%(refname:short)", "refs/heads"],
        CREATE_TIMEOUT,
    )
    .map_err(|error| format!("could not list the repository's branches: {error}"))?;
    let branches: std::collections::HashSet<&str> = branches.lines().collect();
    let worktrees = git::list_worktrees(&repo)
        .map_err(|error| format!("could not read the repository's worktrees: {error}"))?;

    let location = match settings.location.as_deref() {
        Some(location) => {
            validate_location(location)?;
            expand_location(location, home)?
        }
        None => data_dir.join("worktrees"),
    };
    let project = project_name(&repo);

    let (branch, created_branch) = match request.branch().map(str::trim) {
        Some(named) if !named.is_empty() => {
            if !valid_branch(&repo, named)? {
                return Err(format!("`{named}` is not a valid branch name"));
            }
            // `list_worktrees` names linked checkouts only, so the main
            // checkout's own branch is asked for separately.
            let main_branch = git::run_with_timeout(
                &repo,
                ["symbolic-ref", "--quiet", "--short", "HEAD"],
                CREATE_TIMEOUT,
            )
            .ok();
            if main_branch.as_deref() == Some(named) {
                return Err(format!(
                    "the branch `{named}` is already checked out at {}",
                    repo.display()
                ));
            }
            if let Some(existing) = worktrees
                .iter()
                .find(|worktree| worktree.branch.as_deref() == Some(named))
            {
                return Err(format!(
                    "the branch `{named}` is already checked out at {}",
                    existing.path.display()
                ));
            }
            // An existing branch is attached, not recreated, so its history
            // is the one the person asked for.
            (named.to_owned(), !branches.contains(named))
        }
        _ => {
            let generated =
                generated_branch(settings, prompt, &branches, &worktrees, &location, &project);
            (generated, true)
        }
    };

    let dest = checkout_path(&location, &project, &branch);
    if dest.exists() {
        return Err(format!("a folder already exists at {}", dest.display()));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            format!(
                "could not create the worktree folder {}: {error}",
                parent.display()
            )
        })?;
    }
    let dest_text = dest.to_string_lossy().into_owned();
    let add = if created_branch {
        git::run_with_timeout(
            &repo,
            ["worktree", "add", "-b", &branch, &dest_text],
            CREATE_TIMEOUT,
        )
    } else {
        git::run_with_timeout(
            &repo,
            ["worktree", "add", &dest_text, &branch],
            CREATE_TIMEOUT,
        )
    };
    if let Err(error) = add {
        // A failed add can leave the folder behind; the registration is not
        // there, so removing the directory is enough.
        let _ = std::fs::remove_dir_all(&dest);
        return Err(format!(
            "could not create the worktree at {}: {error}",
            dest.display()
        ));
    }

    // New worktrees start without submodule contents. A repository that
    // declares them gets a full recursive checkout, so the agent does not run
    // against empty folders that look like real ones.
    if dest.join(".gitmodules").is_file()
        && let Err(error) = git::run_with_timeout(&dest, submodule_args(&dest), CREATE_TIMEOUT)
    {
        let created = CreatedWorktree {
            path: dest,
            repo,
            branch,
            created_branch,
        };
        created.rollback();
        return Err(format!(
            "the worktree was created but its submodules could not be initialised: {error}"
        ));
    }

    Ok(CreatedWorktree {
        path: dest,
        repo,
        branch,
        created_branch,
    })
}

/// `git submodule update` with the local-path permission a checkout of a
/// repository with local submodules needs.
///
/// Git refuses the `file` transport for submodules unless told otherwise
/// (CVE-2022-39253). A repository whose `.gitmodules` names a relative,
/// absolute or `file:` URL is a local checkout the person already has, so that
/// one command may use local paths; a repository with only network remotes is
/// left at git's stricter default.
fn submodule_args(dest: &Path) -> Vec<&'static str> {
    let needs_file = std::fs::read_to_string(dest.join(".gitmodules"))
        .map(|text| {
            text.lines().any(|line| {
                let Some((key, value)) = line.split_once('=') else {
                    return false;
                };
                if key.trim() != "url" {
                    return false;
                }
                let url = value.trim();
                url.starts_with("file:")
                    || url.starts_with("./")
                    || url.starts_with("../")
                    || Path::new(url).is_absolute()
            })
        })
        .unwrap_or(false);
    if needs_file {
        vec![
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
            "--recursive",
        ]
    } else {
        vec!["submodule", "update", "--init", "--recursive"]
    }
}

/// The daemon-generated branch for `prompt`, with a short suffix when the
/// obvious name is taken.
fn generated_branch(
    settings: &h4rn_protocol::WorktreeSettings,
    prompt: &str,
    branches: &std::collections::HashSet<&str>,
    worktrees: &[git::GitWorktree],
    location: &Path,
    project: &str,
) -> String {
    let prefix = branch_prefix(&settings.branch_prefix);
    let slug = prompt_slug(prompt);
    let base = format!("{prefix}{slug}");
    for suffix in 1..=1000 {
        let candidate = if suffix == 1 {
            base.clone()
        } else {
            format!("{base}-{suffix}")
        };
        if !is_valid_branch_name(&candidate) {
            continue;
        }
        if branches.contains(candidate.as_str()) {
            continue;
        }
        if worktrees
            .iter()
            .any(|worktree| worktree.branch.as_deref() == Some(candidate.as_str()))
        {
            continue;
        }
        if checkout_path(location, project, &candidate).exists() {
            continue;
        }
        return candidate;
    }
    // Every readable name in the window is taken; name it by the folder the
    // person is likely to recognise instead of failing.
    let base = format!("{prefix}thread-{}", uuid::Uuid::now_v7().simple());
    base.chars()
        .take(80)
        .collect::<String>()
        .trim_end_matches(['/', '.'])
        .to_owned()
}

/// Normalises the configured prefix: invalid characters become hyphens, the
/// trailing slash is optional, and an empty prefix adds nothing.
pub fn branch_prefix(prefix: &str) -> String {
    let mut cleaned = String::new();
    for character in prefix.trim().chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | '/') {
            cleaned.push(character);
        } else {
            cleaned.push('-');
        }
    }
    while cleaned.contains("//") {
        cleaned = cleaned.replace("//", "/");
    }
    let cleaned = cleaned.trim_start_matches('-').trim_matches('/');
    if cleaned.is_empty() {
        return String::new();
    }
    format!("{cleaned}/")
}

/// A short, readable slug for the branch from the first prompt.
fn prompt_slug(prompt: &str) -> String {
    let mut slug = String::new();
    for word in prompt.split_whitespace().take(8) {
        let cleaned: String = word
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        let cleaned = cleaned.trim_matches('-');
        if cleaned.is_empty() {
            continue;
        }
        if !slug.is_empty() {
            slug.push('-');
        }
        slug.push_str(cleaned);
        if slug.len() >= 32 {
            break;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "thread".to_owned()
    } else {
        slug.chars()
            .take(40)
            .collect::<String>()
            .trim_end_matches(['-', '.'])
            .to_owned()
    }
}

/// Where a branch's checkout lives: one folder per branch, so a branch with a
/// `/` does not become a surprise directory tree.
fn checkout_path(location: &Path, project: &str, branch: &str) -> PathBuf {
    location.join(project).join(folder_name(branch))
}

/// A folder-safe name for a branch.
fn folder_name(branch: &str) -> String {
    let mut name = String::new();
    for character in branch.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
            name.push(character);
        } else {
            name.push('-');
        }
    }
    let name = name.trim_matches(['-', '.', ' ']);
    if name.is_empty() {
        "worktree".to_owned()
    } else {
        name.to_owned()
    }
}

/// The project folder name: the main worktree's folder.
fn project_name(repo: &Path) -> String {
    let main = git::repo_of(repo).unwrap_or_else(|| repo.to_path_buf());
    main.file_name()
        .and_then(|name| name.to_str())
        .map(folder_name)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "project".to_owned())
}

/// Whether `git` accepts `name` as a branch.
fn valid_branch(repo: &Path, name: &str) -> Result<bool, String> {
    if name.starts_with('-') {
        return Ok(false);
    }
    match git::run_with_timeout(repo, ["check-ref-format", "--branch", name], CREATE_TIMEOUT) {
        Ok(_) => Ok(true),
        // `check-ref-format` reports an invalid name as a plain failure.
        Err(error) if error.contains("could not run git") => Err(error),
        Err(_) => Ok(false),
    }
}

/// Generated names are built from safe pieces; this is the final guard that
/// the result is still a name git will accept.
fn is_valid_branch_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.ends_with('.')
        && !name.contains("..")
        && !name.contains("@{")
        && !name.contains("//")
        && name.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with('.')
                && !part.ends_with(".lock")
                && part.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | '/')
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("repo");
        run(dir.path(), &["init", "-q", "-b", "main"]);
        run(dir.path(), &["config", "user.email", "wt@example.invalid"]);
        run(dir.path(), &["config", "user.name", "worktree test"]);
        std::fs::write(dir.path().join("README.md"), "hello\n").expect("readme");
        run(dir.path(), &["add", "README.md"]);
        run(dir.path(), &["commit", "-q", "-m", "init"]);
        dir
    }

    fn settings(location: Option<&Path>) -> h4rn_protocol::WorktreeSettings {
        h4rn_protocol::WorktreeSettings {
            location: location.map(|path| path.to_string_lossy().into_owned()),
            ..h4rn_protocol::WorktreeSettings::default()
        }
    }

    #[test]
    fn generated_branch_uses_the_prefix_and_prompt() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let created = create_blocking(
            repo.path(),
            &WorktreeRequest::Generated(true),
            "Fix the login loop please",
            &settings(None),
            data.path(),
            None,
        )
        .expect("create");
        assert_eq!(created.branch, "h4rn/fix-the-login-loop-please");
        assert!(created.path.is_dir());
        assert_eq!(
            created.path,
            checkout_path(
                &data.path().join("worktrees"),
                &project_name(repo.path()),
                "h4rn/fix-the-login-loop-please"
            )
        );
        assert_eq!(
            git::toplevel(&created.path).as_deref(),
            Some(created.path.as_path())
        );
    }

    #[test]
    fn a_named_branch_is_attached_as_written() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let created = create_blocking(
            repo.path(),
            &WorktreeRequest::Named("feature/readme".into()),
            "anything",
            &settings(None),
            data.path(),
            None,
        )
        .expect("create");
        assert_eq!(created.branch, "feature/readme");
        assert!(created.created_branch);
        let branch = git::run_with_timeout(
            &created.path,
            ["rev-parse", "--abbrev-ref", "HEAD"],
            CREATE_TIMEOUT,
        )
        .expect("branch");
        assert_eq!(branch, "feature/readme");
    }

    #[test]
    fn an_empty_prefix_generates_a_bare_slug() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let created = create_blocking(
            repo.path(),
            &WorktreeRequest::Generated(true),
            "No prefix here",
            &h4rn_protocol::WorktreeSettings {
                branch_prefix: String::new(),
                ..h4rn_protocol::WorktreeSettings::default()
            },
            data.path(),
            None,
        )
        .expect("create");
        assert_eq!(created.branch, "no-prefix-here");
    }

    #[test]
    fn a_taken_generated_name_gets_a_suffix() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        run(repo.path(), &["branch", "h4rn/same-task"]);
        let created = create_blocking(
            repo.path(),
            &WorktreeRequest::Generated(true),
            "same task",
            &settings(None),
            data.path(),
            None,
        )
        .expect("create");
        assert_eq!(created.branch, "h4rn/same-task-2");
    }

    #[test]
    fn a_branch_checked_out_elsewhere_is_refused() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        run(repo.path(), &["branch", "busy-branch"]);
        let other = data.path().join("other");
        run(
            repo.path(),
            &[
                "worktree",
                "add",
                "-q",
                other.to_str().unwrap(),
                "busy-branch",
            ],
        );
        let error = create_blocking(
            repo.path(),
            &WorktreeRequest::Named("busy-branch".into()),
            "task",
            &settings(None),
            data.path(),
            None,
        )
        .expect_err("refused");
        assert!(error.contains("already checked out"), "{error}");
    }

    #[test]
    fn a_branch_checked_out_in_the_main_checkout_is_refused() {
        let repo = repo();
        let data = tempfile::tempdir().expect("data");
        let error = create_blocking(
            repo.path(),
            &WorktreeRequest::Named("main".into()),
            "task",
            &settings(None),
            data.path(),
            None,
        )
        .expect_err("the main checkout's own branch cannot be checked out twice");
        assert!(error.contains("already checked out"), "{error}");
    }

    #[test]
    fn a_non_git_folder_is_refused() {
        let folder = tempfile::tempdir().expect("folder");
        let data = tempfile::tempdir().expect("data");
        let error = create_blocking(
            folder.path(),
            &WorktreeRequest::Generated(true),
            "task",
            &settings(None),
            data.path(),
            None,
        )
        .expect_err("refused");
        assert!(error.contains("not a git repository"), "{error}");
    }

    #[test]
    fn the_custom_location_is_used_and_tilde_expands_against_home() {
        let repo = repo();
        let custom = tempfile::tempdir().expect("custom");
        let data = tempfile::tempdir().expect("data");
        let created = create_blocking(
            repo.path(),
            &WorktreeRequest::Generated(true),
            "custom place",
            &settings(Some(custom.path())),
            data.path(),
            None,
        )
        .expect("create");
        assert!(created.path.starts_with(custom.path()));
        assert!(!created.path.starts_with(data.path()));

        let home = tempfile::tempdir().expect("home");
        let settings = h4rn_protocol::WorktreeSettings {
            location: Some("~/checkouts".into()),
            ..h4rn_protocol::WorktreeSettings::default()
        };
        let created = create_blocking(
            repo.path(),
            &WorktreeRequest::Generated(true),
            "home place",
            &settings,
            data.path(),
            Some(home.path()),
        )
        .expect("create");
        assert!(
            created.path.starts_with(home.path().join("checkouts")),
            "{}",
            created.path.display()
        );
    }

    #[test]
    fn a_repository_with_submodules_gets_them_checked_out() {
        let sub = tempfile::tempdir().expect("submodule");
        run(sub.path(), &["init", "-q", "-b", "main"]);
        run(sub.path(), &["config", "user.email", "wt@example.invalid"]);
        run(sub.path(), &["config", "user.name", "worktree test"]);
        std::fs::write(sub.path().join("inner.txt"), "inside\n").expect("inner");
        run(sub.path(), &["add", "inner.txt"]);
        run(sub.path(), &["commit", "-q", "-m", "sub init"]);

        let repo = repo();
        // The submodule lives in another temp folder, so git needs to be told
        // this repository's local `file` remotes are trusted.
        run(repo.path(), &["config", "protocol.file.allow", "always"]);
        run(
            repo.path(),
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                sub.path().to_str().unwrap(),
                "sub",
            ],
        );
        run(repo.path(), &["commit", "-q", "-m", "add submodule"]);

        let data = tempfile::tempdir().expect("data");
        let created = create_blocking(
            repo.path(),
            &WorktreeRequest::Generated(true),
            "with a submodule",
            &settings(None),
            data.path(),
            None,
        )
        .expect("create");
        assert!(
            created.path.join("sub").join("inner.txt").is_file(),
            "the submodule is checked out in the worktree"
        );
    }

    #[test]
    fn invalid_prefix_characters_become_hyphens() {
        assert_eq!(branch_prefix("feat:"), "feat-/");
        assert_eq!(branch_prefix("feat"), "feat/");
        assert_eq!(branch_prefix("feat/"), "feat/");
        assert_eq!(branch_prefix(""), "");
        assert_eq!(branch_prefix("  "), "");
        assert_eq!(branch_prefix("a  b"), "a--b/");
    }
}
