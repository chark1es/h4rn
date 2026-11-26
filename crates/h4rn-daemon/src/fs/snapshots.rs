//! Pre-turn content snapshots for "diff vs agent changes".
//!
//! `git diff` answers "what changed since HEAD". It cannot answer "what did
//! *this turn* change", because a turn starts from a dirty tree and may be one
//! of several turns in the same thread. The daemon therefore records the bytes
//! of each file a turn reports touching, the first time it hears about it, and
//! keys the record by `(thread, turn, path)`.
//!
//! Storage is content-addressed: blobs live once under
//! `<data>/blobs/<aa>/<blake3>`, and each turn has a small JSON manifest under
//! `<data>/snapshots/<thread>/<turn>/manifest.json`. Snapshots are append-only
//! within a turn (the first observation wins), so a later tool call cannot
//! overwrite the base the diff needs.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use h4rn_protocol::{FsTurnChanges, FsTurnFile, ThreadId, TurnId};
use serde::{Deserialize, Serialize};

use super::paths::{FsError, ResolveMode, normalize_relative, relative_of, resolve_inside};
use super::{count_line_delta, text};

/// One captured file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotEntry {
    /// Workspace-relative path.
    pub path: String,
    /// Whether the file existed at capture time.
    pub existed: bool,
    /// Content address of the captured bytes, when they exist and fit the cap.
    pub blob: Option<String>,
    /// Etag of the captured bytes (`None` for a new file).
    pub etag: Option<String>,
    /// Size at capture time.
    pub size_bytes: u64,
    /// True when the file was too large to capture.
    pub skipped: bool,
    /// The file's Unix permission bits at capture time, when the platform has
    /// them. A restore keeps the current bits and only toggles the exec bit, so
    /// this is what an ignored path was executable as before the turn.
    #[serde(default)]
    pub mode: Option<u32>,
    /// Whether the captured content predates the turn.
    ///
    /// A capture is taken the first time a tool names a path. A shell command
    /// can write the path before that, so a capture can hold mid-turn bytes; it
    /// is the pre-turn state only when the file's own content timestamp is no
    /// later than the checkpoint that opened the turn.
    #[serde(default)]
    pub pre_turn: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Manifest {
    thread_id: String,
    turn_id: String,
    workspace: String,
    captured_at: DateTime<Utc>,
    /// The git tree whose contents are the complete pre-turn worktree state,
    /// when one was captured. A rewind needs this to cover files no tool call
    /// named.
    #[serde(default)]
    checkpoint: Option<String>,
    /// Whether `checkpoint` covers the whole worktree, untracked files
    /// included.
    ///
    /// A manifest written before this field existed recorded a tracked-only
    /// tree, which is not a complete checkpoint: a rewind must refuse it rather
    /// than restore a partial set. Missing therefore means incomplete.
    #[serde(default)]
    checkpoint_complete: bool,
    /// Repository-relative paths git ignored when the checkpoint was taken.
    ///
    /// The checkpoint tree cannot see them, so their absence from the tree does
    /// not mean they were absent. An older manifest has no list: its checkpoint
    /// predates the completeness of this record and is refused by the restore.
    #[serde(default)]
    ignored: Vec<String>,
    /// Why this turn's checkpoint cannot be complete, when the capture could
    /// not be taken (an older git, too many ignored paths). The restore names
    /// this instead of a generic refusal.
    #[serde(default)]
    checkpoint_reason: Option<String>,
    files: BTreeMap<String, SnapshotEntry>,
}

/// The content-addressed snapshot store.
#[derive(Clone, Debug)]
pub struct SnapshotStore {
    root: PathBuf,
}

/// Largest file captured into a snapshot.
const MAX_SNAPSHOT_BYTES: u64 = 8 * 1024 * 1024;

impl SnapshotStore {
    /// Opens (and creates) the store under the daemon data directory.
    pub fn new(data_dir: &Path) -> Result<Self, FsError> {
        let root = data_dir.join("snapshots");
        fs::create_dir_all(&root).map_err(|error| FsError::Io(error.to_string()))?;
        fs::create_dir_all(data_dir.join("blobs"))
            .map_err(|error| FsError::Io(error.to_string()))?;
        Ok(Self { root })
    }

