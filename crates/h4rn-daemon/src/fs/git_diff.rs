//! Branch and commit review on top of the `git` executable.
//!
//! `git diff` answers what changed since `HEAD`; this module answers the two
//! wider questions a review asks:
//!
//! - **What changed on this branch?** Work (commits and the working tree)
//!   against the merge base with a base branch, so commits made on the base
//!   after the fork stay out of the review.
//! - **What did this commit change?** One commit against its first parent, a
//!   root commit against the empty tree. A merge commit is never compared with
//!   both parents, because that would show work the merge did not author.
//!
//! Every list is built from real git output. Requests that name a ref or a
//! commit are validated here and passed after `--end-of-options`, so user text
//! can never become a git option. Both sides of a file diff are read with a
//! size cap; an oversized side is reported as such instead of being loaded.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

use chrono::{DateTime, Utc};
use h4rn_protocol::{FsDiffCommit, FsDiffCommitList, FsDiffFile, GitFileStatus};

use super::git::repository_root;
use super::paths::{FsError, MAX_READ_BYTES};
use super::text;

/// The most files one range list returns.
pub const MAX_DIFF_FILES: usize = 2_000;
/// The most commits the commit list returns.
pub const MAX_DIFF_COMMITS: usize = 100;
/// The most bytes the untracked-file pass reads to count lines.
///
/// Untracked files have no `git diff` numstat to ask, so the daemon reads them.
/// One person's build output can be many gigabytes; past this budget files are
/// still listed, just without line counts.
pub const MAX_UNTRACKED_BYTES: u64 = 2 * 1024 * 1024;
/// Git's well-known empty tree, used as the base of a root commit.
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// A resolved base ref and the commit the working tree is compared with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseInfo {
    /// The name shown to the person (`main`, `origin/main`).
    pub name: String,
    /// The merge-base commit.
    pub merge_base: String,
}

/// The files a range changed.
#[derive(Clone, Debug)]
pub struct RangeFiles {
    /// Files, path-sorted and capped.
    pub files: Vec<FsDiffFile>,
    /// True when the cap was reached.
    pub truncated: bool,
}

/// One side of a file read from a git object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Blob {
    /// The path does not exist on that side.
    Missing,
    /// The blob is larger than the read cap and was not loaded.
    TooLarge,
    /// The bytes.
    Bytes(Vec<u8>),
}

fn run_git(dir: &Path, args: &[&str]) -> Result<Vec<u8>, FsError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|error| FsError::Io(format!("run git: {error}")))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(FsError::Invalid(if message.is_empty() {
            format!("git {} failed", args.join(" "))
        } else {
            message
        }))
    }
}

fn run_git_ok(dir: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

fn text_of(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_owned()
}

/// A branch name that is safe to pass to git as a value, never an option.
pub fn valid_base_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.starts_with('-')
        && !name.contains("..")
        && !name.contains("@{")
        && !name.ends_with('/')
        && !name.ends_with(".lock")
        && !name.bytes().any(|byte| {
            byte < 0x20
                || byte == b'\\'
                || byte == b' '
                || byte == b'~'
                || byte == b'^'
                || byte == b':'
                || byte == b'?'
                || byte == b'*'
                || byte == b'['
                || byte == 0x7f
        })
}

fn valid_sha(sha: &str) -> bool {
    (4..=64).contains(&sha.len()) && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Resolves a full or abbreviated commit id to its full object id.
///
/// Rejects anything that is not hex of at least four characters before git
/// sees it, so a request can never smuggle an option or an expression.
pub fn resolve_commit(repo: &Path, sha: &str) -> Result<String, FsError> {
    if !valid_sha(sha) {
        return Err(FsError::Invalid(format!(
            "`{sha}` is not a commit object id. Use a hex sha of at least 4 characters."
        )));
    }
    let spec = format!("{sha}^{{commit}}");
    let out = run_git_ok(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &spec,
        ],
    )
    .ok_or_else(|| FsError::Invalid(format!("`{sha}` is not a commit in this repository.")))?;
    let full = text_of(&out);
    if full.is_empty() {
        Err(FsError::Invalid(format!(
            "`{sha}` is not a commit in this repository."
        )))
    } else {
        Ok(full)
    }
}

