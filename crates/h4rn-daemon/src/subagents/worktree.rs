//! Git worktrees for isolated subagents (ADR-039).
//!
//! A worktree-isolated child runs in `<data_dir>/subagents/<subagent id>/worktree`
//! on its own branch. The parent workspace keeps its checkout, so the child can
//! build and test without fighting the parent's uncommitted work. On settle the
//! worktree directory is removed but the branch is kept; cancelling removes the
//! branch too, because nobody asked for that work.

use std::path::{Path, PathBuf};
use std::process::Command;

use h4rn_protocol::SubagentFileChange;

/// A `git` command that ignores inherited repository-location variables
/// (`GIT_DIR`, `GIT_INDEX_FILE`, ...), so it always acts on the repository it
/// names with `-C` (a daemon started from a hook would otherwise inherit them).
fn git_command() -> Command {
    let mut command = Command::new("git");
    crate::worktrees::git::sanitize(&mut command);
    command
}

/// Whether `path` is inside a git work tree.
pub fn is_git_repo(path: &Path) -> bool {
    git_command()
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Creates a worktree at `dest` on a new `branch` from `repo`'s `HEAD`.
pub fn create(repo: &Path, dest: &Path, branch: &str) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            format!(
                "could not create the subagent worktree directory {}: {error}",
                parent.display()
            )
        })?;
    }
    let output = git_command()
        .arg("-C")
        .arg(repo)
        .args(["worktree", "add", "-b", branch])
        .arg(dest)
        .output()
        .map_err(|error| format!("could not run git worktree add: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "git worktree add failed for {}: {}",
            dest.display(),
            stderr.trim()
        ))
    }
}

/// Removes a worktree, and optionally its branch.
///
/// Best-effort: cleanup failures are logged by the caller rather than turning a
/// finished delegation into a failure the user cannot act on.
pub fn remove(repo: &Path, dest: &Path, branch: Option<&str>) {
    let _ = git_command()
        .arg("-C")
        .arg(repo)
        .args(["worktree", "remove", "--force"])
        .arg(dest)
        .output();
    // A worktree directory that was already removed still leaves git metadata;
    // prune keeps the parent repository's `git worktree list` honest.
    let _ = git_command()
        .arg("-C")
        .arg(repo)
        .args(["worktree", "prune"])
        .output();
    if let Some(branch) = branch {
        let _ = git_command()
            .arg("-C")
            .arg(repo)
            .args(["branch", "-D", branch])
            .output();
    }
    if dest.exists() {
        let _ = std::fs::remove_dir_all(dest);
    }
}

/// Files changed in a worktree, read from `git status --porcelain`.
///
/// Call this **before** [`remove`]; it is what the parent's result card shows.
pub fn changes(worktree: &Path) -> Vec<SubagentFileChange> {
    // The same overrides as the clearing checks, so the result card does not
    // under-report under `status.showUntrackedFiles=no` or a filesystem
    // monitor.
    let output = git_command()
        .arg("-C")
        .arg(worktree)
        .args(crate::worktrees::git::SAFETY_CONFIG)
        // `normal` collapses an untracked directory to one row, so a child's
        // `dist/` cannot flood the result card, while still overriding a
        // config that hides untracked files.
        .args([
            "status",
            "--porcelain",
            "--untracked-files=normal",
            "--ignore-submodules=none",
        ])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_porcelain_line)
        .collect()
}

fn parse_porcelain_line(line: &str) -> Option<SubagentFileChange> {
    if line.len() < 4 {
        return None;
    }
    let status = line.get(..2)?.trim().to_owned();
    let mut path = line[3..].trim().to_owned();
    // Renames are `old -> new`; the new path is what the parent cares about.
    if let Some((_, new)) = path.rsplit_once(" -> ") {
        path = new.trim_matches('"').to_owned();
    } else {
        path = path.trim_matches('"').to_owned();
    }
    (!path.is_empty()).then_some(SubagentFileChange { path, status })
}

/// The worktree path for one subagent under the daemon data directory.
pub fn worktree_path(data_dir: &Path, subagent_id: &str) -> PathBuf {
    data_dir
        .join("subagents")
        .join(subagent_id)
        .join("worktree")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_lines_map_to_paths_and_statuses() {
        assert_eq!(
            parse_porcelain_line(" M src/lib.rs"),
            Some(SubagentFileChange {
                path: "src/lib.rs".into(),
                status: "M".into(),
            })
        );
        assert_eq!(
            parse_porcelain_line("?? notes.md"),
            Some(SubagentFileChange {
                path: "notes.md".into(),
                status: "??".into(),
            })
        );
        assert_eq!(
            parse_porcelain_line("R  old.rs -> new.rs"),
            Some(SubagentFileChange {
                path: "new.rs".into(),
                status: "R".into(),
            })
        );
        assert_eq!(parse_porcelain_line(""), None);
    }

    #[test]
    fn worktree_paths_live_under_the_subagent_directory() {
        let path = worktree_path(Path::new("/data"), "sa_123");
        assert_eq!(path, PathBuf::from("/data/subagents/sa_123/worktree"));
    }
}
