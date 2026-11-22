//! Filesystem and git types for IDE mode.
//!
//! These types carry the daemon's path-safe filesystem service: directory
//! listings, file reads and writes, workspace search, git diffs and the two
//! snapshot scopes IDE mode needs (`HEAD` and one agent turn). They live in
//! their own module so the wire contract stays reviewable (ADR-002) while the
//! editor itself stays a thin view over the daemon (ADR-001, ADR-013).
//!
//! Paths on the wire are always **relative to the workspace root** and use
//! `/` separators. The daemon canonicalises every path and rejects anything
//! that escapes its registered workspace roots, including symlink escapes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::ids::{ThreadId, TurnId};

/// The kind of a directory entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum FileEntryKind {
    /// A regular file.
    File,
    /// A directory, listed lazily on expand.
    Directory,
    /// A symbolic link; the daemon reports it without following it in listings.
    Symlink,
}

/// One git status letter for a tracked or untracked path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum GitFileStatus {
    /// A new path in the index.
    Added,
    /// Tracked content differs from `HEAD`.
    Modified,
    /// Tracked path removed from the working tree.
    Deleted,
    /// A tracked path moved.
    Renamed,
    /// A path git does not track yet.
    Untracked,
    /// An unresolved merge conflict.
    Conflicted,
    /// Mode change (for example a new executable bit).
    TypeChanged,
}

impl GitFileStatus {
    /// A stable one-letter badge for the file tree.
    pub fn badge(self) -> &'static str {
        match self {
            Self::Added => "A",
            Self::Modified => "M",
            Self::Deleted => "D",
            Self::Renamed => "R",
            Self::Untracked => "U",
            Self::Conflicted => "!",
            Self::TypeChanged => "T",
        }
    }

    /// A short label for screen readers and tooltips.
    pub fn label(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
            Self::Renamed => "renamed",
            Self::Untracked => "untracked",
            Self::Conflicted => "conflicted",
            Self::TypeChanged => "type changed",
        }
    }
}

/// One entry in a directory listing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct FileEntry {
    /// Display name (the final path component).
    pub name: String,
    /// Workspace-relative path with `/` separators.
    pub path: String,
    /// File, directory or symlink.
    pub kind: FileEntryKind,
    /// Size in bytes; `0` for directories.
    #[ts(type = "number")]
    pub size_bytes: u64,
    /// Modification time, when the host reports one.
    pub modified_at: Option<DateTime<Utc>>,
    /// Git state, when the workspace is a repository and the path is dirty.
    pub git_status: Option<GitFileStatus>,
}

/// One lazily listed directory.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct FsListing {
    /// Canonical workspace root the paths are relative to.
    pub workspace: String,
    /// Workspace-relative directory that was listed (empty for the root).
    pub path: String,
    /// Entries after `.gitignore` filtering, directories first.
    pub entries: Vec<FileEntry>,
    /// Whether the daemon stopped at its entry cap.
    pub truncated: bool,
}

