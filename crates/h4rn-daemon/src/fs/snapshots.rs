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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Manifest {
    thread_id: String,
    turn_id: String,
    workspace: String,
    captured_at: DateTime<Utc>,
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
                files: BTreeMap::new(),
            });
        let mut added = 0;
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
            manifest
                .files
                .insert(rel.clone(), self.capture_one(workspace, &rel)?);
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

    fn capture_one(&self, workspace: &Path, rel: &str) -> Result<SnapshotEntry, FsError> {
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
            });
        };
        let metadata = fs::metadata(&path).map_err(|error| FsError::Io(error.to_string()))?;
        if metadata.len() > MAX_SNAPSHOT_BYTES {
            return Ok(SnapshotEntry {
                path: rel.to_owned(),
                existed: true,
                blob: None,
                etag: None,
                size_bytes: metadata.len(),
                skipped: true,
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
        let mut files = Vec::new();
        for entry in &entries {
            let Ok(relative) = normalize_relative(&entry.path) else {
                continue;
            };
            let current = fs::read(workspace.join(relative)).ok();
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
        }
    }

    /// Reads a captured blob by content address.
    pub fn read_blob(&self, address: &str) -> Option<Vec<u8>> {
        if address.len() != 64 || !address.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        fs::read(self.blob_path(address)).ok()
    }

    fn write_blob(&self, address: &str, bytes: &[u8]) -> Result<(), FsError> {
        let path = self.blob_path(address);
        if path.exists() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| FsError::Io(error.to_string()))?;
        }
        fs::write(&path, bytes).map_err(|error| FsError::Io(error.to_string()))
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
}