    /// Records the current bytes of `paths` for one turn.
    ///
    /// Paths already captured for the turn are left alone: the first
    /// observation is the pre-turn state. Returns the number of new entries.
    pub fn capture(
        &self,
        workspace: &Path,
        thread_id: &ThreadId,
        turn_id: &TurnId,
        paths: &[String],
    ) -> Result<usize, FsError> {
        let manifest_path = self.manifest_path(thread_id, turn_id)?;
        let mut manifest = self
            .load_manifest(&manifest_path)
            .unwrap_or_else(|| Manifest {
                thread_id: thread_id.to_string(),
                turn_id: turn_id.to_string(),
                workspace: workspace.display().to_string(),
                captured_at: Utc::now(),
                checkpoint: None,
                checkpoint_complete: false,
                ignored: Vec::new(),
                checkpoint_reason: None,
                files: BTreeMap::new(),
            });
        let mut added = 0;
        let turn_started = manifest.captured_at;
        for raw in paths {
            let Ok(relative) = normalize_relative(raw) else {
                continue;
            };
            if relative.as_os_str().is_empty() {
                continue;
            }
            let rel = relative_of(workspace, &workspace.join(&relative));
            if manifest.files.contains_key(&rel) {
                continue;
            }
            manifest.files.insert(
                rel.clone(),
                self.capture_one(workspace, &rel, turn_started)?,
            );
            added += 1;
        }
        if added > 0 {
            if let Some(parent) = manifest_path.parent() {
                fs::create_dir_all(parent).map_err(|error| FsError::Io(error.to_string()))?;
            }
            let json = serde_json::to_vec_pretty(&manifest)
                .map_err(|error| FsError::Io(error.to_string()))?;
            fs::write(&manifest_path, json).map_err(|error| FsError::Io(error.to_string()))?;
        }
        Ok(added)
    }

    fn capture_one(
        &self,
        workspace: &Path,
        rel: &str,
        turn_started: DateTime<Utc>,
    ) -> Result<SnapshotEntry, FsError> {
        let absolute = resolve_inside(workspace, rel, ResolveMode::Existing);
        let Ok(path) = absolute else {
            // A path that does not exist yet is a file the turn is about to
            // create; record its absence explicitly so the diff base is "".
            return Ok(SnapshotEntry {
                path: rel.to_owned(),
                existed: false,
                blob: None,
                etag: None,
                size_bytes: 0,
                skipped: false,
                mode: None,
                pre_turn: true,
            });
        };
        let metadata = fs::metadata(&path).map_err(|error| FsError::Io(error.to_string()))?;
        let pre_turn = metadata
            .modified()
            .map(DateTime::<Utc>::from)
            .is_ok_and(|modified| modified <= turn_started);
        let mode = mode_of(&metadata);
        if metadata.len() > MAX_SNAPSHOT_BYTES {
            return Ok(SnapshotEntry {
                path: rel.to_owned(),
                existed: true,
                blob: None,
                etag: None,
                size_bytes: metadata.len(),
                skipped: true,
                mode,
                pre_turn,
            });
        }
        let bytes = fs::read(&path).map_err(|error| FsError::Io(error.to_string()))?;
        let etag = text::etag(&bytes);
        self.write_blob(&etag, &bytes)?;
        Ok(SnapshotEntry {
            path: rel.to_owned(),
            existed: true,
            blob: Some(etag.clone()),
            etag: Some(etag),
            size_bytes: metadata.len(),
            skipped: false,
            mode,
            pre_turn,
        })
    }

    /// Entries captured for one turn, path-sorted.
    pub fn entries(&self, thread_id: &ThreadId, turn_id: &TurnId) -> Vec<SnapshotEntry> {
        let Ok(path) = self.manifest_path(thread_id, turn_id) else {
            return Vec::new();
        };
        self.load_manifest(&path)
            .map(|manifest| manifest.files.into_values().collect())
            .unwrap_or_default()
    }

    /// One entry for a path, when it was captured.
    pub fn entry(
        &self,
        thread_id: &ThreadId,
        turn_id: &TurnId,
        rel: &str,
    ) -> Option<SnapshotEntry> {
        self.entries(thread_id, turn_id)
            .into_iter()
            .find(|entry| entry.path == rel)
    }