/// A file read with encoding and content detection.
///
/// `content` is `None` when the daemon refuses to hand back the bytes: the
/// file is binary, larger than the edit limit, or not valid text in a supported
/// encoding. `hint` says which, in the user's language.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct FsFileContent {
    /// Workspace-relative path.
    pub path: String,
    /// Decoded text, or `None` when the file is not editable text.
    pub content: Option<String>,
    /// BLAKE3 content address of the raw bytes; the optimistic-concurrency token.
    pub etag: Option<String>,
    /// Detected encoding name (`utf-8`, `utf-8-bom`, `utf-16le`, `utf-16be`) or
    /// `binary`.
    pub encoding: String,
    /// Raw size in bytes.
    #[ts(type = "number")]
    pub size_bytes: u64,
    /// True when the daemon refused to decode the file as text.
    pub binary: bool,
    /// True when the file is over the edit size limit.
    pub truncated: bool,
    /// Raw bytes as standard base64, when the read asked for them.
    ///
    /// This is how the HTML preview inlines binary assets such as images and
    /// fonts. A `bytes_base64` read returns these bytes **instead of** decoded
    /// text, so one large asset cannot exceed a transport frame by carrying
    /// both forms. Ordinary reads send neither.
    #[serde(default)]
    pub bytes_base64: Option<String>,
    /// Why the requested raw bytes were not returned.
    ///
    /// A `bytes_base64` read is capped so one response fits the secure
    /// transport's frame; a larger file comes back with its metadata and this
    /// reason, and the preview shows the asset as skipped instead of failing
    /// the whole page.
    #[serde(default)]
    pub bytes_omitted: Option<FsOmitted>,
    /// Why the requested decoded text was not returned.
    ///
    /// A read that asked for `max_bytes` is capped at that many bytes so one
    /// response fits the secure transport's frame. A larger file comes back
    /// with its metadata and this reason instead of its content.
    #[serde(default)]
    pub content_omitted: Option<FsOmitted>,
    /// Last modification time, when available.
    pub modified_at: Option<DateTime<Utc>>,
    /// What to do about a refusal, when there is one.
    pub hint: Option<String>,
}

/// Why a requested payload (text or raw bytes) was not returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum FsOmitted {
    /// The file is larger than the limit that applies to the request.
    TooLarge,
}

/// A save that lost the race against another writer.
///
/// Returned instead of an error so the client can show the conflict dialog with
/// the current bytes already in hand ("keep mine" / "take theirs" / "view diff").
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct FsWriteConflict {
    /// The etag the client based its edit on.
    pub expected_etag: Option<String>,
    /// The etag the daemon found on disk; `None` when the file no longer exists.
    pub current_etag: Option<String>,
    /// The current bytes as text, when they decode.
    pub current_content: Option<String>,
    /// Encoding of the current bytes.
    pub current_encoding: String,
}

/// The result of a write, including a conflict instead of a silent overwrite.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct FsWriteResult {
    /// Workspace-relative path.
    pub path: String,
    /// New etag of the bytes on disk (the old etag on a conflict).
    pub etag: String,
    /// Size after the write (or of the current bytes on a conflict).
    #[ts(type = "number")]
    pub size_bytes: u64,
    /// Last modification time after the write.
    pub modified_at: Option<DateTime<Utc>>,
    /// Present when the write was refused because the file changed.
    pub conflict: Option<FsWriteConflict>,
}

/// What to create.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum FsCreateKind {
    /// An empty regular file.
    File,
    /// A directory (parents are created).
    Directory,
}

/// The result of deleting a path.
///
/// Delete is reversible: the daemon moves the entry into its trash directory
/// and returns the id needed to restore it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct FsDeleteResult {
    /// Workspace-relative path that was removed.
    pub path: String,
    /// Trash id accepted by `FsRestore`.
    pub trash_id: String,
    /// Host-local trash location, for the audit log and diagnostics.
    pub trashed_to: String,
}

/// Which side of a diff to compare against.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiffAgainst {
    /// The repository's `HEAD` blob.
    Head,
    /// The pre-turn snapshot the daemon recorded for one agent turn.
    AgentTurn {
        /// Thread that owns the turn.
        thread_id: ThreadId,
        /// Turn whose pre-turn snapshot is the base.
        turn_id: TurnId,
    },
    /// The working tree against the merge base of a branch and `HEAD`.
    ///
    /// The merge base is what keeps commits made on the base branch after the
    /// fork out of the review. `base` is a local branch or remote-tracking
    /// name such as `main` or `origin/main`; `None` picks the repository's
    /// default: the upstream's default branch, then `main`, then `master`.
    Base {
        /// Branch to compare against, or `None` for the repository default.
        base: Option<String>,
    },
    /// One commit against its first parent (a root commit against an empty tree).
    Commit {
        /// Full or abbreviated commit object id, validated by the daemon.
        sha: String,
    },
}

