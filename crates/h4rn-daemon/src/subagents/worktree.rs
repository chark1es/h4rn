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
    use crate::worktrees::git::run_with_timeout;
    // Each call has the tracker's per-command deadline, so a wedged checkout
    // cannot leave finalize (and the subagent) stuck.
    let _ = run_with_timeout(
        repo,
        [
            std::ffi::OsStr::new("worktree"),
            std::ffi::OsStr::new("remove"),
            std::ffi::OsStr::new("--force"),
            dest.as_os_str(),
        ],
        GIT_TIMEOUT,
    );
    // A worktree directory that was already removed still leaves git metadata;
    // prune keeps the parent repository's `git worktree list` honest.
    let _ = run_with_timeout(repo, ["worktree", "prune"], GIT_TIMEOUT);
    if let Some(branch) = branch {
        let _ = run_with_timeout(repo, ["branch", "-D", branch], GIT_TIMEOUT);
    }
    if dest.exists() {
        let _ = std::fs::remove_dir_all(dest);
    }
}

/// Files changed in a worktree, read from `git status --porcelain`.
///
/// Call this **before** [`remove`]; it is what the parent's result card shows.
pub fn changes(worktree: &Path) -> Vec<SubagentFileChange> {
    use crate::worktrees::git;
    // The same overrides as the clearing checks (including the mode checks for
    // a checkout on another filesystem), so the result card does not
    // under-report, and the same per-command deadline, so a wedged checkout
    // cannot stall finalize. `normal` collapses an untracked directory to one
    // row, so a child's `dist/` cannot flood the result card. `-z` keeps paths
    // exactly as stored: no quoting, no ambiguous ` -> ` in a rename.
    let args = git::SAFETY_CONFIG
        .into_iter()
        .chain(git::mode_config(worktree).iter().copied())
        .chain([
            "status",
            "--porcelain",
            "-z",
            "--untracked-files=normal",
            "--ignore-submodules=none",
        ]);
    let Ok(stdout) = git::run_bytes_with_timeout(worktree, args, GIT_TIMEOUT) else {
        return Vec::new();
    };
    parse_porcelain_z(&stdout)
}

/// How long each git call here may take.
const GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Parses `git status --porcelain -z`: NUL-terminated `XY path` records, where
/// a rename or copy is followed by one more field holding the original path.
fn parse_porcelain_z(output: &[u8]) -> Vec<SubagentFileChange> {
    let mut changes = Vec::new();
    let mut fields = output.split(|byte| *byte == 0);
    while let Some(field) = fields.next() {
        if field.len() < 4 {
            continue;
        }
        let status = String::from_utf8_lossy(&field[..2]).trim().to_owned();
        // Valid UTF-8 verbatim; anything else escaped (`\xNN`) rather than
        // replaced, so two distinct non-UTF-8 names stay distinct.
        let path = match std::str::from_utf8(&field[3..]) {
            Ok(path) => path.to_owned(),
            Err(_) => field[3..].escape_ascii().to_string(),
        };
        if status.starts_with('R') || status.starts_with('C') {
            // The original path; the parent cares about the new one.
            fields.next();
        }
        if !path.is_empty() {
            changes.push(SubagentFileChange { path, status });
        }
    }
    changes
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
    fn porcelain_records_map_to_paths_and_statuses() {
        let output = b" M src/lib.rs\0?? notes.md\0R  new.rs\0old.rs\0?? spa ce -> x.txt\0";
        assert_eq!(
            parse_porcelain_z(output),
            vec![
                SubagentFileChange {
                    path: "src/lib.rs".into(),
                    status: "M".into(),
                },
                SubagentFileChange {
                    path: "notes.md".into(),
                    status: "??".into(),
                },
                SubagentFileChange {
                    path: "new.rs".into(),
                    status: "R".into(),
                },
                SubagentFileChange {
                    path: "spa ce -> x.txt".into(),
                    status: "??".into(),
                },
            ]
        );
        assert!(parse_porcelain_z(b"").is_empty());
        let distinct = parse_porcelain_z(b"?? note-\xff\0?? note-\xfe\0");
        assert_eq!(distinct.len(), 2);
        assert_ne!(distinct[0].path, distinct[1].path);
        assert_eq!(distinct[0].path, "note-\\xff");
    }

    #[test]
    fn worktree_paths_live_under_the_subagent_directory() {
        let path = worktree_path(Path::new("/data"), "sa_123");
        assert_eq!(path, PathBuf::from("/data/subagents/sa_123/worktree"));
    }
}
