//! Editing an earlier prompt: the history cut and the file checkpoint restore.
//!
//! The daemon's thread lifecycle lives in [`crate::threads`]; this module holds
//! the two pieces of a rewind that do not depend on a live session: replaying
//! the journal to find which prompt turns are still live, and putting the
//! workspace back to the snapshot taken before the edited message.
//!
//! A rewind is one durable decision (ADR-050): the files and the history end at
//! the same boundary or not at all. The restore writes a durable journal first,
//! applies the files, commits the `ThreadRewound` event with that journal's id,
//! and only then publishes the new in-memory state. A crash between the apply
//! and the commit is rolled back from the journal at the next startup.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock};

use h4rn_protocol::{Attachment, Event, Item, MessageRole, RewindSettings, ThreadId, TurnId};
use serde::{Deserialize, Serialize};

use crate::fs::{SnapshotStore, normalize_relative};
use crate::journal::WireEvent;

/// The machine-wide Labs switch for "Rewind files with Edit from here".
///
/// The setting lives in `<data_dir>/rewind-settings.json`. It is off by
/// default: the conversation rewind is ready, the file restore is still being
/// hardened, so turning it on is a deliberate choice in Settings → Labs.
pub struct RewindSettingsStore {
    path: PathBuf,
    value: Arc<RwLock<RewindSettings>>,
}

impl RewindSettingsStore {
    /// Opens `<data_dir>/rewind-settings.json`.
    ///
    /// An unreadable or malformed file falls back to the defaults with a
    /// warning: a bad preference must not stop the daemon from starting.
    pub fn open(data_dir: &Path) -> Result<Self, String> {
        let path = data_dir.join("rewind-settings.json");
        let value = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<RewindSettings>(&bytes).unwrap_or_else(|error| {
                tracing::warn!(
                    %error,
                    path = %path.display(),
                    "ignoring unreadable rewind settings; using defaults"
                );
                RewindSettings::default()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => RewindSettings::default(),
            Err(error) => {
                tracing::warn!(
                    %error,
                    path = %path.display(),
                    "could not read rewind settings; using defaults"
                );
                RewindSettings::default()
            }
        };
        Ok(Self {
            path,
            value: Arc::new(RwLock::new(value)),
        })
    }

    /// An in-memory store for tests and embedders without a data directory.
    pub fn in_memory() -> Self {
        Self {
            path: PathBuf::new(),
            value: Arc::new(RwLock::new(RewindSettings::default())),
        }
    }

    /// The current settings.
    pub fn get(&self) -> RewindSettings {
        *self
            .value
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Whether file restore is turned on for this machine.
    pub fn files_enabled(&self) -> bool {
        self.get().files_enabled
    }

    /// Saves the file-restore switch, returning what was saved.
    pub fn set_files_enabled(&self, enabled: bool) -> Result<RewindSettings, String> {
        let settings = RewindSettings {
            files_enabled: enabled,
        };
        if !self.path.as_os_str().is_empty() {
            let bytes = serde_json::to_vec_pretty(&settings)
                .map_err(|error| format!("could not encode rewind settings: {error}"))?;
            write_atomic(&self.path, &bytes)?;
        }
        *self
            .value
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = settings;
        Ok(settings)
    }
}

/// Writes a small settings file through a rename, so a crash never leaves a
/// half-written preference behind.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent folder", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    let suffix = std::process::id();
    let staged = path.with_file_name(format!(
        ".{}.{suffix}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("rewind-settings")
    ));
    {
        let mut file = std::fs::File::create(&staged)
            .map_err(|error| format!("could not write {}: {error}", staged.display()))?;
        file.write_all(bytes)
            .map_err(|error| format!("could not write {}: {error}", staged.display()))?;
        file.sync_all()
            .map_err(|error| format!("could not flush {}: {error}", staged.display()))?;
    }
    std::fs::rename(&staged, path).map_err(|error| {
        let _ = std::fs::remove_file(&staged);
        format!("could not save {}: {error}", path.display())
    })
}

/// One user prompt that is still part of a thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LivePrompt {
    /// The user message item the transcript and the UI name.
    pub item_id: String,
    /// The turn the message was delivered in.
    pub turn_id: TurnId,
    /// The text exactly as the person sent it.
    pub text: String,
    /// Attachments that came with it.
    pub attachments: Vec<Attachment>,
    /// True for a message read from a saved conversation, whose provider
    /// message id h4rn never chose.
    pub imported: bool,
    /// True for a turn's first user item, its cue.
    ///
    /// A steer is journaled as another user item inside the running turn, so
    /// only the cue starts the turn. Editing any later item of the same turn
    /// would cut the cue too, which is the message before the one the person
    /// chose.
    pub cue: bool,
}