fn resolves_commit(repo: &Path, spec: &str) -> Option<String> {
    let qualified = format!("{spec}^{{commit}}");
    run_git_ok(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &qualified,
        ],
    )
    .map(|out| text_of(&out))
    .filter(|sha| !sha.is_empty())
}

fn upstream_remote(workspace: &Path) -> Option<String> {
    let out = run_git_ok(
        workspace,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
    )?;
    let upstream = text_of(&out);
    upstream
        .split_once('/')
        .map(|(remote, _)| remote.to_owned())
}

fn first_remote(workspace: &Path) -> Option<String> {
    let out = run_git_ok(workspace, &["remote"])?;
    text_of(&out)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

/// The base name a request without an explicit base would use.
///
/// Order: the upstream's default branch (`refs/remotes/<remote>/HEAD`), then
/// its `main`, its `master`, a local `main`, a local `master`. A repository
/// with none of those has no base; the caller says so instead of guessing.
pub fn default_base_name(workspace: &Path) -> Option<String> {
    repository_root(workspace)?;
    if let Some(remote) = upstream_remote(workspace).or_else(|| first_remote(workspace)) {
        let head_ref = format!("refs/remotes/{remote}/HEAD");
        if let Some(out) = run_git_ok(
            workspace,
            &["symbolic-ref", "--quiet", "--short", &head_ref],
        ) {
            let name = text_of(&out);
            // A symbolic ref outlives the branch it points at. A `HEAD` left
            // dangling by a deleted or never-fetched branch is not a base; fall
            // through to the remote's main or master instead of failing later.
            if !name.is_empty() && name != "HEAD" && resolves_commit(workspace, &name).is_some() {
                return Some(name);
            }
        }
        for branch in ["main", "master"] {
            if resolves_commit(workspace, &format!("refs/remotes/{remote}/{branch}")).is_some() {
                return Some(format!("{remote}/{branch}"));
            }
        }
    }
    for branch in ["main", "master"] {
        if resolves_commit(workspace, &format!("refs/heads/{branch}")).is_some() {
            return Some(branch.to_owned());
        }
    }
    None
}

/// Resolves a requested base (or the default) to its merge base with `HEAD`.
pub fn resolve_base(workspace: &Path, requested: Option<&str>) -> Result<BaseInfo, FsError> {
    if repository_root(workspace).is_none() {
        return Err(FsError::NotFound(
            "This workspace is not a git repository.".into(),
        ));
    }
    let name = match requested.map(str::trim).filter(|name| !name.is_empty()) {
        Some(raw) => {
            if !valid_base_name(raw) {
                return Err(FsError::Invalid(format!(
                    "`{raw}` is not a branch name h4rn can compare against."
                )));
            }
            raw.to_owned()
        }
        None => default_base_name(workspace).ok_or_else(|| {
            FsError::Invalid(
                "No base branch: this repository has no remote default branch and no main or master branch."
                    .into(),
            )
        })?,
    };
    let spec = format!("{name}^{{commit}}");
    let base_sha = run_git_ok(
        workspace,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &spec,
        ],
    )
    .map(|out| text_of(&out))
    .filter(|sha| !sha.is_empty())
    .ok_or_else(|| FsError::Invalid(format!("`{name}` is not a commit in this repository.")))?;
    let merge_base = run_git_ok(workspace, &["merge-base", &base_sha, "HEAD"])
        .map(|out| text_of(&out))
        .filter(|sha| !sha.is_empty())
        .ok_or_else(|| {
            FsError::Invalid(format!(
                "`{name}` and HEAD have no common history to review."
            ))
        })?;
    Ok(BaseInfo { name, merge_base })
}

