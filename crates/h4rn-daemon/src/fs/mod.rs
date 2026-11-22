//! Filesystem and git service for IDE mode.
//!
//! The daemon owns all file access (ADR-001). This module is the only place
//! that touches a user's working tree, and it is deliberately paranoid:
//!
//! - a request names a **workspace root** the daemon has authorised (a thread
//!   workspace, or an explicit `H4RN_WORKSPACE_ROOTS` entry), never an absolute
//!   target path;
//! - every path is relative, normalised, canonicalised and then re-checked
//!   against that root, so `..`, absolute paths and symlinks that point outside
//!   the workspace are rejected;
//! - reads have a size limit and refuse binary files with a hint instead of
//!   handing the editor mojibake;
//! - writes are optimistic: the client sends the etag it edited, and a mismatch
//!   returns the current bytes so the UI can ask instead of clobbering;
//! - deletes are reversible (a trash directory under the daemon data dir);
//! - every mutating call is appended to `audit.jsonl` with the device that
//!   asked for it.
//!
//! LSP, debuggers and extensions are explicitly out of scope (ADR-013): this is
//! "look at and lightly edit the file the agent touched".

pub mod git;
pub mod paths;
pub mod search;
pub mod snapshots;
pub mod text;
pub mod trash;
pub mod watcher;

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use h4rn_protocol::{
    DiffAgainst, FileEntry, FileEntryKind, FsBytesOmitted, FsCreateKind, FsDeleteResult, FsDiff,
    FsFileContent, FsListing, FsSearchBatch, FsTurnChanges, FsWriteConflict, FsWriteResult,
    ProtoError, ProtoErrorCode, ThreadId, TurnId,
};
use tokio::sync::mpsc;

pub use paths::{
    FsError, MAX_ENTRIES, MAX_INLINE_BYTES, MAX_READ_BYTES, MAX_WRITE_BYTES, ResolveMode,
    normalize_relative, relative_of, resolve_for_diff, resolve_inside,
};
pub use snapshots::{SnapshotEntry, SnapshotStore};
pub use watcher::{FsChange, WatcherHub};

use search::SearchOptions;

/// One authorisation rule: a canonical workspace root the daemon may serve.
#[derive(Clone, Debug)]
struct Root {
    canonical: PathBuf,
}

/// The daemon's filesystem + git service.
pub struct FsService {
    data_dir: PathBuf,
    roots: RwLock<Vec<Root>>,
    watcher: WatcherHub,
    snapshots: SnapshotStore,
    trash: trash::TrashStore,
    audit_lock: Mutex<()>,
    search_seq: AtomicU64,
}

impl FsService {
    /// Opens the service under the daemon data directory.
    pub fn new(data_dir: impl Into<PathBuf>) -> Result<Self, FsError> {
        let data_dir = data_dir.into();
        let snapshots = SnapshotStore::new(&data_dir)?;
        let trash = trash::TrashStore::new(&data_dir)?;
        let watcher = WatcherHub::new()?;
        Ok(Self {
            data_dir,
            roots: RwLock::new(Vec::new()),
            watcher,
            snapshots,
            trash,
            audit_lock: Mutex::new(()),
            search_seq: AtomicU64::new(0),
        })
    }

    /// Authorises one workspace root.
    ///
    /// The root is canonicalised once; later requests go through
    /// [`FsService::workspace`], which canonicalises their input and compares it
    /// against this list.
    pub fn allow_root(&self, path: impl AsRef<Path>) -> Result<PathBuf, FsError> {
        let canonical = path
            .as_ref()
            .canonicalize()
            .map_err(|error| FsError::NotFound(format!("workspace root: {error}")))?;
        if !canonical.is_dir() {
            return Err(FsError::Invalid(format!(
                "{} is not a directory",
                canonical.display()
            )));
        }
        let mut roots = self
            .roots
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !roots.iter().any(|root| root.canonical == canonical) {
            roots.push(Root {
                canonical: canonical.clone(),
            });
        }
        Ok(canonical)
    }

