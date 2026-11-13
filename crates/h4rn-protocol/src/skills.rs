//! Skills: bundles, registry views, import, diff and cross-machine sync.
//!
//! A skill is a directory with a `SKILL.md` entry point plus optional
//! references, scripts and assets (ADR-034). The daemon owns one registry per
//! user, installs each skill into the harness directories the user chooses and
//! keeps a content-addressed copy so the same version can be synced between
//! machines. The wire types here are shared by the daemon WebSocket, the
//! `h4rn-connect` sync endpoints and the generated TypeScript bindings.
//!
//! File contents cross the wire as standard base64 without padding. Skill
//! bundles are small (documents and scripts, not media); the daemon enforces a
//! per-bundle cap before it builds a frame.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::common::HarnessKind;

/// One file in a skill manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SkillFile {
    /// Bundle-relative path with forward slashes.
    pub path: String,
    /// Size in bytes.
    #[ts(type = "number")]
    pub size: u64,
    /// Lowercase hex BLAKE3 digest of the file bytes.
    pub blake3: String,
    /// Whether the executable bit was set in the source.
    pub executable: bool,
}

/// The canonical, machine-independent description of one skill version.
///
/// The version id is the BLAKE3 digest of a canonical JSON encoding of this
/// struct, so two machines that import the same directory agree on the version
/// without exchanging timestamps or absolute paths.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SkillManifest {
    /// Frontmatter `name`, or the directory name when absent.
    pub name: String,
    /// Frontmatter `description`.
    pub description: String,
    /// Total size of every file in the bundle.
    #[ts(type = "number")]
    pub total_bytes: u64,
    /// Every file, sorted by `path`.
    pub files: Vec<SkillFile>,
}

/// One file's bytes in a bundle payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SkillBundleFile {
    /// Bundle-relative path with forward slashes.
    pub path: String,
    /// Standard base64 (no padding) file contents.
    pub content: String,
}

/// Where a skill version came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SkillSource {
    /// A directory on the daemon host.
    LocalFolder {
        /// Absolute path that was read.
        path: String,
    },
    /// A git repository pinned to one commit.
    Git {
        /// Clone or fetch URL.
        url: String,
        /// Full commit id that was checked out.
        commit: String,
        /// Subdirectory inside the repository, when the skill is not the root.
        subdir: Option<String>,
    },
    /// A skill discovered in a harness's own directory.
    HarnessDir {
        /// Harness that owned the directory.
        harness: HarnessKind,
        /// Absolute path that was imported.
        path: String,
    },
    /// A version pulled from h4rn-connect.
    Connect {
        /// Skill id on the synced registry.
        skill_id: String,
        /// Version that was applied.
        version: String,
    },
}

/// The install state of one skill for one harness on this machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SkillInstallState {
    /// Harness the state describes.
    pub harness: HarnessKind,
    /// Whether the harness currently sees the skill.
    pub installed: bool,
    /// Whether the installed copy is owned by h4rn.
    ///
    /// A user-owned skill with the same name is never overwritten or removed.
    pub managed: bool,
    /// Enabled flag recorded in the registry.
    pub enabled: bool,
    /// Installed version, when the copy is managed or readable.
    pub version: Option<String>,
    /// Absolute directory the harness reads, when installed.
    pub path: Option<String>,
}

/// One harness and the directories h4rn would use for it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SkillHarnessAvailability {
    /// Harness.
    pub harness: HarnessKind,
    /// Whether the harness executable was detected on the host.
    pub detected: bool,
    /// User-scope skill directory, when the harness has one.
    pub user_root: Option<String>,
    /// Project-scope skill directory, when the harness has one.
    pub project_root: Option<String>,
}

/// One skill row in the inventory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SkillSummary {
    /// Stable registry id (derived from the first version, never reused).
    pub id: String,
    /// Frontmatter name.
    pub name: String,
    /// Frontmatter description.
    pub description: String,
    /// Current version hash.
    pub version: String,
    /// Where the current version came from.
    pub source: SkillSource,
    /// Total bundle size.
    #[ts(type = "number")]
    pub total_bytes: u64,
    /// When the registry row last changed.
    pub updated_at: DateTime<Utc>,
    /// Per-harness install and enablement state.
    pub installs: Vec<SkillInstallState>,
    /// Number of unresolved conflicts for this skill.
    #[ts(type = "number")]
    pub conflicts: u32,
}