fn workspace_prefix(repo: &Path, workspace: &Path) -> String {
    workspace
        .strip_prefix(repo)
        .ok()
        .map(|path| {
            path.components()
                .map(|component| component.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/")
        })
        .unwrap_or_default()
}

fn in_scope(path: &str, prefix: &str) -> bool {
    prefix.is_empty() || path == prefix || path.starts_with(&format!("{prefix}/"))
}

fn workspace_relative(path: &str, prefix: &str) -> String {
    if prefix.is_empty() {
        path.to_owned()
    } else {
        path.strip_prefix(prefix)
            .map(|rest| rest.trim_start_matches('/').to_owned())
            .unwrap_or_else(|| path.to_owned())
    }
}

fn status_for(code: char) -> Option<GitFileStatus> {
    match code {
        'A' | 'C' => Some(GitFileStatus::Added),
        'D' => Some(GitFileStatus::Deleted),
        'M' => Some(GitFileStatus::Modified),
        'R' => Some(GitFileStatus::Renamed),
        'T' => Some(GitFileStatus::TypeChanged),
        'U' => Some(GitFileStatus::Conflicted),
        _ => None,
    }
}

/// One tracked change parsed from `--name-status` and merged with `--numstat`.
#[derive(Clone, Debug)]
struct RawChange {
    path: String,
    old_path: Option<String>,
    status: GitFileStatus,
    added: u32,
    removed: u32,
    binary: bool,
    counts_omitted: bool,
}

fn name_status(
    repo: &Path,
    range: &[&str],
) -> Result<Vec<(String, GitFileStatus, Option<String>)>, FsError> {
    let mut args = vec!["diff", "--name-status", "-z", "--find-renames"];
    args.extend_from_slice(range);
    args.push("--");
    let out = run_git(repo, &args)?;
    let parts: Vec<&[u8]> = out.split(|byte| *byte == 0).collect();
    let mut parsed = Vec::new();
    let mut index = 0;
    while index < parts.len() {
        let code = parts[index];
        if code.is_empty() {
            index += 1;
            continue;
        }
        let letter = code.first().map(|byte| *byte as char).unwrap_or(' ');
        if letter == 'R' || letter == 'C' {
            if index + 2 >= parts.len() {
                break;
            }
            let old = String::from_utf8_lossy(parts[index + 1]).into_owned();
            let new = String::from_utf8_lossy(parts[index + 2]).into_owned();
            if let Some(status) = status_for(letter) {
                parsed.push((new, status, Some(old)));
            }
            index += 3;
        } else {
            if index + 1 >= parts.len() {
                break;
            }
            let path = String::from_utf8_lossy(parts[index + 1]).into_owned();
            if let Some(status) = status_for(letter) {
                parsed.push((path, status, None));
            }
            index += 2;
        }
    }
    Ok(parsed)
}

fn num_stat(repo: &Path, range: &[&str]) -> Result<BTreeMap<String, (u32, u32, bool)>, FsError> {
    let mut args = vec!["diff", "--numstat", "-z", "--find-renames"];
    args.extend_from_slice(range);
    args.push("--");
    let out = run_git(repo, &args)?;
    let parts: Vec<&[u8]> = out.split(|byte| *byte == 0).collect();
    let mut parsed = BTreeMap::new();
    let mut index = 0;
    while index < parts.len() {
        let fields: Vec<&[u8]> = parts[index].split(|byte| *byte == b'\t').collect();
        if fields.len() < 3 {
            index += 1;
            continue;
        }
        let added = String::from_utf8_lossy(fields[0]).into_owned();
        let removed = String::from_utf8_lossy(fields[1]).into_owned();
        let binary = added == "-" || removed == "-";
        let count = |value: &str| value.parse::<u32>().unwrap_or(0);
        let path_field = String::from_utf8_lossy(fields[2]).into_owned();
        if path_field.is_empty() {
            // A rename or copy: the path field is empty and the old and new
            // names follow as their own NUL-terminated records.
            if index + 2 >= parts.len() {
                break;
            }
            let new = String::from_utf8_lossy(parts[index + 2]).into_owned();
            parsed.insert(new, (count(&added), count(&removed), binary));
            index += 3;
        } else {
            parsed.insert(path_field, (count(&added), count(&removed), binary));
            index += 1;
        }
    }
    Ok(parsed)
}

fn tracked_changes(repo: &Path, range: &[&str], prefix: &str) -> Result<Vec<RawChange>, FsError> {
    let statuses = name_status(repo, range)?;
    let stats = num_stat(repo, range)?;
    let mut changes = Vec::new();
    for (path, status, old_path) in statuses {
        if !in_scope(&path, prefix) {
            continue;
        }
        let (added, removed, binary) = stats.get(&path).copied().unwrap_or((0, 0, false));
        changes.push(RawChange {
            path: workspace_relative(&path, prefix),
            old_path: old_path.map(|old| workspace_relative(&old, prefix)),
            status,
            added,
            removed,
            binary,
            counts_omitted: false,
        });
    }
    Ok(changes)
}

/// Reads NUL-separated untracked paths, stopping as soon as one more than the
/// cap is known.
///
/// `git ls-files --others` on a tree full of build output can be millions of
/// entries long. Reading the whole listing into memory before noticing the cap
/// spends the daemon's time and RAM on a list nobody will see, so the scan reads
/// the stream in chunks and returns at the first entry past the cap; the caller
/// then kills git instead of letting it finish.
fn take_untracked_paths<R: Read>(
    mut reader: R,
    prefix: &str,
) -> std::io::Result<(Vec<String>, bool)> {
    let mut paths: Vec<String> = Vec::new();
    let mut pending: Vec<u8> = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok((paths, false));
        }
        for byte in &buffer[..read] {
            if *byte != 0 {
                pending.push(*byte);
                continue;
            }
            if pending.is_empty() {
                continue;
            }
            let path = String::from_utf8_lossy(&pending).into_owned();
            pending.clear();
            if !in_scope(&path, prefix) {
                continue;
            }
            if paths.len() == MAX_DIFF_FILES {
                // One entry past the cap is enough to know the list was cut.
                return Ok((paths, true));
            }
            paths.push(path);
        }
    }
}

