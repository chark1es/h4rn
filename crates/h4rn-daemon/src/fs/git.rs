//! Git status via the `git` executable.
//!
//! Why shell out to `git` rather than link `git2`:
//!
//! - the user's repository is the source of truth, including its index,
//!   worktrees, sparse checkouts, `.gitignore` layering and hooks; the CLI and
//!   libgit2 differ on edge cases, and IDE mode should agree with what the
//!   user's terminal shows;
//! - h4rn already requires git for checkpoints and worktrees (ADR-022), so the
//!   binary is present wherever this feature is useful;
//! - it keeps the daemon binary small and avoids a second TLS/SSH stack.
//!
//! Failure is always soft: a missing repository, a detached `HEAD` or a bare
//! checkout yields no git status rather than breaking the editor.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use h4rn_protocol::GitFileStatus;

/// Git statuses keyed by workspace-relative path.
#[derive(Clone, Debug, Default)]
pub struct StatusMap {
    statuses: BTreeMap<String, GitFileStatus>,
    repo_root: Option<PathBuf>,
}

impl StatusMap {
    /// Status of one file, if git calls it dirty.
    pub fn for_path(&self, rel: &str) -> Option<GitFileStatus> {
        self.statuses.get(rel).copied()
    }

    /// Aggregate status for a directory: the most significant child state.
    ///
    /// A collapsed folder must still show that something under it changed, so
    /// the badge is the loudest status in the subtree, not a synthetic "dirty".
    pub fn for_directory(&self, rel: &str) -> Option<GitFileStatus> {
        let prefix = format!("{}/", rel.trim_end_matches('/'));
        let mut best: Option<GitFileStatus> = None;
        for (path, status) in &self.statuses {
            if !path.starts_with(&prefix) {
                continue;
            }
            if best.map(|current| priority(*status) > priority(current)) != Some(false) {
                best = Some(*status);
            }
        }
        best
    }

    /// Repository root git reported, when the workspace is a repository.
    pub fn repo_root(&self) -> Option<&Path> {
        self.repo_root.as_deref()
    }

    /// Whether any dirty path was found.
    pub fn is_dirty(&self) -> bool {
        !self.statuses.is_empty()
    }

    /// Number of dirty paths.
    pub fn len(&self) -> usize {
        self.statuses.len()
    }

    /// Whether git reported nothing.
    pub fn is_empty(&self) -> bool {
        self.statuses.is_empty()
    }
}

fn priority(status: GitFileStatus) -> u8 {
    match status {
        GitFileStatus::Untracked => 0,
        GitFileStatus::TypeChanged => 1,
        GitFileStatus::Renamed => 2,
        GitFileStatus::Added => 3,
        GitFileStatus::Deleted => 4,
        GitFileStatus::Modified => 5,
        GitFileStatus::Conflicted => 6,
    }
}