    /// Authorises every root named in `H4RN_WORKSPACE_ROOTS` (`:`-separated).
    ///
    /// Empty entries are ignored. A missing directory is logged and skipped so
    /// one stale configuration value cannot stop the daemon from booting.
    pub fn allow_configured_roots(&self) {
        let Ok(value) = std::env::var("H4RN_WORKSPACE_ROOTS") else {
            return;
        };
        for entry in value.split(':') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            if let Err(error) = self.allow_root(entry) {
                tracing::warn!(root = entry, %error, "ignoring unusable workspace root");
            }
        }
    }

    /// Every canonical workspace root this daemon will serve.
    pub fn roots(&self) -> Vec<PathBuf> {
        self.roots
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|root| root.canonical.clone())
            .collect()
    }

    /// Snapshot store used by the thread manager's turn recorder.
    pub fn snapshots(&self) -> std::sync::Arc<SnapshotStore> {
        // The store is cheap to clone and share; keep one instance alive for the
        // service's lifetime by leaking a reference-counted handle.
        std::sync::Arc::new(self.snapshots.clone())
    }

    /// Resolves a workspace string to a canonical, authorised root.
    pub fn workspace(&self, raw: &str) -> Result<PathBuf, FsError> {
        let canonical = Path::new(raw)
            .canonicalize()
            .map_err(|error| FsError::NotFound(format!("workspace {raw:?}: {error}")))?;
        let allowed = self
            .roots
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .any(|root| canonical == root.canonical);
        if !allowed {
            return Err(FsError::Forbidden(format!(
                "{} is not an authorised workspace root",
                canonical.display()
            )));
        }
        Ok(canonical)
    }

    // ------------------------------------------------------------------
    // Directories and files
    // ------------------------------------------------------------------

    /// Lists one directory, respecting `.gitignore` and annotating git status.
    pub fn list(&self, workspace: &str, rel: &str) -> Result<FsListing, FsError> {
        let root = self.workspace(workspace)?;
        let dir = resolve_inside(&root, rel, ResolveMode::Existing)?;
        if !dir.is_dir() {
            return Err(FsError::Invalid(format!("{rel:?} is not a directory")));
        }
        let statuses = git::status_map(&root);
        let walker = ignore::WalkBuilder::new(&dir)
            .max_depth(Some(1))
            .hidden(false)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .parents(true)
            .filter_entry(|entry| entry.file_name() != ".git")
            .build();

        let mut entries = Vec::new();
        let mut truncated = false;
        for item in walker {
            let Ok(item) = item else { continue };
            if item.depth() == 0 {
                continue;
            }
            if entries.len() >= MAX_ENTRIES {
                truncated = true;
                break;
            }
            let Some(entry) = entry_for(&root, &item) else {
                continue;
            };
            entries.push(entry);
        }

        entries.sort_by(|left, right| {
            let left_dir = left.kind == FileEntryKind::Directory;
            let right_dir = right.kind == FileEntryKind::Directory;
            right_dir
                .cmp(&left_dir)
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
                .then_with(|| left.name.cmp(&right.name))
        });

        // Directory badges aggregate the deepest status under them so a
        // collapsed folder still shows that something inside it is dirty.
        for entry in &mut entries {
            if entry.kind != FileEntryKind::Directory {
                entry.git_status = statuses.for_path(&entry.path);
            } else {
                entry.git_status = statuses.for_directory(&entry.path);
            }
        }

        Ok(FsListing {
            workspace: root.display().to_string(),
            path: relative_of(&root, &dir),
            entries,
            truncated,
        })
    }

    /// Reads one file with size, binary and encoding detection.
    pub fn read(&self, workspace: &str, rel: &str) -> Result<FsFileContent, FsError> {
        self.read_with(workspace, rel, false)
    }

    /// Reads one file, optionally including the raw bytes as base64.
    ///
    /// The HTML preview asks for the bytes so it can inline a relative image
    /// or font as a `data:` URL. Text reads keep the previous shape.
    pub fn read_with(
        &self,
        workspace: &str,
        rel: &str,
        bytes_base64: bool,
    ) -> Result<FsFileContent, FsError> {
        let root = self.workspace(workspace)?;
        let path = resolve_inside(&root, rel, ResolveMode::Existing)?;
        if !path.is_file() {
            return Err(FsError::Invalid(format!("{rel:?} is not a file")));
        }
        let metadata = fs::metadata(&path).map_err(|error| FsError::Io(error.to_string()))?;
        let size = metadata.len();
        let modified_at = metadata.modified().ok().map(to_utc);
        let rel_path = relative_of(&root, &path);

        // A base64 copy must fit one secure-transport frame. Larger assets
        // still report their metadata so the preview can skip them and keep
        // rendering the rest of the page.
        if bytes_base64 && size > MAX_INLINE_BYTES {
            return Ok(FsFileContent {
                path: rel_path,
                content: None,
                etag: None,
                encoding: "unknown".into(),
                size_bytes: size,
                binary: false,
                truncated: false,
                bytes_base64: None,
                bytes_omitted: Some(FsBytesOmitted::TooLarge),
                modified_at,
                hint: Some(format!(
                    "This asset is {:.1} MiB; the preview embeds files up to {} KiB, so it was skipped.",
                    size as f64 / 1_048_576.0,
                    MAX_INLINE_BYTES / 1024
                )),
            });
        }

        if size > MAX_READ_BYTES {
            return Ok(FsFileContent {
                path: rel_path,
                content: None,
                etag: None,
                encoding: "unknown".into(),
                size_bytes: size,
                binary: false,
                truncated: true,
                bytes_base64: None,
                bytes_omitted: None,
                modified_at,
                hint: Some(format!(
                    "This file is {:.1} MiB; the editor refuses files over {} MiB. Open it in a terminal instead.",
                    size as f64 / 1_048_576.0,
                    MAX_READ_BYTES / 1_048_576
                )),
            });
        }

        let bytes = fs::read(&path).map_err(|error| FsError::Io(error.to_string()))?;
        let etag = text::etag(&bytes);
        let raw = bytes_base64.then(|| {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        });
        let decoded = text::decode(&bytes);
        let has_text = decoded.is_some();
        let encoding = match &decoded {
            Some(text) => text.encoding.to_owned(),
            None => "binary".to_owned(),
        };
        if bytes_base64 {
            // The preview asked for the raw bytes. It does not also need the
            // decoded text, and sending both could push a text asset such as
            // an SVG past one secure-transport frame.
            return Ok(FsFileContent {
                path: rel_path,
                content: None,
                etag: Some(etag),
                encoding,
                size_bytes: size,
                binary: !has_text,
                truncated: false,
                bytes_base64: raw,
                bytes_omitted: None,
                modified_at,
                hint: None,
            });
        }
        match decoded {
            Some(decoded) => Ok(FsFileContent {
                path: rel_path,
                content: Some(decoded.text),
                etag: Some(etag),
                encoding: decoded.encoding.into(),
                size_bytes: size,
                binary: false,
                truncated: false,
                bytes_base64: None,
                bytes_omitted: None,
                modified_at,
                hint: None,
            }),
            None => Ok(FsFileContent {
                path: rel_path,
                content: None,
                etag: Some(etag),
                encoding: "binary".into(),
                size_bytes: size,
                binary: true,
                truncated: false,
                bytes_base64: None,
                bytes_omitted: None,
                modified_at,
                hint: Some(
                    "This looks like a binary file. h4rn does not edit binaries; use a terminal or your IDE."
                        .into(),
                ),
            }),
        }
    }

    /// Writes one file when `base_etag` still matches the bytes on disk.
    ///
    /// A mismatch is not an error: the result carries a conflict payload with
    /// the current content so the client can offer keep-mine / take-theirs.
    pub fn write(
        &self,
        workspace: &str,
        rel: &str,
        content: &str,
        base_etag: Option<&str>,
        device: Option<&str>,
    ) -> Result<FsWriteResult, FsError> {
        let root = self.workspace(workspace)?;
        if content.len() > MAX_WRITE_BYTES {
            return Err(FsError::TooLarge(format!(
                "refusing to write {} bytes; the limit is {} MiB",
                content.len(),
                MAX_WRITE_BYTES / 1_048_576
            )));
        }

        let target = resolve_inside(&root, rel, ResolveMode::Create)?;
        let rel_path = relative_of(&root, &target);
        let existing = fs::read(&target).ok();
        let current_etag = existing.as_deref().map(text::etag);
        let current_encoding = existing
            .as_deref()
            .and_then(text::decode)
            .map(|decoded| decoded.encoding.to_owned());

        if let Some(expected) = base_etag {
            let actual = current_etag.clone();
            if actual.as_deref() != Some(expected) {
                if let Some(bytes) = existing {
                    let decoded = text::decode(&bytes);
                    let _ = self.audit(device, "write-conflict", &root, &rel_path, "conflict");
                    return Ok(FsWriteResult {
                        path: rel_path,
                        etag: current_etag.unwrap_or_default(),
                        size_bytes: bytes.len() as u64,
                        modified_at: fs::metadata(&target)
                            .ok()
                            .and_then(|meta| meta.modified().ok())
                            .map(to_utc),
                        conflict: Some(FsWriteConflict {
                            expected_etag: base_etag.map(str::to_owned),
                            current_etag: Some(text::etag(&bytes)),
                            current_content: decoded.as_ref().map(|text| text.text.clone()),
                            current_encoding: decoded
                                .map(|text| text.encoding.to_owned())
                                .unwrap_or_else(|| "binary".into()),
                        }),
                    });
                }
                // The file the edit was based on is gone (deleted or moved
                // mid-edit). Recreating it silently would resurrect a file
                // someone removed on purpose, so report a conflict with no
                // current etag; the client can retry as a create.
                let _ = self.audit(device, "write-conflict", &root, &rel_path, "deleted");
                return Ok(FsWriteResult {
                    path: rel_path,
                    etag: String::new(),
                    size_bytes: 0,
                    modified_at: None,
                    conflict: Some(FsWriteConflict {
                        expected_etag: base_etag.map(str::to_owned),
                        current_etag: None,
                        current_content: None,
                        current_encoding: "utf-8".into(),
                    }),
                });
            }
        } else if current_etag.is_some() {
            // The caller asked to create a new file and it already exists.
            let bytes = existing.unwrap_or_default();
            let decoded = text::decode(&bytes);
            let _ = self.audit(device, "write-conflict", &root, &rel_path, "conflict");
            return Ok(FsWriteResult {
                path: rel_path,
                etag: text::etag(&bytes),
                size_bytes: bytes.len() as u64,
                modified_at: None,
                conflict: Some(FsWriteConflict {
                    expected_etag: None,
                    current_etag: Some(text::etag(&bytes)),
                    current_content: decoded.as_ref().map(|text| text.text.clone()),
                    current_encoding: decoded
                        .map(|text| text.encoding.to_owned())
                        .unwrap_or_else(|| "binary".into()),
                }),
            });
        }

        let encoding = current_encoding.as_deref().unwrap_or("utf-8");
        let bytes = text::encode(content, encoding);
        atomic_write(&target, &bytes)?;
        let etag = text::etag(&bytes);
        let metadata = fs::metadata(&target).map_err(|error| FsError::Io(error.to_string()))?;
        let _ = self.audit(device, "write", &root, &rel_path, "ok");
        Ok(FsWriteResult {
            path: rel_path,
            etag,
            size_bytes: metadata.len(),
            modified_at: metadata.modified().ok().map(to_utc),
            conflict: None,
        })
    }

    /// Creates an empty file or a directory.
    pub fn create(
        &self,
        workspace: &str,
        rel: &str,
        kind: FsCreateKind,
        device: Option<&str>,
    ) -> Result<FileEntry, FsError> {
        let root = self.workspace(workspace)?;
        let target = resolve_inside(&root, rel, ResolveMode::Create)?;
        if target.exists() {
            return Err(FsError::Conflict(format!("{rel:?} already exists")));
        }
        match kind {
            FsCreateKind::File => {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).map_err(|error| FsError::Io(error.to_string()))?;
                }
                fs::write(&target, b"").map_err(|error| FsError::Io(error.to_string()))?;
            }
            FsCreateKind::Directory => {
                fs::create_dir_all(&target).map_err(|error| FsError::Io(error.to_string()))?;
            }
        }
        let rel_path = relative_of(&root, &target);
        let _ = self.audit(device, "create", &root, &rel_path, "ok");
        entry_at(&root, &target).ok_or_else(|| FsError::Io("created entry vanished".into()))
    }

    /// Moves a path inside the workspace.
    pub fn rename(
        &self,
        workspace: &str,
        from: &str,
        to: &str,
        device: Option<&str>,
    ) -> Result<FileEntry, FsError> {
        let root = self.workspace(workspace)?;
        let source = resolve_inside(&root, from, ResolveMode::Existing)?;
        let target = resolve_inside(&root, to, ResolveMode::Create)?;
        if target.exists() {
            return Err(FsError::Conflict(format!("{to:?} already exists")));
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| FsError::Io(error.to_string()))?;
        }
        fs::rename(&source, &target).map_err(|error| FsError::Io(error.to_string()))?;
        let rel_path = relative_of(&root, &target);
        let _ = self.audit(device, "rename", &root, &rel_path, "ok");
        entry_at(&root, &target).ok_or_else(|| FsError::Io("renamed entry vanished".into()))
    }

    /// Moves a path into the daemon's trash directory.
    pub fn delete(
        &self,
        workspace: &str,
        rel: &str,
        device: Option<&str>,
    ) -> Result<FsDeleteResult, FsError> {
        let root = self.workspace(workspace)?;
        let target = resolve_inside(&root, rel, ResolveMode::Existing)?;
        if target == root {
            return Err(FsError::Forbidden(
                "cannot delete the workspace root".into(),
            ));
        }
        let result = self.trash.move_to_trash(&root, &target)?;
        let _ = self.audit(device, "delete", &root, &result.path, "trashed");
        Ok(result)
    }

    /// Restores a path previously moved by [`FsService::delete`].
    pub fn restore(
        &self,
        workspace: &str,
        trash_id: &str,
        device: Option<&str>,
    ) -> Result<String, FsError> {
        let root = self.workspace(workspace)?;
        let path = self.trash.restore(&root, trash_id)?;
        let _ = self.audit(device, "restore", &root, &path, "ok");
        Ok(path)
    }

    /// Starts a streamed workspace search.
    pub fn search(
        &self,
        workspace: &str,
        query: &str,
        regex: bool,
        case_sensitive: bool,
        glob: Option<&str>,
        max_results: Option<u32>,
    ) -> Result<(String, mpsc::Receiver<FsSearchBatch>), FsError> {
        let root = self.workspace(workspace)?;
        let query = query.trim();
        if query.is_empty() {
            return Err(FsError::Invalid("search query is empty".into()));
        }
        let id = self.search_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let search_id = format!("search-{id}");
        let options = SearchOptions {
            workspace: root,
            query: query.to_owned(),
            regex,
            case_sensitive,
            glob: glob.map(str::to_owned),
            max_results: max_results
                .map(|value| value.clamp(1, 5_000) as usize)
                .unwrap_or(1_000),
        };
        let rx = search::spawn(&search_id, options)?;
        Ok((search_id, rx))
    }

    // ------------------------------------------------------------------
    // Git diffs and agent-turn snapshots
    // ------------------------------------------------------------------

    /// Compares one file against `HEAD` or one turn's pre-turn snapshot.
    pub fn diff(
        &self,
        workspace: &str,
        rel: &str,
        against: &DiffAgainst,
    ) -> Result<FsDiff, FsError> {
        let root = self.workspace(workspace)?;
        // A diff reads the file, so it crosses the same boundary as `read`:
        // canonicalise the target and refuse anything that escapes the root.
        // The target may be gone — a deletion is exactly what a diff must show
        // — so resolve the way `create` does: canonicalise the nearest existing
        // ancestor and append the missing parts. The leaf is special: a dangling
        // or escaping symlink is not followed (reading it would fail or leave
        // the workspace), so it reads as `after: None`, never as an error.
        let relative = normalize_relative(rel)?;
        let rel_path = relative_of(&root, &root.join(&relative));
        let path = resolve_for_diff(&root, rel)?;
        let current = path.and_then(|path| fs::read(path).ok());
        let after = current.as_deref().and_then(text::decode);
        let after_text = after.as_ref().map(|decoded| decoded.text.clone());
        let binary = current
            .as_deref()
            .map(|bytes| text::decode(bytes).is_none())
            .unwrap_or(false);

        match against {
            DiffAgainst::Head => {
                let before = git::head_blob(&root, &rel_path)?;
                let before_text = before.as_deref().and_then(text::decode);
                let binary = binary
                    || before
                        .as_deref()
                        .map(|bytes| text::decode(bytes).is_none())
                        .unwrap_or(false);
                Ok(FsDiff {
                    path: rel_path,
                    against: against.clone(),
                    label: "HEAD".into(),
                    before: before_text.map(|decoded| decoded.text),
                    after: after_text,
                    binary,
                    note: (before.is_none())
                        .then(|| "No HEAD version: the file is new or not tracked by git.".into()),
                })
            }
            DiffAgainst::AgentTurn { thread_id, turn_id } => {
                let entry = self.snapshots.entry(thread_id, turn_id, &rel_path);
                let (before, note) = match entry {
                    Some(entry) if entry.existed => {
                        let bytes = entry
                            .blob
                            .as_deref()
                            .and_then(|blob| self.snapshots.read_blob(blob));
                        (bytes, None)
                    }
                    Some(_) => (
                        None,
                        Some("The file did not exist when this turn started.".into()),
                    ),
                    None => (
                        None,
                        Some(
                            "No pre-turn snapshot for this file. The daemon records snapshots when a turn's file tools first report the path."
                                .into(),
                        ),
                    ),
                };
                let before_text = before.as_deref().and_then(text::decode);
                let binary = binary
                    || before
                        .as_deref()
                        .map(|bytes| text::decode(bytes).is_none())
                        .unwrap_or(false);
                Ok(FsDiff {
                    path: rel_path,
                    against: against.clone(),
                    label: format!("Turn {}", short_turn(turn_id.as_str())),
                    before: before_text.map(|decoded| decoded.text),
                    after: after_text,
                    binary,
                    note,
                })
            }
        }
    }

    /// Lists the files one turn touched with added/removed line counts.
    pub fn turn_changes(
        &self,
        workspace: &str,
        thread_id: &ThreadId,
        turn_id: &TurnId,
    ) -> Result<FsTurnChanges, FsError> {
        let root = self.workspace(workspace)?;
        Ok(self.snapshots.turn_changes(&root, thread_id, turn_id))
    }

    // ------------------------------------------------------------------
    // Watching and audit
    // ------------------------------------------------------------------

    /// Subscribes to filesystem changes.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<FsChange> {
        self.watcher.subscribe()
    }

    /// Ensures a recursive watcher is running for the workspace.
    pub fn watch(&self, workspace: &str) -> Result<PathBuf, FsError> {
        let root = self.workspace(workspace)?;
        self.watcher.ensure(&root)?;
        Ok(root)
    }

    /// Appends one line to the daemon's audit log.
    fn audit(
        &self,
        device: Option<&str>,
        action: &str,
        workspace: &Path,
        path: &str,
        outcome: &str,
    ) -> std::io::Result<()> {
        let _guard = self
            .audit_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = serde_json::json!({
            "at": Utc::now(),
            "device": device.unwrap_or("local"),
            "action": action,
            "workspace": workspace.display().to_string(),
            "path": path,
            "outcome": outcome,
        });
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.data_dir.join("audit.jsonl"))?;
        writeln!(file, "{entry}")
    }
}