/// The untracked files inside `prefix`, and whether the cap cut the list.
fn untracked_changes(repo: &Path, prefix: &str) -> Result<(Vec<RawChange>, bool), FsError> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["ls-files", "--others", "--exclude-standard", "-z"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| FsError::Io(format!("run git: {error}")))?;
    let scanned = child
        .stdout
        .take()
        .ok_or_else(|| FsError::Io("git ls-files produced no output to read".into()))
        .and_then(|stdout| {
            take_untracked_paths(stdout, prefix)
                .map_err(|error| FsError::Io(format!("read git ls-files: {error}")))
        });
    // Stop git as soon as the list is long enough; a huge tree is not worth
    // walking to the end for a list that is already cut at the cap.
    let _ = child.kill();
    let status = child.wait();
    let (paths, truncated) = scanned?;
    if !truncated {
        if let Ok(status) = status {
            if !status.success() {
                return Err(FsError::Invalid(
                    "git ls-files --others failed in this repository.".into(),
                ));
            }
        }
    }

    let mut changes = Vec::new();
    let mut budget = MAX_UNTRACKED_BYTES;
    for path in paths {
        let absolute = repo.join(&path);
        // Never follow a symlink here: an untracked link can point outside the
        // workspace, and only its existence, not its target's bytes, is ours
        // to report. It reads as binary with no line counts.
        let metadata = std::fs::symlink_metadata(&absolute).ok();
        let regular = metadata
            .as_ref()
            .map(|meta| meta.is_file())
            .unwrap_or(false);
        let size = metadata.as_ref().map(|meta| meta.len()).unwrap_or(0);
        let oversized = metadata
            .as_ref()
            .map(|meta| meta.len() > MAX_READ_BYTES)
            .unwrap_or(false);
        let mut binary = false;
        let mut counts_omitted = false;
        let mut added = 0;
        if !regular {
            binary = true;
        } else if oversized || size > budget {
            // Do not read a file the line count does not need, and do not spend
            // the whole pass on one large new file. The list says the count was
            // not read instead of pretending the file is empty.
            counts_omitted = true;
        } else {
            match std::fs::read(&absolute) {
                Ok(bytes) => {
                    budget = budget.saturating_sub(bytes.len() as u64);
                    match text::decode(&bytes) {
                        Some(decoded) => added = decoded.text.lines().count() as u32,
                        None => binary = true,
                    }
                }
                Err(_) => binary = true,
            }
        }
        changes.push(RawChange {
            path: workspace_relative(&path, prefix),
            old_path: None,
            status: GitFileStatus::Untracked,
            added,
            removed: 0,
            binary,
            counts_omitted,
        });
    }
    Ok((changes, truncated))
}

