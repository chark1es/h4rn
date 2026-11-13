//! Reversible deletes: the daemon's trash directory.
//!
//! IDE mode must not offer an unrecoverable delete next to a normal save button
//! (DESIGN.md: reversible by default). `FsDelete` moves the entry into
//! `<data>/trash/<id>/payload` and writes a small `meta.json`; `FsRestore`
//! moves it back only when the recorded workspace still matches and the target
//! is still inside that workspace.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use h4rn_protocol::FsDeleteResult;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::paths::{FsError, normalize_relative, relative_of};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TrashMeta {
    original_path: String,
    workspace: String,
    deleted_at: DateTime<Utc>,
    name: String,
}

/// The daemon-local trash directory.
#[derive(Clone, Debug)]
pub struct TrashStore {
    root: PathBuf,
}

impl TrashStore {
    /// Opens (and creates) the trash directory under the daemon data dir.
    pub fn new(data_dir: &Path) -> Result<Self, FsError> {
        let root = data_dir.join("trash");
        fs::create_dir_all(&root).map_err(|error| FsError::Io(error.to_string()))?;
        Ok(Self { root })
    }

    /// Moves `target` into the trash and returns the restore handle.
    pub fn move_to_trash(
        &self,
        workspace: &Path,
        target: &Path,
    ) -> Result<FsDeleteResult, FsError> {
        let rel = relative_of(workspace, target);
        if rel.is_empty() {
            return Err(FsError::Forbidden(
                "cannot delete the workspace root".into(),
            ));
        }
        // Re-prove containment; the caller already resolved the path, and this
        // keeps the trash store safe if it is ever called from somewhere else.
        let relative = normalize_relative(&rel)?;
        let canonical = workspace
            .join(&relative)
            .canonicalize()
            .map_err(|error| FsError::NotFound(error.to_string()))?;
        if !canonical.starts_with(workspace) {
            return Err(FsError::Forbidden(
                "refusing to trash a path outside the workspace".into(),
            ));
        }

        let id = Uuid::now_v7().to_string();
        let folder = self.root.join(&id);
        fs::create_dir_all(&folder).map_err(|error| FsError::Io(error.to_string()))?;
        let name = target
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "entry".into());
        let payload = folder.join("payload");
        fs::rename(target, &payload).map_err(|error| FsError::Io(error.to_string()))?;
        let meta = TrashMeta {
            original_path: rel.clone(),
            workspace: workspace.display().to_string(),
            deleted_at: Utc::now(),
            name,
        };
        let json =
            serde_json::to_vec_pretty(&meta).map_err(|error| FsError::Io(error.to_string()))?;
        fs::write(folder.join("meta.json"), json)
            .map_err(|error| FsError::Io(error.to_string()))?;

        Ok(FsDeleteResult {
            path: rel,
            trash_id: id,
            trashed_to: payload.display().to_string(),
        })
    }

    /// Moves a trashed entry back to its recorded workspace-relative path.
    pub fn restore(&self, workspace: &Path, trash_id: &str) -> Result<String, FsError> {
        if trash_id.is_empty()
            || !trash_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(FsError::Invalid("trash id is not valid".into()));
        }
        let folder = self.root.join(trash_id);
        let meta_bytes = fs::read(folder.join("meta.json"))
            .map_err(|_| FsError::NotFound(format!("no trashed entry {trash_id:?}")))?;
        let meta: TrashMeta = serde_json::from_slice(&meta_bytes)
            .map_err(|error| FsError::Invalid(format!("trash metadata: {error}")))?;
        if Path::new(&meta.workspace) != workspace {
            return Err(FsError::Forbidden(format!(
                "that entry was deleted from a different workspace ({})",
                meta.workspace
            )));
        }
        let relative = normalize_relative(&meta.original_path)?;
        let target = workspace.join(&relative);
        if target.exists() {
            return Err(FsError::Conflict(format!(
                "{} already exists; move it away before restoring",
                meta.original_path
            )));
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| FsError::Io(error.to_string()))?;
        }
        fs::rename(folder.join("payload"), &target)
            .map_err(|error| FsError::Io(error.to_string()))?;
        let _ = fs::remove_file(folder.join("meta.json"));
        let _ = fs::remove_dir(&folder);
        Ok(meta.original_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_and_restore_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        fs::create_dir_all(workspace.join("src")).unwrap();
        let file = workspace.join("src").join("a.txt");
        fs::write(&file, "hello").unwrap();
        let store = TrashStore::new(dir.path()).unwrap();
        let deleted = store.move_to_trash(&workspace, &file).unwrap();
        assert!(!file.exists());
        assert_eq!(deleted.path, "src/a.txt");
        assert_eq!(
            store.restore(&workspace, &deleted.trash_id).unwrap(),
            "src/a.txt"
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), "hello");
    }

    #[test]
    fn restore_refuses_an_unknown_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = TrashStore::new(dir.path()).unwrap();
        assert!(store.restore(dir.path(), "../etc").is_err());
        assert!(store.restore(dir.path(), "missing").is_err());
    }
}