/// Maps a service failure to the wire error, with a user-facing hint.
impl FsError {
    /// Converts to the protocol error code and a hint the UI can render.
    pub fn into_proto(self) -> ProtoError {
        let code = match self {
            Self::Forbidden(_) => ProtoErrorCode::Forbidden,
            Self::NotFound(_) => ProtoErrorCode::NotFound,
            Self::Conflict(_) => ProtoErrorCode::Conflict,
            Self::Invalid(_) | Self::TooLarge(_) | Self::Binary(_) => {
                ProtoErrorCode::InvalidRequest
            }
            Self::Unsupported(_) => ProtoErrorCode::Unsupported,
            Self::Io(_) => ProtoErrorCode::Internal,
        };
        ProtoError::plain(code, self.to_string())
    }
}

fn entry_for(root: &Path, item: &ignore::DirEntry) -> Option<FileEntry> {
    let path = item.path();
    let file_type = item.file_type()?;
    let metadata = fs::symlink_metadata(path).ok();
    let kind = if file_type.is_symlink() {
        FileEntryKind::Symlink
    } else if file_type.is_dir() {
        FileEntryKind::Directory
    } else {
        FileEntryKind::File
    };
    Some(FileEntry {
        name: item.file_name().to_string_lossy().into_owned(),
        path: relative_of(root, path),
        kind,
        size_bytes: metadata.as_ref().map(|meta| meta.len()).unwrap_or(0),
        modified_at: metadata
            .as_ref()
            .and_then(|meta| meta.modified().ok())
            .map(to_utc),
        git_status: None,
    })
}