fn finish(mut changes: Vec<RawChange>, truncated_by_cap: bool) -> RangeFiles {
    changes.sort_by(|left, right| left.path.cmp(&right.path));
    let truncated = truncated_by_cap || changes.len() > MAX_DIFF_FILES;
    changes.truncate(MAX_DIFF_FILES);
    RangeFiles {
        files: changes
            .into_iter()
            .map(|change| FsDiffFile {
                path: change.path,
                old_path: change.old_path,
                status: change.status,
                added: change.added,
                removed: change.removed,
                binary: change.binary,
                counts_omitted: change.counts_omitted,
            })
            .collect(),
        truncated,
    }
}

/// The files the working tree changed against the base's merge base.
pub fn branch_files(
    workspace: &Path,
    requested: Option<&str>,
) -> Result<(BaseInfo, RangeFiles), FsError> {
    let repo = repository_root(workspace)
        .ok_or_else(|| FsError::NotFound("This workspace is not a git repository.".into()))?;
    let base = resolve_base(workspace, requested)?;
    let prefix = workspace_prefix(&repo, workspace);
    let range = [&base.merge_base[..]];
    let mut changes = tracked_changes(&repo, &range, &prefix)?;
    let capped;
    if changes.len() < MAX_DIFF_FILES {
        let (untracked, untracked_truncated) = untracked_changes(&repo, &prefix)?;
        capped = untracked_truncated || changes.len() + untracked.len() > MAX_DIFF_FILES;
        changes.extend(untracked);
    } else {
        capped = true;
    }
    Ok((base, finish(changes, capped)))
}

/// The files one commit changed against its first parent.
pub fn commit_files(workspace: &Path, sha: &str) -> Result<(String, RangeFiles), FsError> {
    let repo = repository_root(workspace)
        .ok_or_else(|| FsError::NotFound("This workspace is not a git repository.".into()))?;
    let full = resolve_commit(&repo, sha)?;
    let short = run_git_ok(&repo, &["rev-parse", "--short", &full])
        .map(|out| text_of(&out))
        .unwrap_or_else(|| full[..7.min(full.len())].to_owned());
    let parent = first_parent(&repo, &full)?;
    let prefix = workspace_prefix(&repo, workspace);
    let range = [&parent[..], &full[..]];
    let changes = tracked_changes(&repo, &range, &prefix)?;
    Ok((short, finish(changes, false)))
}

/// The newest commits in `merge-base..HEAD`, newest first.
pub fn commits(workspace: &Path, requested: Option<&str>) -> Result<FsDiffCommitList, FsError> {
    let base = resolve_base(workspace, requested)?;
    let range = format!("{}..HEAD", base.merge_base);
    let out = run_git(
        workspace,
        &[
            "log",
            "--max-count=101",
            "--format=%H%x1f%h%x1f%aI%x1f%an%x1f%s",
            &range,
            "--",
        ],
    )?;
    let mut commits = Vec::new();
    let mut truncated = false;
    for line in text_of(&out).lines() {
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\x1f').collect();
        if fields.len() < 5 {
            continue;
        }
        let Ok(authored_at) = DateTime::parse_from_rfc3339(fields[2]) else {
            continue;
        };
        if commits.len() == MAX_DIFF_COMMITS {
            truncated = true;
            break;
        }
        commits.push(FsDiffCommit {
            sha: fields[0].to_owned(),
            short_sha: fields[1].to_owned(),
            authored_at: authored_at.with_timezone(&Utc),
            author: fields[3].to_owned(),
            subject: fields[4].to_owned(),
        });
    }
    Ok(FsDiffCommitList {
        base: base.name,
        merge_base: base.merge_base,
        commits,
        truncated,
    })
}