/// One file in a branch or commit range.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct FsDiffFile {
    /// Workspace-relative path (the new path for a rename).
    pub path: String,
    /// The previous path for a rename.
    pub old_path: Option<String>,
    /// What happened to the file in the range.
    pub status: GitFileStatus,
    /// Lines added, when the side is text.
    pub added: u32,
    /// Lines removed, when the side is text.
    pub removed: u32,
    /// True when git or the daemon found either side is not text.
    pub binary: bool,
}

/// The files a branch or commit range changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct FsDiffFiles {
    /// Scope the list was compared against.
    pub against: DiffAgainst,
    /// Human label naming the scope ("vs main", "Commit 1a2b3c4").
    pub label: String,
    /// The resolved base ref, when the scope is a branch.
    pub base: Option<String>,
    /// The merge-base commit the working tree was compared with.
    pub merge_base: Option<String>,
    /// Changed files, path-sorted.
    pub files: Vec<FsDiffFile>,
    /// True when the daemon stopped at its file cap.
    pub truncated: bool,
    /// Why the list may be incomplete, when that is worth saying.
    pub note: Option<String>,
}

/// One commit in the branch comparison.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct FsDiffCommit {
    /// Full object id.
    pub sha: String,
    /// Git's short form, for labels.
    pub short_sha: String,
    /// The commit subject line.
    pub subject: String,
    /// Author name.
    pub author: String,
    /// Author time.
    pub authored_at: DateTime<Utc>,
}

/// The newest commits in `merge-base..HEAD`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct FsDiffCommitList {
    /// The resolved base ref.
    pub base: String,
    /// The merge-base commit the range starts at.
    pub merge_base: String,
    /// Newest first, capped at 100.
    pub commits: Vec<FsDiffCommit>,
    /// True when older commits exist beyond the cap.
    pub truncated: bool,
}

/// Branch names a person can compare against.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct FsBranchList {
    /// Local branches first, then remote-tracking names.
    pub branches: Vec<String>,
    /// The base a `Base { base: None }` request resolves to, when one exists.
    pub default_base: Option<String>,
}

/// Both sides of one file diff.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct FsDiff {
    /// Workspace-relative path.
    pub path: String,
    /// What the diff was compared against.
    pub against: DiffAgainst,
    /// Human label naming the scope ("HEAD" or "Turn 4 · Claude Code").
    pub label: String,
    /// Base text, or `None` when the file is new or the base is binary.
    pub before: Option<String>,
    /// Current text, or `None` when the file was deleted or is binary.
    pub after: Option<String>,
    /// True when either side could not be decoded.
    pub binary: bool,
    /// True when a side is over the read limit and was not loaded.
    #[serde(default)]
    pub too_large: bool,
    /// Why one side is missing, when that is not self-evident.
    pub note: Option<String>,
}

/// One line match from workspace search.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct FsSearchMatch {
    /// Workspace-relative path.
    pub path: String,
    /// One-based line number.
    pub line_number: u32,
    /// One-based UTF-8 character column of the first match.
    pub column: u32,
    /// The matched line, trimmed to a bounded preview.
    pub preview: String,
}

/// A streamed batch of search results.
///
/// Search is streamed as a sequence of these batches; the last one carries
/// `done: true`. Batches are transient events and are never journaled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct FsSearchBatch {
    /// Client-visible search id returned by `FsSearchStarted`.
    pub search_id: String,
    /// Matches found since the previous batch.
    pub results: Vec<FsSearchMatch>,
    /// True on the final batch.
    pub done: bool,
    /// True when the daemon hit its result cap.
    pub truncated: bool,
}

/// A filesystem change reported by the watcher.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum FsChangeKind {
    /// A path appeared.
    Created,
    /// Content or metadata changed.
    Modified,
    /// A path disappeared.
    Deleted,
    /// A path moved; the reported path is the new one.
    Renamed,
}