fn entry_at(root: &Path, path: &Path) -> Option<FileEntry> {
    let metadata = fs::symlink_metadata(path).ok()?;
    let file_type = metadata.file_type();
    let kind = if file_type.is_symlink() {
        FileEntryKind::Symlink
    } else if file_type.is_dir() {
        FileEntryKind::Directory
    } else {
        FileEntryKind::File
    };
    Some(FileEntry {
        name: path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| relative_of(root, path)),
        path: relative_of(root, path),
        kind,
        size_bytes: metadata.len(),
        modified_at: metadata.modified().ok().map(to_utc),
        git_status: None,
    })
}

fn atomic_write(target: &Path, bytes: &[u8]) -> Result<(), FsError> {
    let parent = target
        .parent()
        .ok_or_else(|| FsError::Invalid("file has no parent directory".into()))?;
    fs::create_dir_all(parent).map_err(|error| FsError::Io(error.to_string()))?;
    let temp = parent.join(format!(
        ".h4rn-{}.tmp",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default()
    ));
    fs::write(&temp, bytes).map_err(|error| FsError::Io(error.to_string()))?;
    #[cfg(unix)]
    if let Ok(metadata) = fs::metadata(target) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(
            &temp,
            fs::Permissions::from_mode(metadata.permissions().mode()),
        );
    }
    fs::rename(&temp, target).map_err(|error| {
        let _ = fs::remove_file(&temp);
        FsError::Io(error.to_string())
    })
}