/// Local and remote-tracking branch names, plus the default base when one resolves.
pub fn branches(workspace: &Path) -> Result<h4rn_protocol::FsBranchList, FsError> {
    let repo = repository_root(workspace)
        .ok_or_else(|| FsError::NotFound("This workspace is not a git repository.".into()))?;
    let out = run_git(
        &repo,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads",
            "refs/remotes",
        ],
    )?;
    let mut local = BTreeSet::new();
    let mut remote = BTreeSet::new();
    for line in text_of(&out).lines() {
        if let Some(name) = line.strip_prefix("refs/heads/") {
            if !name.is_empty() {
                local.insert(name.to_owned());
            }
        } else if let Some(name) = line.strip_prefix("refs/remotes/") {
            if !name.is_empty() && !name.ends_with("/HEAD") {
                remote.insert(name.to_owned());
            }
        }
    }
    let mut branches: Vec<String> = local.into_iter().collect();
    branches.extend(remote);
    Ok(h4rn_protocol::FsBranchList {
        branches,
        default_base: default_base_name(workspace),
    })
}

/// Reads one path from a git object, bounded by the read cap.
pub fn blob_at(repo: &Path, rev: &str, path: &str) -> Result<Blob, FsError> {
    let spec = format!("{rev}:{path}");
    let Some(size_out) = run_git_ok(repo, &["cat-file", "-s", &spec]) else {
        return Ok(Blob::Missing);
    };
    let size = text_of(&size_out).parse::<u64>().unwrap_or(u64::MAX);
    if size > MAX_READ_BYTES {
        return Ok(Blob::TooLarge);
    }
    match run_git_ok(repo, &["cat-file", "blob", &spec]) {
        Some(bytes) => Ok(Blob::Bytes(bytes)),
        None => Ok(Blob::Missing),
    }
}

/// Maps a workspace-relative path to the repository-relative path git expects.
pub fn repo_path(workspace: &Path, repo: &Path, rel: &str) -> String {
    let prefix = workspace_prefix(repo, workspace);
    if prefix.is_empty() {
        rel.to_owned()
    } else {
        format!("{prefix}/{rel}")
    }
}

/// The old path of a rename in `range`, as `git diff --name-status` sees it.
///
/// The branch review passes one revision (working tree against the merge base);
/// a commit review passes parent and commit.
pub fn renamed_from_range(repo: &Path, range: &[&str], new_path: &str) -> Option<String> {
    let mut args = vec!["diff", "--name-status", "-z", "--find-renames"];
    args.extend_from_slice(range);
    args.push("--");
    let out = run_git_ok(repo, &args)?;
    let parts: Vec<&[u8]> = out.split(|byte| *byte == 0).collect();
    let mut index = 0;
    while index + 2 < parts.len() {
        let code = parts[index]
            .first()
            .map(|byte| *byte as char)
            .unwrap_or(' ');
        if code == 'R' {
            let old = String::from_utf8_lossy(parts[index + 1]).into_owned();
            let new = String::from_utf8_lossy(parts[index + 2]).into_owned();
            if new == new_path {
                return Some(old);
            }
        }
        index += if code == 'R' || code == 'C' { 3 } else { 2 };
    }
    None
}

/// True when the stored commit object names a parent.
///
/// `git rev-parse <sha>^` respects shallow grafts and finds no parent for a
/// shallow boundary, so the raw object is the only way to tell a boundary
/// commit from a root commit.
fn stored_parent_count(repo: &Path, sha: &str) -> Option<usize> {
    let spec = format!("{sha}^{{commit}}");
    let out = run_git_ok(repo, &["cat-file", "-p", &spec])?;
    Some(
        text_of(&out)
            .lines()
            .take_while(|line| !line.is_empty())
            .filter(|line| line.starts_with("parent "))
            .count(),
    )
}