/// The prompts still live in a thread's journal, oldest first.
///
/// Prompts and cuts are folded in journal order, so a rewind only removes what
/// was part of the conversation at that point: after A, B, a rewind of B, the
/// later prompt C is still live and can be edited.
pub(crate) fn live_prompts(events: &[WireEvent]) -> Vec<LivePrompt> {
    let mut prompts: Vec<LivePrompt> = Vec::new();
    for wire in events {
        match &wire.event {
            Event::ItemCompleted {
                item:
                    Item::Message {
                        item_id,
                        role: MessageRole::User,
                        text,
                        attachments,
                    },
                turn_id,
                at,
                ..
            } => {
                let cue = !prompts
                    .iter()
                    .any(|prompt| prompt.turn_id == turn_id.clone());
                prompts.push(LivePrompt {
                    item_id: item_id.clone(),
                    turn_id: turn_id.clone(),
                    text: text.clone(),
                    attachments: attachments.clone(),
                    imported: at.is_some(),
                    cue,
                });
            }
            Event::ThreadRewound { before_item_id, .. } => {
                if let Some(index) = prompts
                    .iter()
                    .position(|prompt| &prompt.item_id == before_item_id)
                {
                    prompts.truncate(index);
                }
            }
            _ => {}
        }
    }
    prompts
}

/// The turns a thread's rewinds removed, in the order they were cut.
///
/// A prompt whose turn is no longer among the live prompts was cut by a rewind
/// (a live steer shares its cue's turn, so it never adds one). A fork reads this
/// to name a removed turn as removed instead of pretending the thread never had
/// it.
pub(crate) fn removed_turn_ids(events: &[WireEvent]) -> Vec<TurnId> {
    let live: Vec<TurnId> = live_prompts(events)
        .into_iter()
        .map(|prompt| prompt.turn_id)
        .collect();
    let mut removed: Vec<TurnId> = Vec::new();
    for wire in events {
        if let Event::ItemCompleted {
            item:
                Item::Message {
                    role: MessageRole::User,
                    ..
                },
            turn_id,
            ..
        } = &wire.event
            && !live.contains(turn_id)
            && !removed.contains(turn_id)
        {
            removed.push(turn_id.clone());
        }
    }
    removed
}

/// The turn ids a rewind at `index` removes, in turn order and without repeats.
///
/// A message queued behind a running turn is journaled under its own delivered
/// turn id, so in practice one prompt names one turn; the deduplication keeps
/// an older journal with one turn for several queued messages consistent.
pub(crate) fn removed_turns(prompts: &[LivePrompt], index: usize) -> Vec<TurnId> {
    let mut turns: Vec<TurnId> = Vec::new();
    for prompt in prompts.iter().skip(index) {
        if !turns.contains(&prompt.turn_id) {
            turns.push(prompt.turn_id.clone());
        }
    }
    turns
}

/// What a file restore did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FileRestore {
    /// Files written back or created.
    pub files: usize,
    /// Files deleted because the checkpoint had none.
    pub deleted: usize,
}

/// One file the checkpoint puts back.
#[derive(Debug)]
struct RestoreOp {
    path: String,
    absolute: PathBuf,
    /// Bytes to write; `None` removes the entry.
    bytes: Option<Vec<u8>>,
    /// The leaf was a symlink when the restore was prepared.
    leaf_symlink: bool,
}