fn to_utc(value: SystemTime) -> DateTime<Utc> {
    DateTime::<Utc>::from(value)
}

fn short_turn(value: &str) -> String {
    value
        .strip_prefix("turn_")
        .unwrap_or(value)
        .chars()
        .take(8)
        .collect()
}

/// Counts added/removed lines between two texts.
///
/// Uses an LCS for ordinary files and a multiset comparison for very large
/// ones, where the quadratic table would cost more than the number is worth.
pub fn count_line_delta(before: &str, after: &str) -> (u32, u32) {
    let left: Vec<&str> = before.lines().collect();
    let right: Vec<&str> = after.lines().collect();
    if left == right {
        return (0, 0);
    }
    if left.len().saturating_mul(right.len()) > 2_250_000 {
        let mut counts: BTreeMap<&str, i64> = BTreeMap::new();
        for line in &left {
            *counts.entry(line).or_default() -= 1;
        }
        for line in &right {
            *counts.entry(line).or_default() += 1;
        }
        let mut added = 0u32;
        let mut removed = 0u32;
        for delta in counts.values() {
            if *delta > 0 {
                added += *delta as u32;
            } else {
                removed += (-*delta) as u32;
            }
        }
        return (added, removed);
    }

    let mut previous = vec![0usize; right.len() + 1];
    let mut current = vec![0usize; right.len() + 1];
    for left_line in &left {
        for (index, right_line) in right.iter().enumerate() {
            current[index + 1] = if left_line == right_line {
                previous[index] + 1
            } else {
                current[index].max(previous[index + 1])
            };
        }
        std::mem::swap(&mut previous, &mut current);
        current.fill(0);
    }
    let common = previous[right.len()] as u32;
    (right.len() as u32 - common, left.len() as u32 - common)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_delta_counts_additions_and_removals() {
        assert_eq!(count_line_delta("a\nb\n", "a\nb\n"), (0, 0));
        assert_eq!(count_line_delta("a\nb\n", "a\nc\n"), (1, 1));
        assert_eq!(count_line_delta("a\n", "a\nb\n"), (1, 0));
        assert_eq!(count_line_delta("a\nb\n", "a\n"), (0, 1));
    }

    #[test]
    fn relative_paths_use_forward_slashes() {
        let root = Path::new("/repo");
        assert_eq!(
            relative_of(root, &root.join("src").join("main.rs")),
            "src/main.rs"
        );
        assert_eq!(relative_of(root, root), "");
    }
}