/// One file in a turn's change set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct FsTurnFile {
    /// Workspace-relative path.
    pub path: String,
    /// Lines added, counted from the pre-turn snapshot to the current file.
    pub added: u32,
    /// Lines removed.
    pub removed: u32,
    /// Etag recorded before the turn, when the file existed.
    pub before_etag: Option<String>,
    /// Current etag, when the file still exists.
    pub after_etag: Option<String>,
}

/// The files one agent turn touched, with line counts for the review list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct FsTurnChanges {
    /// Thread that owns the turn.
    pub thread_id: ThreadId,
    /// Turn whose snapshots were compared with the working tree.
    pub turn_id: TurnId,
    /// Changed files, path-sorted.
    pub files: Vec<FsTurnFile>,
    /// Why the list may be incomplete, when snapshots were missing.
    pub note: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_against_is_tagged_and_stable() {
        let head = serde_json::to_value(DiffAgainst::Head).unwrap();
        assert_eq!(head["kind"], "head");

        let turn = serde_json::to_value(DiffAgainst::AgentTurn {
            thread_id: ThreadId::new("th_1"),
            turn_id: TurnId::new("turn_1"),
        })
        .unwrap();
        assert_eq!(turn["kind"], "agent_turn");
        assert_eq!(turn["thread_id"], "th_1");
        assert_eq!(turn["turn_id"], "turn_1");

        let branch = serde_json::to_value(DiffAgainst::Base { base: None }).unwrap();
        assert_eq!(branch["kind"], "base");
        assert!(branch["base"].is_null());

        let commit = serde_json::to_value(DiffAgainst::Commit {
            sha: "1a2b3c4".into(),
        })
        .unwrap();
        assert_eq!(commit["kind"], "commit");
        assert_eq!(commit["sha"], "1a2b3c4");
    }

    #[test]
    fn older_diff_scopes_still_deserialize() {
        // A client from before branch and commit review only knows these two
        // shapes; the daemon must keep accepting them byte for byte.
        let head: DiffAgainst = serde_json::from_str(r#"{"kind":"head"}"#).unwrap();
        assert_eq!(head, DiffAgainst::Head);
        let turn: DiffAgainst =
            serde_json::from_str(r#"{"kind":"agent_turn","thread_id":"th_1","turn_id":"turn_1"}"#)
                .unwrap();
        assert_eq!(
            turn,
            DiffAgainst::AgentTurn {
                thread_id: ThreadId::new("th_1"),
                turn_id: TurnId::new("turn_1"),
            }
        );
    }

    #[test]
    fn a_diff_without_the_newer_too_large_flag_still_deserializes() {
        // An older daemon sends no `too_large`; the field defaults to false so
        // the client never has to special-case the missing key.
        let diff: FsDiff = serde_json::from_str(
            r#"{
                "path": "a.txt",
                "against": {"kind": "head"},
                "label": "HEAD",
                "before": "old",
                "after": "new",
                "binary": false,
                "note": null
            }"#,
        )
        .unwrap();
        assert!(!diff.too_large);
    }

    #[test]
    fn git_badges_are_one_letter_and_paired_with_a_label() {
        assert_eq!(GitFileStatus::Modified.badge(), "M");
        assert_eq!(GitFileStatus::Modified.label(), "modified");
        assert_eq!(GitFileStatus::Conflicted.badge(), "!");
    }

    #[test]
    fn file_content_round_trips() {
        let file = FsFileContent {
            path: "src/main.rs".into(),
            content: Some("fn main() {}\n".into()),
            etag: Some("abc".into()),
            encoding: "utf-8".into(),
            size_bytes: 12,
            binary: false,
            truncated: false,
            bytes_base64: None,
            bytes_omitted: None,
            content_omitted: None,
            modified_at: None,
            hint: None,
        };
        let json = serde_json::to_string(&file).unwrap();
        let back: FsFileContent = serde_json::from_str(&json).unwrap();
        assert_eq!(file, back);
    }
}