/// The bytes one path had before the restore, kept durably for rollback.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Backup {
    /// The path did not exist.
    Absent,
    /// The path held these bytes.
    File(Vec<u8>),
    /// The path was a symlink to this target.
    Symlink(PathBuf),
}

/// A restore that wrote its journal and can now apply or roll back.
#[derive(Debug)]
pub(crate) struct PreparedRestore {
    restore_id: String,
    ops: Vec<RestoreOp>,
    backups: Vec<(PathBuf, Backup)>,
    record_path: PathBuf,
}

/// The on-disk restore journal.
#[derive(Debug, Serialize, Deserialize)]
struct RestoreRecord {
    restore_id: String,
    thread_id: ThreadId,
    workspace: String,
    entries: Vec<RestoreRecordEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RestoreRecordEntry {
    path: String,
    backup: BackupRecord,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum BackupRecord {
    Absent,
    File { blob: String },
    Symlink { target: String },
}

impl PreparedRestore {
    /// The durable id this restore commits with.
    pub(crate) fn restore_id(&self) -> &str {
        &self.restore_id
    }
}

/// Writes the restore journal and prepares the file changes without applying
/// them.
///
/// Every target is resolved and every parent checked before anything is
/// written. A file the checkpoint could not capture, a target that is now a
/// folder and a symlinked parent all refuse here, leaving the worktree exactly
/// as it was.
pub(crate) fn prepare_restore(
    store: &SnapshotStore,
    workspace: &Path,
    thread_id: &ThreadId,
    turns: &[TurnId],
) -> Result<PreparedRestore, String> {
    let root = workspace.canonicalize().map_err(|error| {
        format!(
            "could not open the workspace {}: {error}",
            workspace.display()
        )
    })?;

    let mut targets: BTreeMap<String, Option<Vec<u8>>> = BTreeMap::new();
    for turn in turns {
        // A complete git checkpoint covers shell commands and every tracked
        // file. The per-path snapshots fill in untracked files a tool named.
        if let Some(commit) = store.checkpoint(thread_id, turn) {
            for (path, bytes) in crate::worktrees::git::checkpoint_changes(&root, &commit)? {
                targets.entry(path).or_insert(bytes);
            }
        }
        for entry in store.entries(thread_id, turn) {
            if entry.existed && entry.skipped {
                return Err(format!(
                    "{} was too large to capture before this message, so it cannot be restored",
                    entry.path
                ));
            }
            let bytes = if entry.existed {
                let blob = entry.blob.as_deref().ok_or_else(|| {
                    format!(
                        "{} has no captured contents, so it cannot be restored",
                        entry.path
                    )
                })?;
                Some(store.read_blob(blob).ok_or_else(|| {
                    format!(
                        "the captured contents of {} are missing, so it cannot be restored",
                        entry.path
                    )
                })?)
            } else {
                None
            };
            targets.entry(entry.path.clone()).or_insert(bytes);
        }
    }

    let mut ops = Vec::with_capacity(targets.len());
    for (path, bytes) in targets {
        let relative = normalize_relative(&path)
            .map_err(|error| format!("{path} cannot be restored: {error}"))?;
        let absolute = root.join(&relative);
        preflight_target(&root, &relative, &path)?;
        let leaf_symlink = absolute
            .symlink_metadata()
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false);
        ops.push(RestoreOp {
            path,
            absolute,
            bytes,
            leaf_symlink,
        });
    }

    let mut backups = Vec::with_capacity(ops.len());
    for op in &ops {
        let current = match op.absolute.symlink_metadata() {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = fs::read_link(&op.absolute)
                    .map_err(|error| format!("could not read the link at {}: {error}", op.path))?;
                Backup::Symlink(target)
            }
            Ok(_) => Backup::File(fs::read(&op.absolute).map_err(|error| {
                format!("could not read {} before restoring it: {error}", op.path)
            })?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Backup::Absent,
            Err(error) => {
                return Err(format!(
                    "could not inspect {} before restoring it: {error}",
                    op.path
                ));
            }
        };
        backups.push((op.absolute.clone(), current));
    }

    let restore_id = format!("restore_{}", uuid::Uuid::now_v7());
    let record_path = store.restores_dir().join(format!("{restore_id}.json"));
    write_record(store, &record_path, thread_id, &root, &ops, &backups)?;
    Ok(PreparedRestore {
        restore_id,
        ops,
        backups,
        record_path,
    })
}

/// Applies the prepared changes, rolling them back if any write fails.
pub(crate) fn apply_restore(prepared: &PreparedRestore) -> Result<FileRestore, String> {
    let mut result = FileRestore::default();
    for (index, op) in prepared.ops.iter().enumerate() {
        let applied = match &op.bytes {
            Some(bytes) => {
                let prepared_parent = op
                    .absolute
                    .parent()
                    .map(fs::create_dir_all)
                    .transpose()
                    .map_err(|error| error.to_string());
                match prepared_parent {
                    Ok(_) => {
                        if op.leaf_symlink || op.absolute.is_dir() {
                            // Never write through a link into its target.
                            let _ = fs::remove_file(&op.absolute);
                        }
                        fs::write(&op.absolute, bytes).map_err(|error| error.to_string())
                    }
                    Err(error) => Err(error),
                }
            }
            None => match op.absolute.symlink_metadata() {
                Ok(_) => fs::remove_file(&op.absolute).map_err(|error| error.to_string()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error.to_string()),
            },
        };
        if let Err(error) = applied {
            rollback(&prepared.backups[..=index]);
            let _ = fs::remove_file(&prepared.record_path);
            return Err(format!("could not restore {}: {error}", op.path));
        }
        match op.bytes {
            Some(_) => result.files += 1,
            None => result.deleted += 1,
        }
    }
    Ok(result)
}

/// Removes the restore journal after the cut committed.
pub(crate) fn finish_restore(prepared: &PreparedRestore) {
    let _ = fs::remove_file(&prepared.record_path);
}

/// Rolls the files back and removes the restore journal.
pub(crate) fn abort_restore(prepared: &PreparedRestore) {
    rollback(&prepared.backups);
    let _ = fs::remove_file(&prepared.record_path);
}

/// Every restore journal still on disk, oldest first.
pub(crate) fn pending_restore_ids(store: &SnapshotStore) -> Vec<String> {
    let Ok(entries) = fs::read_dir(store.restores_dir()) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            name.strip_suffix(".json").map(str::to_owned)
        })
        .collect();
    ids.sort();
    ids
}