/// Everything the Skills screen needs in one response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SkillInventory {
    /// Registry rows, sorted by name.
    pub skills: Vec<SkillSummary>,
    /// Harness directories and detection state.
    pub harnesses: Vec<SkillHarnessAvailability>,
}

/// A skill plus its full manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SkillDetail {
    /// Registry row.
    pub summary: SkillSummary,
    /// Canonical manifest of the current version.
    pub manifest: SkillManifest,
}

/// How one file changed between two versions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum SkillFileChange {
    /// Present only in the newer version.
    Added,
    /// Present only in the older version.
    Removed,
    /// Present in both with different bytes.
    Modified,
}

/// One changed file in a version diff.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SkillDiffFile {
    /// Bundle-relative path.
    pub path: String,
    /// Change kind.
    pub change: SkillFileChange,
    /// Digest in the older version, when present.
    pub old_blake3: Option<String>,
    /// Digest in the newer version, when present.
    pub new_blake3: Option<String>,
    /// Size in the older version, when present.
    #[ts(type = "number | null")]
    pub old_size: Option<u64>,
    /// Size in the newer version, when present.
    #[ts(type = "number | null")]
    pub new_size: Option<u64>,
}

/// A diff between two versions of one skill.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SkillDiff {
    /// Registry id.
    pub skill_id: String,
    /// Older version, or `None` when the skill is new locally.
    pub from_version: Option<String>,
    /// Newer version.
    pub to_version: String,
    /// Changed files, sorted by path.
    pub files: Vec<SkillDiffFile>,
}

/// An unresolved content conflict.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SkillConflict {
    /// Conflict id.
    pub id: String,
    /// Registry id of the existing skill.
    pub skill_id: String,
    /// Skill name both sides claim.
    pub name: String,
    /// Version already in the registry, when any.
    pub local_version: Option<String>,
    /// Version that could not be applied.
    pub remote_version: String,
    /// Human-readable reason.
    pub reason: String,
    /// When it was detected.
    pub detected_at: DateTime<Utc>,
}

/// How the user wants a conflict settled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum SkillConflictResolution {
    /// Discard the incoming version and keep the registry copy.
    KeepLocal,
    /// Replace the registry copy with the incoming version.
    KeepRemote,
    /// Rename the incoming version and keep both.
    KeepBoth,
}

/// One skill discovered in a harness directory during an import scan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SkillImportCandidate {
    /// Stable candidate id (content hash plus source path).
    pub id: String,
    /// Skill name.
    pub name: String,
    /// Skill description.
    pub description: String,
    /// Content version that would be imported.
    pub version: String,
    /// Absolute source directory.
    pub source_path: String,
    /// Harness that owns the source directory.
    pub harness: HarnessKind,
    /// `Some(reason)` when importing would conflict with the registry.
    pub conflict: Option<String>,
    /// Whether an identical version is already in the registry.
    pub duplicate: bool,
}

/// Preview of one import scan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SkillImportPreview {
    /// Importable skills.
    pub candidates: Vec<SkillImportCandidate>,
    /// Number of candidates that would conflict.
    #[ts(type = "number")]
    pub conflicts: u32,
    /// Number that are already present at the same version.
    #[ts(type = "number")]
    pub duplicates: u32,
}

/// Overall sync state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum SkillSyncState {
    /// No sync has run in this daemon process.
    Idle,
    /// A sync is running.
    Syncing,
    /// The last sync finished cleanly.
    Synced,
    /// The last sync was refused by the secret scanner.
    Blocked,
    /// Connect was not reachable or the daemon is signed out.
    Offline,
}

/// Result of the latest sync, also emitted as a transient event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SkillSyncStatus {
    /// Overall state.
    pub state: SkillSyncState,
    /// When the last successful sync finished.
    pub last_sync: Option<DateTime<Utc>>,
    /// Entries accepted by the server.
    #[ts(type = "number")]
    pub pushed: u32,
    /// Entries applied from the server.
    #[ts(type = "number")]
    pub pulled: u32,
    /// Unresolved conflicts known after the sync.
    #[ts(type = "number")]
    pub conflicts: u32,
    /// Cursor for the next incremental pull.
    pub etag: Option<String>,
    /// What happened, for the UI.
    pub message: Option<String>,
    /// What to do about it.
    pub hint: Option<String>,
}