/// The first parent of a commit, or the empty tree for a root commit.
///
/// A shallow clone carries a boundary commit whose parent object is absent. Its
/// stored object still names the parent, so reporting it as a root commit would
/// invent an "everything was added" review. Say what is missing instead.
pub fn first_parent(repo: &Path, sha: &str) -> Result<String, FsError> {
    if let Some(parent) = run_git_ok(
        repo,
        &["rev-parse", "--verify", "--quiet", &format!("{sha}^")],
    )
    .map(|out| text_of(&out))
    .filter(|parent| !parent.is_empty())
    {
        return Ok(parent);
    }
    match stored_parent_count(repo, sha) {
        Some(count) if count > 0 => Err(FsError::Invalid(format!(
            "This commit's parent isn't in this shallow clone, so there is nothing to compare `{sha}` with. Fetch more history and try again."
        ))),
        _ => Ok(EMPTY_TREE.to_owned()),
    }
}

/// Git's abbreviated form of a full commit id.
pub fn short_sha(repo: &Path, full: &str) -> String {
    run_git_ok(repo, &["rev-parse", "--short", full])
        .map(|out| text_of(&out))
        .unwrap_or_else(|| full[..7.min(full.len())].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Counts the bytes the scan actually pulled out of the stream.
    struct CountingReader {
        inner: std::io::Cursor<Vec<u8>>,
        read: std::rc::Rc<std::cell::Cell<usize>>,
    }

    impl Read for CountingReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let read = self.inner.read(buffer)?;
            self.read.set(self.read.get() + read);
            Ok(read)
        }
    }

    #[test]
    fn the_untracked_scan_stops_reading_one_entry_past_the_cap() {
        let total = MAX_DIFF_FILES + 20_000;
        let mut data = Vec::new();
        for index in 0..total {
            data.extend_from_slice(format!("loose/file-{index:05}.txt\0").as_bytes());
        }
        let read = std::rc::Rc::new(std::cell::Cell::new(0));
        let reader = CountingReader {
            inner: std::io::Cursor::new(data.clone()),
            read: std::rc::Rc::clone(&read),
        };

        let (paths, truncated) = take_untracked_paths(reader, "").expect("scan");

        // Reading the whole listing before noticing the cap is the bug this
        // guards: the scan must stop at the first entry past it.
        assert!(
            read.get() < data.len(),
            "the scan read {} of {} bytes instead of stopping at the cap",
            read.get(),
            data.len()
        );
        assert_eq!(paths.len(), MAX_DIFF_FILES);
        assert!(truncated);
    }

    #[test]
    fn the_untracked_scan_counts_only_paths_inside_the_workspace() {
        let mut data = Vec::new();
        for path in ["sub/a.txt", "other/b.txt", "sub/c.txt"] {
            data.extend_from_slice(format!("{path}\0").as_bytes());
        }
        let reader = std::io::Cursor::new(data);
        let (paths, truncated) = take_untracked_paths(reader, "sub").expect("scan");
        assert_eq!(paths, vec!["sub/a.txt".to_owned(), "sub/c.txt".to_owned()]);
        assert!(!truncated);
    }

    #[test]
    fn branch_names_and_shas_are_validated_before_git_sees_them() {
        assert!(valid_base_name("main"));
        assert!(valid_base_name("origin/feature-x"));
        assert!(!valid_base_name("--output=/tmp/x"));
        assert!(!valid_base_name("-D"));
        assert!(!valid_base_name("a..b"));
        assert!(!valid_base_name("main "));
        assert!(valid_sha("1a2b3c4"));
        assert!(valid_sha("0123456789abcdef0123456789abcdef01234567"));
        assert!(!valid_sha("--help"));
        assert!(!valid_sha("zzzzzz"));
        assert!(!valid_sha("abc"));
    }

    #[test]
    fn status_letters_map_to_the_file_badge() {
        assert_eq!(status_for('A'), Some(GitFileStatus::Added));
        assert_eq!(status_for('M'), Some(GitFileStatus::Modified));
        assert_eq!(status_for('R'), Some(GitFileStatus::Renamed));
        assert_eq!(status_for('?'), None);
    }
}
