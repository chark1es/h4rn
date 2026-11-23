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
pub mod git_diff;
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
    DiffAgainst, FileEntry, FileEntryKind, FsBranchList, FsCreateKind, FsDeleteResult, FsDiff,
    FsDiffCommitList, FsDiffFiles, FsFileContent, FsListing, FsOmitted, FsSearchBatch,
    FsTurnChanges, FsWriteConflict, FsWriteResult, ProtoError, ProtoErrorCode, ThreadId, TurnId,
};
use tokio::sync::mpsc;

pub use paths::{
    FRAME_HEADROOM_BYTES, FsError, MAX_ENTRIES, MAX_INLINE_BYTES, MAX_READ_BYTES,
    MAX_RESPONSE_BYTES, MAX_WRITE_BYTES, ResolveMode, normalize_relative, relative_of,
    resolve_for_diff, resolve_inside,
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
    ///
    /// This is the editor's read on the plain loopback connection, which has
    /// no transport frame limit: the documented 2 MiB editor limit is the only
    /// bound.
    pub fn read(&self, workspace: &str, rel: &str) -> Result<FsFileContent, FsError> {
        self.read_with(workspace, rel, false, None, None)
    }

    /// Reads one file, optionally including the raw bytes as base64.
    ///
    /// The HTML preview asks for the bytes so it can inline a relative image
    /// or font as a `data:` URL. Text reads keep the previous shape, with
    /// `max_bytes` as an optional preview-sized cap instead of the editor's
    /// full read limit.
    ///
    /// `frame_limit` is the transport frame bound for this session, when the
    /// transport has one (the Noise link does; the plain loopback WebSocket
    /// does not). A payload whose serialized size would exceed it comes back
    /// typed instead of closing the session. A preview read (`max_bytes` set)
    /// keeps the same check even on loopback: its cap is chosen for a secure
    /// frame, and the response shape should not depend on which transport
    /// happened to answer.
    pub fn read_with(
        &self,
        workspace: &str,
        rel: &str,
        bytes_base64: bool,
        max_bytes: Option<u64>,
        frame_limit: Option<usize>,
    ) -> Result<FsFileContent, FsError> {
        let enforce_frame = frame_limit.is_some() || max_bytes.is_some();
        let frame_limit = frame_limit.unwrap_or(MAX_RESPONSE_BYTES);
        let root = self.workspace(workspace)?;
        let path = resolve_inside(&root, rel, ResolveMode::Existing)?;
        if !path.is_file() {
            return Err(FsError::Invalid(format!("{rel:?} is not a file")));
        }
        let metadata = fs::metadata(&path).map_err(|error| FsError::Io(error.to_string()))?;
        let modified_at = metadata.modified().ok().map(to_utc);
        let rel_path = relative_of(&root, &path);

        // Both payload kinds are read with a hard bound: `cap + 1` bytes is
        // enough to tell an over-limit file from a file that fits, and a file
        // that grows while it is being read can never exceed the cap. Metadata
        // is only used for the message, never to decide how much to read.
        let text_cap = max_bytes.map_or(MAX_READ_BYTES, |requested| requested.min(MAX_READ_BYTES));
        // A cap below the editor's own limit is a preview read: its payload is
        // optional, so a file that does not fit comes back as a typed omission
        // instead of the editor's "too large" shape.
        let preview_read = text_cap < MAX_READ_BYTES;
        let cap = if bytes_base64 {
            MAX_INLINE_BYTES
        } else {
            text_cap
        };
        let bytes = read_bounded(&path, cap)?;
        // The size the response reports is the number of bytes the read
        // actually delivered, not the metadata sampled before it: a file that
        // shrank in between must not be described by its stale size.
        let delivered_size = bytes.len() as u64;
        // `read_bounded` always stops at `cap + 1`, so an omitted payload can
        // only promise "at least this many bytes".

        if bytes_base64 && delivered_size > cap {
            return Ok(FsFileContent {
                path: rel_path,
                content: None,
                etag: None,
                encoding: "unknown".into(),
                size_bytes: delivered_size,
                binary: false,
                truncated: false,
                bytes_base64: None,
                bytes_omitted: Some(FsOmitted::TooLarge),
                content_omitted: None,
                modified_at,
                hint: Some(format!(
                    "This asset is at least {}; the preview embeds assets up to {}, so it was skipped.",
                    kibibytes(delivered_size),
                    kibibytes(MAX_INLINE_BYTES)
                )),
            });
        }

        if !bytes_base64 && delivered_size > cap {
            return Ok(FsFileContent {
                path: rel_path,
                content: None,
                etag: None,
                encoding: "unknown".into(),
                size_bytes: delivered_size,
                binary: false,
                truncated: !preview_read,
                bytes_base64: None,
                bytes_omitted: None,
                content_omitted: Some(FsOmitted::TooLarge),
                modified_at,
                hint: Some(if preview_read {
                    format!(
                        "This file is at least {}; a preview reads at most {} of text so one response fits a transport frame.",
                        kibibytes(delivered_size),
                        kibibytes(text_cap)
                    )
                } else {
                    format!(
                        "This file is at least {}; the editor refuses files over {}. Open it in a terminal instead.",
                        kibibytes(delivered_size),
                        kibibytes(MAX_READ_BYTES)
                    )
                }),
            });
        }

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
            // an SVG past one secure-transport frame. The per-response cap
            // already keeps an ordinary asset inside one frame; the measured
            // check only runs where a frame limit exists.
            let file = FsFileContent {
                path: rel_path,
                content: None,
                etag: Some(etag),
                encoding,
                size_bytes: delivered_size,
                binary: !has_text,
                truncated: false,
                bytes_base64: raw,
                bytes_omitted: None,
                content_omitted: None,
                modified_at,
                hint: None,
            };
            return Ok(if !enforce_frame || file_fits_frame(&file, frame_limit) {
                file
            } else {
                omit_file_payload(file)
            });
        }
        match decoded {
            Some(decoded) => {
                let file = FsFileContent {
                    path: rel_path,
                    content: Some(decoded.text),
                    etag: Some(etag),
                    encoding: decoded.encoding.into(),
                    size_bytes: delivered_size,
                    binary: false,
                    truncated: false,
                    bytes_base64: None,
                    bytes_omitted: None,
                    content_omitted: None,
                    modified_at,
                    hint: None,
                };
                // JSON escaping of quotes, backslashes, newlines and control
                // characters can make the serialized response much larger than
                // the file on disk. A secure transport closes the whole
                // session on an oversized frame, so a payload that would not
                // fit comes back typed instead. Editor reads on the plain
                // loopback connection have no frame limit and keep the whole
                // 2 MiB the editor documents.
                Ok(if !enforce_frame || file_fits_frame(&file, frame_limit) {
                    file
                } else {
                    FsFileContent {
                        content: None,
                        content_omitted: Some(FsOmitted::TooLarge),
                        truncated: !preview_read,
                        hint: Some(frame_hint(file.size_bytes)),
                        ..file
                    }
                })
            }
            None => Ok(FsFileContent {
                path: rel_path,
                content: None,
                etag: Some(etag),
                encoding: "binary".into(),
                size_bytes: delivered_size,
                binary: true,
                truncated: false,
                bytes_base64: None,
                bytes_omitted: None,
                content_omitted: None,
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

    /// Compares one file against `HEAD`, one turn, a branch's merge base or one commit.
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

        match against {
            DiffAgainst::Head => {
                // The working-tree side is bounded exactly like an editor read:
                // an oversized file is reported as too large instead of being
                // loaded.
                let working = working_side(path.as_deref());
                let mut too_large = working.too_large;
                // `HEAD:./path` is relative to the workspace, which can be a
                // subdirectory of the repository; go through the repository
                // path so the blob is size-checked before it is read.
                let before = match git::repository_root(&root) {
                    Some(repo) => {
                        let repo_rel = git_diff::repo_path(&root, &repo, &rel_path);
                        match git_diff::blob_at(&repo, "HEAD", &repo_rel)? {
                            git_diff::Blob::Bytes(bytes) => Some(bytes),
                            git_diff::Blob::TooLarge => {
                                too_large = true;
                                None
                            }
                            git_diff::Blob::Missing => None,
                        }
                    }
                    None => None,
                };
                let before_text = before.as_deref().and_then(text::decode);
                let binary = working.binary
                    || before
                        .as_deref()
                        .map(|bytes| text::decode(bytes).is_none())
                        .unwrap_or(false);
                Ok(FsDiff {
                    path: rel_path,
                    against: against.clone(),
                    label: "HEAD".into(),
                    before: before_text.map(|decoded| decoded.text),
                    after: working.after,
                    binary,
                    too_large,
                    note: if too_large {
                        Some("This file is too large to diff.".into())
                    } else if before.is_none() {
                        Some("No HEAD version: the file is new or not tracked by git.".into())
                    } else {
                        None
                    },
                })
            }
            DiffAgainst::AgentTurn { thread_id, turn_id } => {
                let working = working_side(path.as_deref());
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
                let binary = working.binary
                    || before
                        .as_deref()
                        .map(|bytes| text::decode(bytes).is_none())
                        .unwrap_or(false);
                Ok(FsDiff {
                    path: rel_path,
                    against: against.clone(),
                    label: format!("Turn {}", short_turn(turn_id.as_str())),
                    before: before_text.map(|decoded| decoded.text),
                    after: working.after,
                    binary,
                    too_large: working.too_large,
                    note: if working.too_large {
                        Some("This file is too large to diff.".into())
                    } else {
                        note
                    },
                })
            }
            DiffAgainst::Base { base } => {
                let working = working_side(path.as_deref());
                let repo = git::repository_root(&root).ok_or_else(|| {
                    FsError::NotFound("This workspace is not a git repository.".into())
                })?;
                let info = git_diff::resolve_base(&root, base.as_deref())?;
                let repo_rel = git_diff::repo_path(&root, &repo, &rel_path);
                let mut too_large = working.too_large;
                let range = [&info.merge_base[..]];
                let before = blob_following_rename(
                    &repo,
                    &info.merge_base,
                    &range,
                    &repo_rel,
                    &mut too_large,
                )?;
                let before_text = before.as_deref().and_then(text::decode);
                let binary = working.binary
                    || before
                        .as_deref()
                        .map(|bytes| text::decode(bytes).is_none())
                        .unwrap_or(false);
                let note = if too_large {
                    Some("This file is too large to diff.".into())
                } else if before.is_none() {
                    Some("No base version: the file is new since the merge base.".into())
                } else if !working.present {
                    Some("This file was deleted since the merge base.".into())
                } else {
                    None
                };
                Ok(FsDiff {
                    path: rel_path,
                    against: against.clone(),
                    label: format!("vs {}", info.name),
                    before: before_text.map(|decoded| decoded.text),
                    after: working.after,
                    binary,
                    too_large,
                    note,
                })
            }
            DiffAgainst::Commit { sha } => {
                let repo = git::repository_root(&root).ok_or_else(|| {
                    FsError::NotFound("This workspace is not a git repository.".into())
                })?;
                let full = git_diff::resolve_commit(&repo, sha)?;
                let short = git_diff::short_sha(&repo, &full);
                let repo_rel = git_diff::repo_path(&root, &repo, &rel_path);
                let parent = git_diff::first_parent(&repo, &full)?;
                // A commit review shows the commit's own blobs. The working
                // tree may be any size; it is not part of this answer.
                let mut too_large = false;
                let range = [&parent[..], &full[..]];
                let before =
                    blob_following_rename(&repo, &parent, &range, &repo_rel, &mut too_large)?;
                let committed = match git_diff::blob_at(&repo, &full, &repo_rel)? {
                    git_diff::Blob::Bytes(bytes) => Some(bytes),
                    git_diff::Blob::TooLarge => {
                        too_large = true;
                        None
                    }
                    git_diff::Blob::Missing => None,
                };
                let before_text = before.as_deref().and_then(text::decode);
                let after_text = committed.as_deref().and_then(text::decode);
                let binary = before
                    .as_deref()
                    .map(|bytes| text::decode(bytes).is_none())
                    .unwrap_or(false)
                    || committed
                        .as_deref()
                        .map(|bytes| text::decode(bytes).is_none())
                        .unwrap_or(false);
                let root_commit = parent == git_diff::EMPTY_TREE;
                let note = if too_large {
                    Some("This file is too large to diff.".into())
                } else if before.is_none() && committed.is_none() {
                    Some("This commit does not change this path.".into())
                } else if before.is_none() && root_commit {
                    Some("The root commit adds this file.".into())
                } else if before.is_none() {
                    Some("Added by this commit.".into())
                } else if committed.is_none() {
                    Some("Deleted by this commit.".into())
                } else {
                    None
                };
                Ok(FsDiff {
                    path: rel_path,
                    against: against.clone(),
                    label: format!("Commit {short}"),
                    before: before_text.map(|decoded| decoded.text),
                    after: after_text.map(|decoded| decoded.text),
                    binary,
                    too_large,
                    note,
                })
            }
        }
    }

    /// Lists the files a branch or commit range changed.
    pub fn diff_files(
        &self,
        workspace: &str,
        against: &DiffAgainst,
    ) -> Result<FsDiffFiles, FsError> {
        let root = self.workspace(workspace)?;
        match against {
            DiffAgainst::Base { base } => {
                let (info, range) = git_diff::branch_files(&root, base.as_deref())?;
                let note = range
                    .files
                    .is_empty()
                    .then(|| "No changes against this base.".into());
                Ok(FsDiffFiles {
                    against: against.clone(),
                    label: format!("vs {}", info.name),
                    base: Some(info.name),
                    merge_base: Some(info.merge_base),
                    files: range.files,
                    truncated: range.truncated,
                    note,
                })
            }
            DiffAgainst::Commit { sha } => {
                let (short, range) = git_diff::commit_files(&root, sha)?;
                let note = range
                    .files
                    .is_empty()
                    .then(|| "This commit changed no files.".into());
                Ok(FsDiffFiles {
                    against: against.clone(),
                    label: format!("Commit {short}"),
                    base: None,
                    merge_base: None,
                    files: range.files,
                    truncated: range.truncated,
                    note,
                })
            }
            DiffAgainst::Head | DiffAgainst::AgentTurn { .. } => Err(FsError::Unsupported(
                "A file list is only available for a branch or a commit.".into(),
            )),
        }
    }

    /// Lists the newest commits between a base's merge base and `HEAD`.
    pub fn diff_commits(
        &self,
        workspace: &str,
        base: Option<&str>,
    ) -> Result<FsDiffCommitList, FsError> {
        let root = self.workspace(workspace)?;
        git_diff::commits(&root, base)
    }

    /// Lists the branches a diff can be compared against.
    pub fn branches(&self, workspace: &str) -> Result<FsBranchList, FsError> {
        let root = self.workspace(workspace)?;
        git_diff::branches(&root)
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

/// Reads at most `cap + 1` bytes, so callers can tell an over-limit file from
/// one that fits without ever loading more than the cap into memory.
fn read_bounded(path: &Path, cap: u64) -> Result<Vec<u8>, FsError> {
    use std::io::Read as _;

    let file = fs::File::open(path).map_err(|error| FsError::Io(error.to_string()))?;
    let mut bytes = Vec::new();
    file.take(cap.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| FsError::Io(error.to_string()))?;
    Ok(bytes)
}

/// Bytes the response envelope around one file payload adds at most.
///
/// This covers the field names, punctuation, flags and numbers of
/// [`FsFileContent`] plus the `ResponseBody::FsFile` wrapper and the response
/// frame around it (ids, the tag and its braces), with room to spare. Every
/// string field is measured exactly; the reserved part never carries payload.
const FILE_RESPONSE_ENVELOPE_BYTES: usize = 1024;

/// Exact byte length of a string as `serde_json` writes it inside quotes.
///
/// serde_json escapes `"`, `\`, the five short control escapes and every other
/// control character as `\u00XX`; it leaves `/`, DEL and non-ASCII text in
/// their UTF-8 form. Measuring this way avoids serializing the whole response
/// twice, once to decide whether it fits and once to send it.
fn json_string_len(text: &str) -> usize {
    text.chars()
        .map(|ch| match ch {
            '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
            ch if (ch as u32) < 0x20 => 6,
            ch => ch.len_utf8(),
        })
        .sum()
}

/// Upper bound on the serialized size of one file payload.
fn file_wire_len(file: &FsFileContent) -> usize {
    let mut len = FILE_RESPONSE_ENVELOPE_BYTES + json_string_len(&file.path);
    len += json_string_len(&file.encoding);
    for text in [
        file.etag.as_deref(),
        file.content.as_deref(),
        file.hint.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        len += json_string_len(text);
    }
    len += file.bytes_base64.as_ref().map_or(0, String::len);
    len
}

/// Whether one file read fits inside `limit` bytes of one transport frame.
///
/// Measures the payload's serialized size, escaping included: a text read can
/// be much larger on the wire than on disk when it is full of quotes,
/// backslashes, newlines or control characters. No serialization is needed, so
/// a response is only ever serialized once, when it is sent.
pub fn file_fits_frame(file: &FsFileContent, limit: usize) -> bool {
    file_wire_len(file) <= limit
}

/// Whether one file read fits inside one secure-transport frame.
pub fn file_fits_one_frame(file: &FsFileContent) -> bool {
    file_fits_frame(file, MAX_RESPONSE_BYTES)
}

/// The typed result for a payload the transport could not carry in one frame.
///
/// Keeps the metadata (path, etag, encoding, size, modification time) and
/// replaces the payload with [`FsOmitted::TooLarge`], because the transport
/// closes the whole paired session on a `FrameTooLarge` frame. A read with no
/// payload is returned unchanged.
pub fn omit_file_payload(file: FsFileContent) -> FsFileContent {
    if file.bytes_base64.is_some() {
        return FsFileContent {
            bytes_base64: None,
            bytes_omitted: Some(FsOmitted::TooLarge),
            hint: Some(format!(
                "This asset is at least {}; the preview embeds assets up to {}.",
                kibibytes(file.size_bytes),
                kibibytes(MAX_INLINE_BYTES)
            )),
            ..file
        };
    }
    if file.content.is_some() {
        return FsFileContent {
            content: None,
            content_omitted: Some(FsOmitted::TooLarge),
            truncated: true,
            hint: Some(frame_hint(file.size_bytes)),
            ..file
        };
    }
    file
}

/// The hint for a text payload that does not fit one transport frame.
fn frame_hint(size: u64) -> String {
    format!(
        "This file is {}; escaping its text for the wire needs more than the {} one transport frame allows. Open it in a terminal or split the report.",
        kibibytes(size),
        kibibytes(h4rn_transport::DEFAULT_MAX_FRAME_BYTES as u64)
    )
}

/// Formats a byte count in the unit the limit beside it uses, so the two
/// numbers can be compared at a glance.
fn kibibytes(bytes: u64) -> String {
    format!("{} KiB", bytes.div_ceil(1024))
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
    // A UUIDv7 starts with a timestamp that only ticks over about once a
    // minute, so the first characters are shared by turns a moment apart. The
    // trailing characters are random, so two quick turns still read apart.
    let id = value.strip_prefix("turn_").unwrap_or(value);
    let skip = id.chars().count().saturating_sub(8);
    id.chars().skip(skip).collect()
}

/// The working-tree side of a diff, read within the same bound as an editor.
struct WorkingSide {
    /// The file is over the read cap and was not loaded.
    too_large: bool,
    /// A readable file exists at the path.
    present: bool,
    /// Decoded text, or `None` when the file is missing, oversized or binary.
    after: Option<String>,
    /// The bytes are not text.
    binary: bool,
}

fn working_side(path: Option<&Path>) -> WorkingSide {
    let too_large = path
        .and_then(|path| fs::metadata(path).ok())
        .map(|meta| meta.len() > MAX_READ_BYTES)
        .unwrap_or(false);
    if too_large {
        return WorkingSide {
            too_large,
            present: true,
            after: None,
            binary: false,
        };
    }
    let bytes = path.and_then(|path| fs::read(path).ok());
    let present = bytes.is_some();
    let binary = bytes
        .as_deref()
        .map(|bytes| text::decode(bytes).is_none())
        .unwrap_or(false);
    let after = bytes
        .as_deref()
        .and_then(text::decode)
        .map(|found| found.text);
    WorkingSide {
        too_large: false,
        present,
        after,
        binary,
    }
}

/// Reads a path from a tree, following a detected rename.
///
/// A renamed file has no blob under its new name at the base, so the review
/// would otherwise look like the file was newly added. When git reports the
/// rename in `range`, the old path supplies the real base content.
fn blob_following_rename(
    repo: &Path,
    rev: &str,
    range: &[&str],
    repo_rel: &str,
    too_large: &mut bool,
) -> Result<Option<Vec<u8>>, FsError> {
    match git_diff::blob_at(repo, rev, repo_rel)? {
        git_diff::Blob::Bytes(bytes) => Ok(Some(bytes)),
        git_diff::Blob::TooLarge => {
            *too_large = true;
            Ok(None)
        }
        git_diff::Blob::Missing => {
            let Some(old) = git_diff::renamed_from_range(repo, range, repo_rel) else {
                return Ok(None);
            };
            match git_diff::blob_at(repo, rev, &old)? {
                git_diff::Blob::Bytes(bytes) => Ok(Some(bytes)),
                git_diff::Blob::TooLarge => {
                    *too_large = true;
                    Ok(None)
                }
                git_diff::Blob::Missing => Ok(None),
            }
        }
    }
}

/// Added and removed line counts between two texts.
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

    #[test]
    fn turn_labels_stay_apart_for_turns_a_moment_parted() {
        // UUIDv7 ids created within the same timestamp step share a prefix.
        let first = "0192f0c1-2345-7abc-8def-0123456789ab";
        let second = "0192f0c1-9876-7abc-8def-fedcba987654";
        assert_eq!(&first[..8], &second[..8]);
        assert_ne!(short_turn(first), short_turn(second));
        assert_eq!(short_turn("turn_test"), "test");
    }
}