    /// The files one turn changed, compared with the current working tree.
    ///
    /// Uses the first observation of each path as the pre-turn base, so the
    /// result stays stable across turns that start from a dirty tree. A path
    /// whose content is unchanged since capture is omitted.
    pub fn turn_changes(
        &self,
        workspace: &Path,
        thread_id: &ThreadId,
        turn_id: &TurnId,
    ) -> FsTurnChanges {
        let entries = self.entries(thread_id, turn_id);
        let complete = self.checkpoint(thread_id, turn_id).is_some();
        let mut files = Vec::new();
        for entry in &entries {
            // Reading the current bytes must not follow a symlink out of the
            // workspace. A path that now escapes is skipped, never reported.
            let current = match resolve_inside(workspace, &entry.path, ResolveMode::Existing) {
                Ok(path) => fs::read(path).ok(),
                Err(FsError::NotFound(_)) => None,
                Err(_) => continue,
            };
            let before = entry.blob.as_deref().and_then(|blob| self.read_blob(blob));
            let before_text = before.as_deref().and_then(text::decode);
            let after_text = current.as_deref().and_then(text::decode);
            let (added, removed) = match (&before_text, &after_text) {
                (Some(before), Some(after)) => count_line_delta(&before.text, &after.text),
                (None, Some(after)) => (after.text.lines().count() as u32, 0),
                (Some(before), None) => (0, before.text.lines().count() as u32),
                (None, None) => (0, 0),
            };
            if added == 0 && removed == 0 && entry.etag == current.as_deref().map(text::etag) {
                continue;
            }
            files.push(FsTurnFile {
                path: entry.path.clone(),
                added,
                removed,
                before_etag: entry.etag.clone(),
                after_etag: current.as_deref().map(text::etag),
            });
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        FsTurnChanges {
            thread_id: thread_id.clone(),
            turn_id: turn_id.clone(),
            note: entries.is_empty().then(|| {
                "No snapshots recorded for this turn yet. They are captured as the turn reports file tool calls."
                    .into()
            }),
            files,
            complete,
        }
    }

    /// Reads a captured blob by content address.
    pub fn read_blob(&self, address: &str) -> Option<Vec<u8>> {
        if address.len() != 64 || !address.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        fs::read(self.blob_path(address)).ok()
    }

    /// Stores bytes and returns their content address.
    ///
    /// The restore journal keeps the bytes a rewind replaced here, so a crash
    /// between applying files and committing the cut can put them back.
    pub fn store_blob(&self, bytes: &[u8]) -> Result<String, FsError> {
        let address = text::etag(bytes);
        self.write_blob(&address, bytes)?;
        Ok(address)
    }

    /// Records the complete pre-turn git tree for a turn.
    ///
    /// `ignored` is the checkpoint's ignored listing (see
    /// [`crate::worktrees::git::Checkpoint::ignored`]); it is stored with the
    /// tree so a restore can tell an ignored path's absence from an absence
    /// that is authoritative.
    pub fn record_checkpoint(
        &self,
        workspace: &Path,
        thread_id: &ThreadId,
        turn_id: &TurnId,
        commit: &str,
        ignored: &[String],
    ) -> Result<(), FsError> {
        let manifest_path = self.manifest_path(thread_id, turn_id)?;
        let mut manifest = self
            .load_manifest(&manifest_path)
            .unwrap_or_else(|| Manifest {
                thread_id: thread_id.to_string(),
                turn_id: turn_id.to_string(),
                workspace: workspace.display().to_string(),
                captured_at: Utc::now(),
                checkpoint: None,
                checkpoint_complete: false,
                ignored: Vec::new(),
                checkpoint_reason: None,
                files: BTreeMap::new(),
            });
        manifest.checkpoint = Some(commit.to_owned());
        // The capture stages the whole worktree in git, untracked files
        // included, so a recorded checkpoint is complete by construction.
        manifest.checkpoint_complete = true;
        manifest.ignored = ignored.to_vec();
        manifest.checkpoint_reason = None;
        if let Some(parent) = manifest_path.parent() {
            fs::create_dir_all(parent).map_err(|error| FsError::Io(error.to_string()))?;
        }
        let json =
            serde_json::to_vec_pretty(&manifest).map_err(|error| FsError::Io(error.to_string()))?;
        fs::write(&manifest_path, json).map_err(|error| FsError::Io(error.to_string()))?;
        Ok(())
    }

    /// Records that one turn's checkpoint could not be taken completely.
    ///
    /// The reason is kept so a restore says why it refuses instead of only
    /// that no complete checkpoint exists.
    pub fn record_checkpoint_incomplete(
        &self,
        workspace: &Path,
        thread_id: &ThreadId,
        turn_id: &TurnId,
        reason: &str,
    ) -> Result<(), FsError> {
        let manifest_path = self.manifest_path(thread_id, turn_id)?;
        let mut manifest = self
            .load_manifest(&manifest_path)
            .unwrap_or_else(|| Manifest {
                thread_id: thread_id.to_string(),
                turn_id: turn_id.to_string(),
                workspace: workspace.display().to_string(),
                captured_at: Utc::now(),
                checkpoint: None,
                checkpoint_complete: false,
                ignored: Vec::new(),
                checkpoint_reason: None,
                files: BTreeMap::new(),
            });
        manifest.checkpoint = None;
        manifest.checkpoint_complete = false;
        manifest.ignored.clear();
        manifest.checkpoint_reason = Some(reason.to_owned());
        if let Some(parent) = manifest_path.parent() {
            fs::create_dir_all(parent).map_err(|error| FsError::Io(error.to_string()))?;
        }
        let json =
            serde_json::to_vec_pretty(&manifest).map_err(|error| FsError::Io(error.to_string()))?;
        fs::write(&manifest_path, json).map_err(|error| FsError::Io(error.to_string()))?;
        Ok(())
    }

    /// Why a turn's files cannot be restored, naming the recorded reason when
    /// the checkpoint itself could not be taken.
    pub fn checkpoint_refusal(&self, thread_id: &ThreadId, turn_id: &TurnId) -> String {
        let reason = self
            .manifest_path(thread_id, turn_id)
            .ok()
            .and_then(|path| self.load_manifest(&path))
            .and_then(|manifest| manifest.checkpoint_reason);
        match reason {
            Some(reason) => format!(
                "{} has no complete file checkpoint: {reason}",
                turn_id.as_str()
            ),
            None => format!(
                "{} has no complete file checkpoint, so its files cannot be restored",
                turn_id.as_str()
            ),
        }
    }

    /// The ignored paths the turn's checkpoint recorded, sorted.
    ///
    /// Empty when the checkpoint has no listing: an older record whose absence
    /// from the tree cannot be trusted for ignored paths.
    pub fn ignored(&self, thread_id: &ThreadId, turn_id: &TurnId) -> Vec<String> {
        let Ok(path) = self.manifest_path(thread_id, turn_id) else {
            return Vec::new();
        };
        self.load_manifest(&path)
            .map(|manifest| manifest.ignored)
            .unwrap_or_default()
    }

    /// Where one restore journal lives: `<restores>/<restore_id>.<thread>.json`.
    ///
    /// The thread is part of the name so startup recovery can still name it
    /// when the journal itself cannot be read, and show the person which thread
    /// was left alone.
    pub fn restore_journal_path(
        &self,
        thread_id: &ThreadId,
        restore_id: &str,
    ) -> Result<PathBuf, FsError> {
        let thread = safe_component(thread_id.as_str())?;
        Ok(self
            .restores_dir()
            .join(format!("{restore_id}.{thread}.json")))
    }

    /// The complete pre-turn git tree recorded for a turn, when there is one.
    ///
    /// A manifest whose checkpoint is missing, or was recorded before
    /// checkpoints had to cover the whole worktree, answers `None`: a rewind
    /// must refuse it rather than restore a tracked-only subset.
    pub fn checkpoint(&self, thread_id: &ThreadId, turn_id: &TurnId) -> Option<String> {
        let path = self.manifest_path(thread_id, turn_id).ok()?;
        let manifest = self.load_manifest(&path)?;
        (manifest.checkpoint_complete)
            .then_some(manifest.checkpoint)
            .flatten()
    }

    /// Where restore journals live, beside the blobs.
    pub fn restores_dir(&self) -> PathBuf {
        self.root.parent().unwrap_or(&self.root).join("restores")
    }

    fn write_blob(&self, address: &str, bytes: &[u8]) -> Result<(), FsError> {
        let path = self.blob_path(address);
        if path.exists() {
            return Ok(());
        }
        let Some(parent) = path.parent() else {
            return Err(FsError::Io(format!(
                "{} has no parent folder",
                path.display()
            )));
        };
        fs::create_dir_all(parent).map_err(|error| FsError::Io(error.to_string()))?;
        // A restore journal names these bytes as its way back, so they must be
        // on disk before the journal can be written.
        let mut file = fs::File::create(&path).map_err(|error| FsError::Io(error.to_string()))?;
        file.write_all(bytes)
            .map_err(|error| FsError::Io(error.to_string()))?;
        file.sync_all()
            .map_err(|error| FsError::Io(error.to_string()))?;
        drop(file);
        sync_dir(parent).map_err(|error| FsError::Io(error.to_string()))
    }

    fn blob_path(&self, address: &str) -> PathBuf {
        self.root
            .parent()
            .unwrap_or(&self.root)
            .join("blobs")
            .join(&address[..2.min(address.len())])
            .join(address)
    }

    fn manifest_path(&self, thread_id: &ThreadId, turn_id: &TurnId) -> Result<PathBuf, FsError> {
        let thread = safe_component(thread_id.as_str())?;
        let turn = safe_component(turn_id.as_str())?;
        Ok(self.root.join(thread).join(turn).join("manifest.json"))
    }

    fn load_manifest(&self, path: &Path) -> Option<Manifest> {
        let bytes = fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }
}

/// Makes a folder's own entries durable (a file created in it, a rename into
/// it).
#[cfg(unix)]
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

/// Makes a folder's own entries durable where the platform supports it.
#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

/// A file's Unix permission bits, when the platform has them.
#[cfg(unix)]
fn mode_of(metadata: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(metadata.permissions().mode() & 0o7777)
}

/// A platform without Unix permission bits has no mode to record.
#[cfg(not(unix))]
fn mode_of(_metadata: &fs::Metadata) -> Option<u32> {
    None
}

/// Makes an identifier safe as one path component without losing uniqueness.
///
/// Ordinary ids (`th_<uuid>`, `turn_<uuid>`) pass through; anything with a
/// separator or traversal sequence is percent-ish escaped with its BLAKE3 hash.
fn safe_component(value: &str) -> Result<String, FsError> {
    let safe = !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        && value != "."
        && value != "..";
    if safe {
        Ok(value.to_owned())
    } else if value.is_empty() {
        Err(FsError::Invalid("snapshot id is empty".into()))
    } else {
        Ok(format!("x{}", &text::etag(value.as_bytes())[..32]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsafe_components_are_hashed_not_traversed() {
        let value = safe_component("../../etc").unwrap();
        assert!(value.starts_with('x'));
        assert!(!value.contains('/'));
        assert!(safe_component("turn_abc-123").is_ok());
        assert!(safe_component("").is_err());
    }

    #[test]
    fn capture_keeps_the_first_observation() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("a.txt"), "before").unwrap();
        let store = SnapshotStore::new(dir.path()).unwrap();
        let thread = ThreadId::new("th_1");
        let turn = TurnId::new("turn_1");
        assert_eq!(
            store
                .capture(&workspace, &thread, &turn, &["a.txt".into()])
                .unwrap(),
            1
        );
        fs::write(workspace.join("a.txt"), "after").unwrap();
        assert_eq!(
            store
                .capture(&workspace, &thread, &turn, &["a.txt".into()])
                .unwrap(),
            0
        );
        let entry = store.entry(&thread, &turn, "a.txt").unwrap();
        assert_eq!(
            store.read_blob(entry.blob.as_deref().unwrap()).unwrap(),
            b"before"
        );
    }

    #[test]
    fn turn_changes_reports_a_file_the_turn_created() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let store = SnapshotStore::new(dir.path()).unwrap();
        let thread = ThreadId::new("th_1");
        let turn = TurnId::new("turn_1");

        // The turn announces a file before it exists, then writes it.
        assert_eq!(
            store
                .capture(&workspace, &thread, &turn, &["notes.txt".into()])
                .unwrap(),
            1
        );
        fs::write(workspace.join("notes.txt"), "hello\n").unwrap();

        let changes = store.turn_changes(&workspace, &thread, &turn);
        assert_eq!(changes.files.len(), 1);
        assert_eq!(changes.files[0].path, "notes.txt");
        assert_eq!(changes.files[0].added, 1);
        assert_eq!(changes.files[0].removed, 0);
        assert!(changes.files[0].before_etag.is_none());
    }
}