/// The thread a pending restore journal belongs to.
pub(crate) fn restore_thread(store: &SnapshotStore, restore_id: &str) -> Option<ThreadId> {
    let path = store.restores_dir().join(format!("{restore_id}.json"));
    let bytes = fs::read(path).ok()?;
    let record: RestoreRecord = serde_json::from_slice(&bytes).ok()?;
    Some(record.thread_id)
}

/// Finishes or rolls back one interrupted restore at startup.
///
/// A committed cut keeps the files it wrote; an uncommitted one is put back.
/// Either way the journal is removed, so recovery runs once.
pub(crate) fn recover_restore(
    store: &SnapshotStore,
    restore_id: &str,
    committed: bool,
) -> Result<(), String> {
    let path = store.restores_dir().join(format!("{restore_id}.json"));
    let bytes = fs::read(&path).map_err(|error| error.to_string())?;
    let record: RestoreRecord =
        serde_json::from_slice(&bytes).map_err(|error| format!("restore journal: {error}"))?;
    if !committed {
        rollback_record(store, &record)?;
    }
    fs::remove_file(&path).map_err(|error| error.to_string())
}

/// Writes the restore journal durably before any file changes.
fn write_record(
    store: &SnapshotStore,
    path: &Path,
    thread_id: &ThreadId,
    workspace: &Path,
    ops: &[RestoreOp],
    backups: &[(PathBuf, Backup)],
) -> Result<(), String> {
    let mut entries = Vec::with_capacity(backups.len());
    for (index, (_, backup)) in backups.iter().enumerate() {
        let backup = match backup {
            Backup::Absent => BackupRecord::Absent,
            Backup::File(bytes) => BackupRecord::File {
                blob: store.store_blob(bytes).map_err(|error| error.to_string())?,
            },
            Backup::Symlink(target) => BackupRecord::Symlink {
                target: target.display().to_string(),
            },
        };
        entries.push(RestoreRecordEntry {
            path: ops[index].path.clone(),
            backup,
        });
    }
    let record = RestoreRecord {
        restore_id: path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned(),
        thread_id: thread_id.clone(),
        workspace: workspace.display().to_string(),
        entries,
    };
    let json =
        serde_json::to_vec_pretty(&record).map_err(|error| format!("restore journal: {error}"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let temporary = path.with_extension("json.tmp");
    let mut file = fs::File::create(&temporary).map_err(|error| error.to_string())?;
    file.write_all(&json).map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    drop(file);
    fs::rename(&temporary, path).map_err(|error| error.to_string())?;
    Ok(())
}

/// Puts the recorded bytes back for one journal.
fn rollback_record(store: &SnapshotStore, record: &RestoreRecord) -> Result<(), String> {
    for entry in &record.entries {
        let Ok(relative) = normalize_relative(&entry.path) else {
            continue;
        };
        let absolute = Path::new(&record.workspace).join(relative);
        match &entry.backup {
            BackupRecord::Absent => {
                if absolute.symlink_metadata().is_ok() {
                    let _ = fs::remove_file(&absolute);
                }
            }
            BackupRecord::File { blob } => {
                let Some(bytes) = store.read_blob(blob) else {
                    continue;
                };
                if let Some(parent) = absolute.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::remove_file(&absolute);
                let _ = fs::write(&absolute, bytes);
            }
            BackupRecord::Symlink { target } => {
                if let Some(parent) = absolute.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::remove_file(&absolute);
                make_symlink(Path::new(target), &absolute);
            }
        }
    }
    Ok(())
}

/// Refuses a target whose parents are not a plain directory chain.
///
/// `create_dir_all` would otherwise follow a symlinked parent out of the
/// worktree, or fail after earlier files had already changed.
fn preflight_target(root: &Path, relative: &Path, label: &str) -> Result<(), String> {
    let mut current = root.to_path_buf();
    let components: Vec<Component<'_>> = relative.components().collect();
    for (index, component) in components.iter().enumerate() {
        current.push(component.as_os_str());
        let is_leaf = index + 1 == components.len();
        match current.symlink_metadata() {
            Ok(meta) if is_leaf => {
                if meta.is_dir() {
                    return Err(format!("{label} is now a folder, so it cannot be restored"));
                }
                if meta.file_type().is_symlink() {
                    // The leaf link is removed itself, never followed.
                    return Ok(());
                }
            }
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!(
                    "{label} is under a symlinked folder; refusing to restore through it"
                ));
            }
            Ok(meta) if !meta.is_dir() => {
                return Err(format!(
                    "{} is now a file, so {label} cannot be restored",
                    current.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // The rest of the chain is created fresh; nothing can be a
                // symlink until it exists.
                if !is_leaf {
                    return Ok(());
                }
            }
            Err(error) => {
                return Err(format!("could not inspect {label}: {error}"));
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn make_symlink(target: &Path, path: &Path) {
    let _ = std::os::unix::fs::symlink(target, path);
}

#[cfg(not(unix))]
fn make_symlink(_target: &Path, _path: &Path) {}

/// Puts current bytes back after a failed restore, best effort.
fn rollback(backups: &[(PathBuf, Backup)]) {
    for (path, backup) in backups {
        match backup {
            Backup::File(bytes) => {
                if let Some(parent) = path.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::remove_file(path);
                let _ = fs::write(path, bytes);
            }
            Backup::Absent => {
                let _ = fs::remove_file(path);
            }
            Backup::Symlink(target) => {
                if let Some(parent) = path.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::remove_file(path);
                make_symlink(target, path);
            }
        }
    }
}

/// Whether two workspace paths are the same folder or nested in each other.
///
/// A restore would write through both, so either direction blocks it. Paths are
/// canonicalised when they exist; a path that cannot be canonicalised is
/// compared component-wise, which is conservative enough for a refusal.
pub(crate) fn paths_overlap(left: &Path, right: &Path) -> bool {
    let left = canonical_or_lexical(left);
    let right = canonical_or_lexical(right);
    left == right || left.starts_with(&right) || right.starts_with(&left)
}

fn canonical_or_lexical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        let mut normalized = PathBuf::new();
        for component in path.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    normalized.pop();
                }
                other => normalized.push(other.as_os_str()),
            }
        }
        normalized
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(seq: u64, event: Event) -> WireEvent {
        WireEvent {
            seq,
            thread_id: ThreadId::new("th_1"),
            event,
        }
    }

    fn user(seq: u64, item_id: &str, turn: &str, text: &str) -> WireEvent {
        event(
            seq,
            Event::ItemCompleted {
                thread_id: ThreadId::new("th_1"),
                turn_id: TurnId::new(turn),
                item: Item::Message {
                    item_id: item_id.into(),
                    role: MessageRole::User,
                    text: text.into(),
                    attachments: Vec::new(),
                },
                at: None,
            },
        )
    }

    fn rewound(seq: u64, before_item_id: &str) -> WireEvent {
        event(
            seq,
            Event::ThreadRewound {
                thread_id: ThreadId::new("th_1"),
                before_item_id: before_item_id.into(),
                files_restored: false,
                restore_id: None,
                at: chrono::Utc::now(),
            },
        )
    }

    #[test]
    fn a_rewind_drops_the_named_prompt_and_every_prompt_after_it() {
        let events = vec![
            user(1, "item_user_1", "turn_1", "first"),
            user(2, "item_user_2", "turn_2", "second"),
            user(3, "item_user_3", "turn_3", "third"),
            rewound(4, "item_user_2"),
        ];
        let prompts = live_prompts(&events);
        assert_eq!(
            prompts.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(),
            ["first"]
        );
        // A second rewind can only select a message that is still there.
        assert_eq!(removed_turns(&prompts, 0).len(), 1);
        assert!(removed_turns(&prompts, 1).is_empty());
    }

    #[test]
    fn a_prompt_after_a_rewind_is_live_and_editable() {
        // A, B, rewind B, then C: the later prompt is part of the conversation.
        let events = vec![
            user(1, "item_user_a", "turn_a", "A"),
            user(2, "item_user_b", "turn_b", "B"),
            rewound(3, "item_user_b"),
            user(4, "item_user_c", "turn_c", "C"),
        ];
        let prompts = live_prompts(&events);
        assert_eq!(
            prompts.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(),
            ["A", "C"]
        );
        assert_eq!(
            removed_turns(&prompts, 1)
                .iter()
                .map(TurnId::as_str)
                .collect::<Vec<_>>(),
            ["turn_c"],
            "editing C removes only C, not the prompt the earlier rewind removed"
        );
    }

    #[test]
    fn a_rewind_names_the_turns_it_removed_and_never_the_ones_still_live() {
        let events = vec![
            user(1, "item_user_a", "turn_a", "A"),
            user(2, "item_user_b", "turn_b", "B"),
            rewound(3, "item_user_b"),
            user(4, "item_user_c", "turn_c", "C"),
        ];
        assert_eq!(
            removed_turn_ids(&events)
                .iter()
                .map(TurnId::as_str)
                .collect::<Vec<_>>(),
            ["turn_b"],
            "B left with its rewind; A and the later C are still live"
        );
    }

    #[test]
    fn nested_and_shared_workspaces_overlap_but_siblings_do_not() {
        assert!(paths_overlap(
            Path::new("/home/me/code/demo"),
            Path::new("/home/me/code/demo/.worktrees/one")
        ));
        assert!(paths_overlap(
            Path::new("/home/me/code/demo/.worktrees/one"),
            Path::new("/home/me/code/demo")
        ));
        assert!(!paths_overlap(
            Path::new("/home/me/code/demo/.worktrees/one"),
            Path::new("/home/me/code/demo/.worktrees/two")
        ));
    }

    #[test]
    fn a_later_turn_restores_a_file_to_its_first_removed_observation() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("a.txt"), "before").unwrap();
        let store = SnapshotStore::new(dir.path()).unwrap();
        let thread = ThreadId::new("th_1");
        let turn_2 = TurnId::new("turn_2");
        let turn_3 = TurnId::new("turn_3");
        store
            .capture(&workspace, &thread, &turn_2, &["a.txt".into()])
            .unwrap();
        std::fs::write(workspace.join("a.txt"), "skipped middle").unwrap();
        store
            .capture(&workspace, &thread, &turn_3, &["a.txt".into()])
            .unwrap();
        std::fs::write(workspace.join("a.txt"), "after").unwrap();

        let prepared =
            prepare_restore(&store, &workspace, &thread, &[turn_2, turn_3]).expect("prepare");
        let outcome = apply_restore(&prepared).expect("restore");
        finish_restore(&prepared);
        assert_eq!(outcome.files, 1);
        assert_eq!(
            std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
            "before"
        );
    }

    #[test]
    fn a_file_created_after_the_checkpoint_is_deleted_and_a_skipped_file_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let store = SnapshotStore::new(dir.path()).unwrap();
        let thread = ThreadId::new("th_1");
        let turn = TurnId::new("turn_1");
        // Seen before it existed: the checkpoint has no file.
        store
            .capture(&workspace, &thread, &turn, &["new.txt".into()])
            .unwrap();
        std::fs::write(workspace.join("new.txt"), "created").unwrap();
        let prepared = prepare_restore(&store, &workspace, &thread, std::slice::from_ref(&turn))
            .expect("prepare");
        let outcome = apply_restore(&prepared).expect("restore");
        finish_restore(&prepared);
        assert_eq!(outcome.deleted, 1);
        assert!(!workspace.join("new.txt").exists());

        // A file too large to capture refuses the restore before writing.
        std::fs::write(workspace.join("big.bin"), "x").unwrap();
        let manifest_dir = dir.path().join("snapshots").join("th_1").join("turn_big");
        std::fs::create_dir_all(&manifest_dir).unwrap();
        let manifest = serde_json::json!({
            "thread_id": "th_1",
            "turn_id": "turn_big",
            "workspace": workspace.display().to_string(),
            "captured_at": chrono::Utc::now(),
            "files": {
                "big.bin": {
                    "path": "big.bin",
                    "existed": true,
                    "blob": null,
                    "etag": null,
                    "size_bytes": 99999999,
                    "skipped": true
                }
            }
        });
        std::fs::write(
            manifest_dir.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let refused = prepare_restore(&store, &workspace, &thread, &[TurnId::new("turn_big")]);
        assert!(refused.unwrap_err().contains("too large"));
        assert_eq!(
            std::fs::read_to_string(workspace.join("big.bin")).unwrap(),
            "x"
        );
    }

    #[test]
    fn a_missing_blob_refuses_before_any_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("keep.txt"), "current").unwrap();
        let store = SnapshotStore::new(dir.path()).unwrap();
        let thread = ThreadId::new("th_1");
        let manifest_dir = dir.path().join("snapshots").join("th_1").join("turn_1");
        std::fs::create_dir_all(&manifest_dir).unwrap();
        let manifest = serde_json::json!({
            "thread_id": "th_1",
            "turn_id": "turn_1",
            "workspace": workspace.display().to_string(),
            "captured_at": chrono::Utc::now(),
            "files": {
                "keep.txt": {
                    "path": "keep.txt",
                    "existed": true,
                    "blob": "0".repeat(64),
                    "etag": "0".repeat(64),
                    "size_bytes": 4,
                    "skipped": false
                }
            }
        });
        std::fs::write(
            manifest_dir.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let refused = prepare_restore(&store, &workspace, &thread, &[TurnId::new("turn_1")]);
        assert!(refused.unwrap_err().contains("missing"));
        assert_eq!(
            std::fs::read_to_string(workspace.join("keep.txt")).unwrap(),
            "current"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_new_symlink_is_removed_itself_and_its_target_is_never_touched() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("notes.txt"), "keep me").unwrap();
        let store = SnapshotStore::new(dir.path()).unwrap();
        let thread = ThreadId::new("th_1");
        let turn = TurnId::new("turn_1");
        store
            .capture(&workspace, &thread, &turn, &["scratch.txt".into()])
            .unwrap();
        std::os::unix::fs::symlink("notes.txt", workspace.join("scratch.txt")).unwrap();

        let prepared = prepare_restore(&store, &workspace, &thread, std::slice::from_ref(&turn))
            .expect("prepare");
        let outcome = apply_restore(&prepared).expect("restore");
        finish_restore(&prepared);
        assert_eq!(outcome.deleted, 1);
        assert!(
            workspace.join("scratch.txt").symlink_metadata().is_err(),
            "the link itself was removed"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("notes.txt")).unwrap(),
            "keep me",
            "the link target was never followed"
        );
    }

    #[test]
    fn a_file_that_became_a_parent_folder_refuses_before_other_files_change() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("a.txt"), "old a").unwrap();
        std::fs::write(workspace.join("z"), "old z").unwrap();
        let store = SnapshotStore::new(dir.path()).unwrap();
        let thread = ThreadId::new("th_1");
        let turn = TurnId::new("turn_1");
        store
            .capture(
                &workspace,
                &thread,
                &turn,
                &["a.txt".into(), "z/child.txt".into()],
            )
            .unwrap();
        std::fs::write(workspace.join("a.txt"), "new a").unwrap();

        let refused = prepare_restore(&store, &workspace, &thread, std::slice::from_ref(&turn));
        assert!(
            refused
                .err()
                .is_some_and(|message| message.contains("is now a file")),
            "the parent type is checked before a.txt is written"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
            "new a"
        );
    }

    #[test]
    fn an_interrupted_restore_is_rolled_back_at_the_next_startup() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("a.txt"), "before").unwrap();
        let store = SnapshotStore::new(dir.path()).unwrap();
        let thread = ThreadId::new("th_1");
        let turn = TurnId::new("turn_1");
        store
            .capture(&workspace, &thread, &turn, &["a.txt".into()])
            .unwrap();
        std::fs::write(workspace.join("a.txt"), "after").unwrap();

        let prepared = prepare_restore(&store, &workspace, &thread, std::slice::from_ref(&turn))
            .expect("prepare");
        let restore_id = prepared.restore_id().to_owned();
        apply_restore(&prepared).expect("apply");
        assert_eq!(
            std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
            "before"
        );
        // The daemon stops here, before the cut committed.
        drop(prepared);
        assert_eq!(pending_restore_ids(&store), vec![restore_id.clone()]);
        recover_restore(&store, &restore_id, false).expect("rollback");
        assert_eq!(
            std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
            "after",
            "an uncommitted restore is put back"
        );
        assert!(pending_restore_ids(&store).is_empty());
    }

    #[test]
    fn a_committed_restore_keeps_its_files_when_the_journal_is_finished_late() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("a.txt"), "before").unwrap();
        let store = SnapshotStore::new(dir.path()).unwrap();
        let thread = ThreadId::new("th_1");
        let turn = TurnId::new("turn_1");
        store
            .capture(&workspace, &thread, &turn, &["a.txt".into()])
            .unwrap();
        std::fs::write(workspace.join("a.txt"), "after").unwrap();

        let prepared = prepare_restore(&store, &workspace, &thread, std::slice::from_ref(&turn))
            .expect("prepare");
        let restore_id = prepared.restore_id().to_owned();
        apply_restore(&prepared).expect("apply");
        drop(prepared);
        recover_restore(&store, &restore_id, true).expect("finish");
        assert_eq!(
            std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
            "before"
        );
        assert!(pending_restore_ids(&store).is_empty());
    }
}