/// A Lamport-style version vector keyed by machine id.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SkillVersionVector {
    /// Per-machine counters.
    pub clocks: BTreeMap<String, i64>,
}

impl SkillVersionVector {
    /// Whether `self` strictly dominates `other`.
    pub fn dominates(&self, other: &Self) -> bool {
        let mut strictly_greater = false;
        for (machine, clock) in &other.clocks {
            match self.clocks.get(machine) {
                Some(ours) if ours > clock => strictly_greater = true,
                Some(ours) if ours == clock => {}
                _ => return false,
            }
        }
        for machine in self.clocks.keys() {
            if !other.clocks.contains_key(machine) {
                strictly_greater = true;
            }
        }
        strictly_greater
    }

    /// Whether neither side dominates the other.
    pub fn concurrent(&self, other: &Self) -> bool {
        !self.dominates(other) && !other.dominates(self)
    }
}

/// One registry entry exchanged with connect.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SkillSyncEntry {
    /// Registry id.
    pub skill_id: String,
    /// Skill name.
    pub name: String,
    /// Skill description.
    pub description: String,
    /// Content version.
    pub version: String,
    /// Canonical manifest.
    pub manifest: SkillManifest,
    /// File bytes.
    pub files: Vec<SkillBundleFile>,
    /// Version vector at the sender.
    pub vector: SkillVersionVector,
    /// Sender's wall clock; last-writer-wins tie breaker.
    pub updated_at: DateTime<Utc>,
    /// Tombstone for a removed skill.
    pub deleted: bool,
    /// ETag the sender believes is current, when updating.
    pub expected_etag: Option<String>,
}

/// Push request body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SkillSyncPush {
    /// Entries to publish.
    pub entries: Vec<SkillSyncEntry>,
}

/// One entry's outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SkillSyncAck {
    /// Registry id.
    pub skill_id: String,
    /// Version the server now stores, or the rejected incoming version.
    pub version: String,
    /// Server ETag after the operation.
    pub etag: Option<String>,
    /// Whether the server applied the entry.
    pub accepted: bool,
    /// Why it was rejected.
    pub reason: Option<String>,
}

/// Push response body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SkillSyncPushResponse {
    /// Applied entries.
    pub accepted: Vec<SkillSyncAck>,
    /// Rejected entries.
    pub rejected: Vec<SkillSyncAck>,
    /// Conflicts visible to this user.
    pub conflicts: Vec<SkillSyncConflict>,
    /// User-level cursor after the push.
    pub etag: Option<String>,
    /// Server time, so clients can spot a badly skewed clock.
    pub server_time: DateTime<Utc>,
}

/// Pull request body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SkillSyncPull {
    /// Cursor from a previous response; `None` asks for everything.
    pub since: Option<String>,
}

/// A conflict as the sync server sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SkillSyncConflict {
    /// Registry id.
    pub skill_id: String,
    /// Version the server kept.
    pub local_version: Option<String>,
    /// Version that lost.
    pub remote_version: String,
    /// Why the conflict exists.
    pub reason: String,
    /// When the server detected it.
    pub at: DateTime<Utc>,
}

/// Pull response body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SkillSyncPullResponse {
    /// Changed entries since the cursor.
    pub entries: Vec<SkillSyncEntry>,
    /// Conflicts visible to this user.
    pub conflicts: Vec<SkillSyncConflict>,
    /// Cursor for the next pull.
    pub etag: Option<String>,
    /// Server time.
    pub server_time: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_vectors_detect_dominance_and_concurrency() {
        let mut a = SkillVersionVector::default();
        a.clocks.insert("m1".into(), 2);
        let mut b = SkillVersionVector::default();
        b.clocks.insert("m1".into(), 1);
        assert!(a.dominates(&b));
        assert!(!b.dominates(&a));
        assert!(!a.concurrent(&b));

        let mut c = SkillVersionVector::default();
        c.clocks.insert("m1".into(), 1);
        c.clocks.insert("m2".into(), 1);
        assert!(a.concurrent(&c));
    }

    #[test]
    fn sources_serialise_with_a_stable_tag() {
        let source = SkillSource::Git {
            url: "https://example.test/s.git".into(),
            commit: "abc".into(),
            subdir: Some("skills/x".into()),
        };
        let value = serde_json::to_value(&source).unwrap();
        assert_eq!(value["type"], "git");
    }
}