/// Reads `git status --porcelain=v1 -z` for a workspace.
///
/// Outside a repository (or with git missing) this returns an empty map; the
/// editor still works, it just shows no badges.
pub fn status_map(workspace: &Path) -> StatusMap {
    let repo_root = match repository_root(workspace) {
        Some(root) => root,
        None => return StatusMap::default(),
    };
    let Some(stdout) = run_git(
        workspace,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    ) else {
        return StatusMap::default();
    };
    let prefix = workspace
        .strip_prefix(&repo_root)
        .ok()
        .map(|path| {
            path.components()
                .map(|component| component.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/")
        })
        .unwrap_or_default();

    let mut statuses = BTreeMap::new();
    let mut parts = stdout
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .peekable();
    while let Some(part) = parts.next() {
        if part.len() < 4 {
            continue;
        }
        let x = part[0] as char;
        let y = part[1] as char;
        let rel = String::from_utf8_lossy(&part[3..]).into_owned();
        let mut rel = strip_prefix(&rel, &prefix);
        if rel.is_empty() {
            continue;
        }
        if (x == 'R' || x == 'C') && parts.peek().is_some() {
            // `-z` puts the original path next; the current path is the one we
            // key the badge on.
            let _original = parts.next();
        }
        if y == ' ' && (x == 'R' || x == 'C') {
            rel = rel.trim_start_matches('/').to_owned();
        }
        if let Some(status) = parse_xy(x, y) {
            statuses.insert(rel, status);
        }
    }
    StatusMap {
        statuses,
        repo_root: Some(repo_root),
    }
}

/// The repository root that contains `workspace`, if any.
pub fn repository_root(workspace: &Path) -> Option<PathBuf> {
    let stdout = run_git(workspace, &["rev-parse", "--show-toplevel"])?;
    let root = String::from_utf8_lossy(&stdout).trim().to_owned();
    if root.is_empty() {
        None
    } else {
        Some(PathBuf::from(root))
    }
}

/// The current branch, or `None` for a detached `HEAD`.
///
/// `symbolic-ref` is asked first so an unborn branch (a fresh `git init` with
/// no commits yet, which is what a new worktree looks like) still reports its
/// name; `rev-parse` covers older Git layouts where `symbolic-ref` is absent.
pub fn branch(workspace: &Path) -> Option<String> {
    let named = run_git(workspace, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .or_else(|| run_git(workspace, &["rev-parse", "--abbrev-ref", "HEAD"]))?;
    let branch = String::from_utf8_lossy(&named).trim().to_owned();
    if branch.is_empty() || branch == "HEAD" {
        None
    } else {
        Some(branch)
    }
}

/// Runs `git` in `workspace` and returns its stdout when it succeeds.
fn run_git(workspace: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

fn strip_prefix(path: &str, prefix: &str) -> String {
    if prefix.is_empty() {
        return path.to_owned();
    }
    path.strip_prefix(prefix)
        .map(|rest| rest.trim_start_matches('/').to_owned())
        .unwrap_or_default()
}

fn parse_xy(x: char, y: char) -> Option<GitFileStatus> {
    if x == '?' && y == '?' {
        return Some(GitFileStatus::Untracked);
    }
    if x == 'U' || y == 'U' || (x == 'A' && y == 'A') || (x == 'D' && y == 'D') {
        return Some(GitFileStatus::Conflicted);
    }
    let from_worktree = match y {
        'M' => Some(GitFileStatus::Modified),
        'D' => Some(GitFileStatus::Deleted),
        'A' => Some(GitFileStatus::Added),
        'R' => Some(GitFileStatus::Renamed),
        'T' => Some(GitFileStatus::TypeChanged),
        _ => None,
    };
    if from_worktree.is_some() {
        return from_worktree;
    }
    match x {
        'M' => Some(GitFileStatus::Modified),
        'D' => Some(GitFileStatus::Deleted),
        'A' => Some(GitFileStatus::Added),
        'R' => Some(GitFileStatus::Renamed),
        'C' => Some(GitFileStatus::Added),
        'T' => Some(GitFileStatus::TypeChanged),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_letters_map_to_badges() {
        assert_eq!(parse_xy(' ', 'M'), Some(GitFileStatus::Modified));
        assert_eq!(parse_xy('A', ' '), Some(GitFileStatus::Added));
        assert_eq!(parse_xy('?', '?'), Some(GitFileStatus::Untracked));
        assert_eq!(parse_xy('U', 'U'), Some(GitFileStatus::Conflicted));
        assert_eq!(parse_xy(' ', ' '), None);
    }

    #[test]
    fn directory_badges_use_the_loudest_child() {
        let mut statuses = BTreeMap::new();
        statuses.insert("src/a.ts".to_owned(), GitFileStatus::Untracked);
        statuses.insert("src/b.ts".to_owned(), GitFileStatus::Modified);
        statuses.insert("docs/readme.md".to_owned(), GitFileStatus::Added);
        let map = StatusMap {
            statuses,
            repo_root: None,
        };
        assert_eq!(map.for_directory("src"), Some(GitFileStatus::Modified));
        assert_eq!(map.for_directory("docs"), Some(GitFileStatus::Added));
        assert_eq!(map.for_directory("other"), None);
    }
}
