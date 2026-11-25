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

#[cfg(test)]
thread_local! {
    /// Test-only: make every durability flush in a restore fail.
    static FAIL_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

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

/// One path the checkpoint puts back.
#[derive(Debug)]
struct RestoreOp {
    /// Path relative to the repository root, as the journal records it.
    path: String,
    absolute: PathBuf,
    change: RestoreChange,
    /// The leaf was a symlink when the restore was prepared.
    leaf_symlink: bool,
}

/// What one path becomes.
#[derive(Debug)]
enum RestoreChange {
    /// Write these bytes and set the executable bit.
    File { bytes: Vec<u8>, executable: bool },
    /// Replace the entry with this symlink.
    Symlink { target: PathBuf },
    /// Remove the entry: it did not exist at the checkpoint.
    Remove,
}

/// The bytes one path had before the restore, kept durably for rollback.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Backup {
    /// The path did not exist.
    Absent,
    /// The path held these bytes with this executable bit.
    File { bytes: Vec<u8>, executable: bool },
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

/// A restore journal read back at startup, before anything is touched.
#[derive(Debug)]
pub(crate) struct PendingRestore {
    record: RestoreRecord,
}

impl PendingRestore {
    /// The thread whose rewind the journal belongs to.
    pub(crate) fn thread_id(&self) -> &ThreadId {
        &self.record.thread_id
    }

    /// The worktree the journal recorded, canonicalised when it was written.
    pub(crate) fn workspace(&self) -> &Path {
        Path::new(&self.record.workspace)
    }
}

/// The on-disk restore journal.
#[derive(Debug, Serialize, Deserialize)]
struct RestoreRecord {
    restore_id: String,
    thread_id: ThreadId,
    /// The thread's worktree, used to prove it is still the same checkout.
    workspace: String,
    /// The repository root the entry paths are relative to.
    ///
    /// A journal written before this field existed used the workspace as the
    /// base, which is what a missing value falls back to.
    #[serde(default)]
    root: String,
    entries: Vec<RestoreRecordEntry>,
}

impl RestoreRecord {
    /// The folder the journal's paths are relative to.
    fn base(&self) -> &Path {
        if self.root.is_empty() {
            Path::new(&self.workspace)
        } else {
            Path::new(&self.root)
        }
    }
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
    File {
        blob: String,
        #[serde(default)]
        executable: bool,
    },
    Symlink {
        target: String,
    },
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
    // Git paths are relative to the repository root, which is not the same
    // folder as a thread created in a subdirectory. Resolve them there and
    // refuse anything that would fall outside the thread's own worktree.
    let repo = crate::worktrees::git::toplevel(&root)
        .ok_or_else(|| format!("{} is not inside a git worktree", root.display()))?;
    let Some(first) = turns.first() else {
        return Err("there is no turn to restore files for".into());
    };
    let checkpoint = store.checkpoint(thread_id, first).ok_or_else(|| {
        format!(
            "{} has no complete file checkpoint, so its files cannot be restored",
            first.as_str()
        )
    })?;
    let mut targets: BTreeMap<String, Option<crate::worktrees::git::CheckpointValue>> =
        BTreeMap::new();
    // One checkpoint decides the whole restore: the pre-turn state of the
    // first removed turn. A later removed turn is after that message, so its
    // checkpoint can only describe work the person asked to undo.
    for change in crate::worktrees::git::checkpoint_changes(&repo, &checkpoint)? {
        targets.insert(change.path, change.value);
    }
    // The turn's per-path captures are the pre-turn bytes for paths the git
    // checkpoint does not contain: ignored files, and anything git did not
    // stage. A capture never overrides a path the checkpoint has (the tree is
    // the earlier, whole-worktree evidence), but captured bytes beat the
    // checkpoint's absence, so a file that existed before the turn is put back
    // instead of deleted.
    for entry in store.entries(thread_id, first) {
        let relative = normalize_relative(&entry.path)
            .map_err(|error| format!("{} cannot be restored: {error}", entry.path))?;
        let absolute = root.join(&relative);
        let repo_relative = absolute.strip_prefix(&repo).map_err(|_| {
            format!(
                "{} is outside the worktree, so it cannot be restored",
                entry.path
            )
        })?;
        let path = path_string(repo_relative)?;
        if matches!(targets.get(&path), Some(Some(_))) {
            // The checkpoint tree has the path: it is the earlier, whole-tree
            // evidence and wins over a later per-path capture.
            continue;
        }
        if entry.existed {
            let blob = entry.blob.as_deref().ok_or_else(|| {
                if entry.skipped {
                    format!(
                        "{} was too large to capture before this message, so it cannot be restored",
                        entry.path
                    )
                } else {
                    format!(
                        "{} has no captured contents, so it cannot be restored",
                        entry.path
                    )
                }
            })?;
            let bytes = store.read_blob(blob).ok_or_else(|| {
                format!(
                    "the captured contents of {} are missing, so it cannot be restored",
                    entry.path
                )
            })?;
            let executable = current_is_executable(&absolute);
            targets.insert(
                path,
                Some(crate::worktrees::git::CheckpointValue::File { bytes, executable }),
            );
        } else if !targets.contains_key(&path) && absolute.symlink_metadata().is_ok() {
            // Captured as absent before the turn and present now: the turn
            // created it.
            targets.insert(path, None);
        }
    }

    let mut ops = Vec::with_capacity(targets.len());
    for (path, value) in targets {
        let relative = normalize_relative(&path)
            .map_err(|error| format!("{path} cannot be restored: {error}"))?;
        let absolute = repo.join(&relative);
        let inside = absolute.strip_prefix(&root).map_err(|_| {
            format!("{path} is outside the thread's worktree, so it cannot be restored")
        })?;
        if inside.as_os_str().is_empty() {
            return Err(format!("{path} names the worktree itself"));
        }
        preflight_target(&root, inside, &path)?;
        let leaf_symlink = absolute
            .symlink_metadata()
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false);
        let change = match value {
            Some(crate::worktrees::git::CheckpointValue::File { bytes, executable }) => {
                RestoreChange::File { bytes, executable }
            }
            Some(crate::worktrees::git::CheckpointValue::Symlink { target }) => {
                RestoreChange::Symlink { target }
            }
            None => RestoreChange::Remove,
        };
        ops.push(RestoreOp {
            path,
            absolute,
            change,
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
            Ok(meta) if meta.is_file() => Backup::File {
                bytes: fs::read(&op.absolute).map_err(|error| {
                    format!("could not read {} before restoring it: {error}", op.path)
                })?,
                executable: is_executable(&meta),
            },
            Ok(_) => {
                return Err(format!(
                    "{} is not a file or a symlink, so it cannot be restored",
                    op.path
                ));
            }
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
    write_record(store, &record_path, thread_id, &root, &repo, &ops, &backups)?;
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
        if let Err(error) = apply_op(op) {
            let rollback_error = rollback(&prepared.backups[..=index]).err();
            return match rollback_error {
                // The record is this restore's only way back; keep it until
                // the rollback itself is on disk, so the next startup can
                // finish the job.
                Some(rollback_error) => Err(format!(
                    "could not restore {}: {error}; putting back the files changed before it also failed: {rollback_error}, so the restore record is kept for the next startup",
                    op.path
                )),
                None => {
                    remove_record(&prepared.record_path);
                    Err(format!("could not restore {}: {error}", op.path))
                }
            };
        }
        match op.change {
            RestoreChange::Remove => result.deleted += 1,
            RestoreChange::File { .. } | RestoreChange::Symlink { .. } => result.files += 1,
        }
    }
    Ok(result)
}

/// Applies one path, writing through nothing.
fn apply_op(op: &RestoreOp) -> Result<(), String> {
    let parent = op
        .absolute
        .parent()
        .ok_or_else(|| format!("{} has no parent folder", op.absolute.display()))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    match &op.change {
        RestoreChange::File { bytes, executable } => {
            if op.leaf_symlink || op.absolute.is_dir() {
                // A link is removed itself, never followed: a failed unlink
                // must stop the restore instead of writing through the link
                // into whatever it points at.
                fs::remove_file(&op.absolute).map_err(|error| {
                    format!(
                        "could not remove the link at {} before writing it: {error}",
                        op.path
                    )
                })?;
            }
            write_durable(&op.absolute, bytes)?;
            set_executable(&op.absolute, *executable)?;
        }
        RestoreChange::Symlink { target } => {
            if op.absolute.symlink_metadata().is_ok() {
                fs::remove_file(&op.absolute).map_err(|error| {
                    format!(
                        "could not remove {} before replacing it with a link: {error}",
                        op.path
                    )
                })?;
            }
            make_symlink(target, &op.absolute)?;
        }
        RestoreChange::Remove => match op.absolute.symlink_metadata() {
            Ok(meta) if meta.is_dir() => {
                return Err(format!(
                    "{} is now a folder, so it cannot be restored",
                    op.path
                ));
            }
            Ok(_) => fs::remove_file(&op.absolute)
                .map_err(|error| format!("could not remove {}: {error}", op.path))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!("could not inspect {}: {error}", op.path));
            }
        },
    }
    sync_dir(parent)
}

/// Removes the restore journal after the cut committed.
pub(crate) fn finish_restore(prepared: &PreparedRestore) {
    remove_record(&prepared.record_path);
}

/// Rolls the files back and removes the restore journal.
///
/// The journal is kept when a file cannot be put back, so the next startup
/// tries the rollback again instead of accepting a half-restored worktree.
pub(crate) fn abort_restore(prepared: &PreparedRestore) -> Result<(), String> {
    rollback(&prepared.backups)?;
    remove_record(&prepared.record_path);
    Ok(())
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

/// Reads a pending restore journal before anything is touched.
///
/// A journal that cannot be read is an error, never an empty one: recovery
/// must not guess that an unreadable restore was uncommitted.
pub(crate) fn read_pending_restore(
    store: &SnapshotStore,
    restore_id: &str,
) -> Result<PendingRestore, String> {
    let path = store.restores_dir().join(format!("{restore_id}.json"));
    let bytes =
        fs::read(&path).map_err(|error| format!("could not read the restore journal: {error}"))?;
    let record: RestoreRecord = serde_json::from_slice(&bytes)
        .map_err(|error| format!("the restore journal is not readable: {error}"))?;
    Ok(PendingRestore { record })
}

/// Finishes or rolls back one interrupted restore at startup.
///
/// A committed cut keeps the files it wrote; an uncommitted one is put back.
/// The journal is removed only once the rollback is on disk, so a failed one
/// is retried by the next startup.
pub(crate) fn recover_restore(
    store: &SnapshotStore,
    pending: &PendingRestore,
    committed: bool,
) -> Result<(), String> {
    if !committed {
        rollback_record(store, &pending.record)?;
    }
    let path = store
        .restores_dir()
        .join(format!("{}.json", pending.record.restore_id));
    remove_file_durable(&path)
}

/// Writes the restore journal durably before any file changes.
fn write_record(
    store: &SnapshotStore,
    path: &Path,
    thread_id: &ThreadId,
    workspace: &Path,
    root: &Path,
    ops: &[RestoreOp],
    backups: &[(PathBuf, Backup)],
) -> Result<(), String> {
    let mut entries = Vec::with_capacity(backups.len());
    for (index, (_, backup)) in backups.iter().enumerate() {
        let backup = match backup {
            Backup::Absent => BackupRecord::Absent,
            Backup::File { bytes, executable } => BackupRecord::File {
                blob: store
                    .store_blob(bytes)
                    .map_err(|error| format!("could not keep {} for rollback: {error}", ops[index].path))?,
                executable: *executable,
            },
            Backup::Symlink(target) => BackupRecord::Symlink {
                target: target.to_str().map(str::to_owned).ok_or_else(|| {
                    format!(
                        "the link at {} has a target that is not valid UTF-8, so it cannot be recorded for rollback",
                        ops[index].path
                    )
                })?,
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
        root: root.display().to_string(),
        entries,
    };
    let json =
        serde_json::to_vec_pretty(&record).map_err(|error| format!("restore journal: {error}"))?;
    let Some(parent) = path.parent() else {
        return Err(format!("{} has no parent folder", path.display()));
    };
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temporary = path.with_extension("json.tmp");
    write_durable(&temporary, &json)?;
    fs::rename(&temporary, path).map_err(|error| error.to_string())?;
    sync_dir(parent)
}

/// Puts the recorded bytes back for one journal.
fn rollback_record(store: &SnapshotStore, record: &RestoreRecord) -> Result<(), String> {
    let base = record.base();
    for entry in &record.entries {
        let relative = normalize_relative(&entry.path)
            .map_err(|error| format!("{} cannot be put back: {error}", entry.path))?;
        let absolute = base.join(relative);
        match &entry.backup {
            BackupRecord::Absent => match absolute.symlink_metadata() {
                Ok(_) => remove_file_durable(&absolute)
                    .map_err(|error| format!("could not put {} back: {error}", entry.path))?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!("could not inspect {}: {error}", entry.path));
                }
            },
            BackupRecord::File { blob, executable } => {
                let bytes = store.read_blob(blob).ok_or_else(|| {
                    format!(
                        "the bytes kept to put {} back are missing from the restore journal",
                        entry.path
                    )
                })?;
                write_parented(&absolute, &bytes)
                    .map_err(|error| format!("could not put {} back: {error}", entry.path))?;
                set_executable(&absolute, *executable)
                    .map_err(|error| format!("could not put {} back: {error}", entry.path))?;
            }
            BackupRecord::Symlink { target } => {
                if absolute.symlink_metadata().is_ok() {
                    remove_file_durable(&absolute)
                        .map_err(|error| format!("could not put {} back: {error}", entry.path))?;
                }
                make_symlink(Path::new(target), &absolute)
                    .map_err(|error| format!("could not put {} back: {error}", entry.path))?;
                if let Some(parent) = absolute.parent() {
                    sync_dir(parent)?;
                }
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
fn make_symlink(target: &Path, path: &Path) -> Result<(), String> {
    std::os::unix::fs::symlink(target, path)
        .map_err(|error| format!("could not create the link at {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn make_symlink(_target: &Path, _path: &Path) -> Result<(), String> {
    Err("this platform cannot restore symbolic links".into())
}

/// A path relative to a root as the journal carries it.
fn path_string(path: &Path) -> Result<String, String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| "a restored path is not valid UTF-8, so it cannot be recorded".to_owned())
}

/// Whether the file at `path` is executable right now.
#[cfg(unix)]
fn current_is_executable(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|meta| is_executable(&meta))
        .unwrap_or(false)
}

/// Whether the file at `path` is executable right now.
#[cfg(not(unix))]
fn current_is_executable(_path: &Path) -> bool {
    false
}

/// Whether the file is executable.
#[cfg(unix)]
fn is_executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

/// Whether the file is executable.
#[cfg(not(unix))]
fn is_executable(_meta: &fs::Metadata) -> bool {
    false
}

/// Sets a restored file's executable bit to the checkpoint's.
#[cfg(unix)]
fn set_executable(path: &Path, executable: bool) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if executable { 0o755 } else { 0o644 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| format!("could not restore the mode of {}: {error}", path.display()))
}

/// A platform without Unix permission bits keeps them as the filesystem gave
/// them.
#[cfg(not(unix))]
fn set_executable(_path: &Path, _executable: bool) -> Result<(), String> {
    Ok(())
}

/// Writes bytes and makes them durable before returning.
fn write_durable(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    file.write_all(bytes)
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    sync_file(&mut file, path)
}

/// Writes bytes through a missing parent folder.
fn write_parented(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
    write_durable(path, bytes)
}

/// Flushes a file's contents to disk.
fn sync_file(file: &mut fs::File, path: &Path) -> Result<(), String> {
    #[cfg(test)]
    if FAIL_SYNC.with(|flag| flag.replace(false)) {
        return Err(format!("could not flush {} (injected)", path.display()));
    }
    file.sync_all()
        .map_err(|error| format!("could not flush {}: {error}", path.display()))
}

/// Makes a folder's own entries durable: a file created in it, a rename, an
/// unlink.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> Result<(), String> {
    let file = fs::File::open(dir)
        .map_err(|error| format!("could not open {} to flush it: {error}", dir.display()))?;
    file.sync_all()
        .map_err(|error| format!("could not flush {}: {error}", dir.display()))
}

/// Makes a folder's own entries durable where the platform supports it.
#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> Result<(), String> {
    Ok(())
}

/// Removes a file or link and makes the removal durable.
fn remove_file_durable(path: &Path) -> Result<(), String> {
    fs::remove_file(path)
        .map_err(|error| format!("could not remove {}: {error}", path.display()))?;
    if let Some(parent) = path.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}

/// Removes a restore journal after the restore committed or rolled back.
fn remove_record(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                let _ = sync_dir(parent);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(
            %error,
            path = %path.display(),
            "could not remove a finished restore journal"
        ),
    }
}

/// Puts every recorded byte back after a failed restore.
///
/// Every path is attempted, and the first failure is returned: the caller keeps
/// the journal until the whole rollback is on disk.
fn rollback(backups: &[(PathBuf, Backup)]) -> Result<(), String> {
    let mut first_error = None;
    for (path, backup) in backups {
        let restored = match backup {
            Backup::File { bytes, executable } => {
                write_parented(path, bytes).and_then(|()| set_executable(path, *executable))
            }
            Backup::Absent => match path.symlink_metadata() {
                Ok(_) => remove_file_durable(path),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(format!("could not inspect {}: {error}", path.display())),
            },
            Backup::Symlink(target) => {
                let removed = match path.symlink_metadata() {
                    Ok(_) => remove_file_durable(path),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(error) => Err(format!("could not inspect {}: {error}", path.display())),
                };
                removed.and_then(|()| {
                    make_symlink(target, path)?;
                    if let Some(parent) = path.parent() {
                        sync_dir(parent)?;
                    }
                    Ok(())
                })
            }
        };
        if let Err(error) = restored
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
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

    // ---- real-git fixtures -------------------------------------------------

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?} failed");
    }

    fn git_output(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("utf8")
            .trim()
            .to_owned()
    }

    fn write(root: &Path, path: &str, text: &str) {
        let absolute = root.join(path);
        if let Some(parent) = absolute.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(absolute, text).unwrap();
    }

    fn read(root: &Path, path: &str) -> String {
        fs::read_to_string(root.join(path)).unwrap()
    }

    /// A temporary repository plus a snapshot store outside it.
    struct Fixture {
        dir: tempfile::TempDir,
        repo: PathBuf,
        store: SnapshotStore,
        thread: ThreadId,
    }

    impl Fixture {
        fn new(files: &[(&str, &str)]) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path().join("repo");
            fs::create_dir_all(&repo).unwrap();
            git(&repo, &["init", "-q", "-b", "main"]);
            git(&repo, &["config", "user.email", "t@example.invalid"]);
            git(&repo, &["config", "user.name", "h4rn test"]);
            for (path, text) in files {
                write(&repo, path, text);
            }
            git(&repo, &["add", "-A"]);
            git(&repo, &["commit", "-q", "-m", "fixture"]);
            let store = SnapshotStore::new(&dir.path().join("data")).unwrap();
            Self {
                dir,
                repo,
                store,
                thread: ThreadId::new("th_1"),
            }
        }

        /// Records the whole-worktree checkpoint a turn captured.
        fn checkpoint(&self, turn: &str) -> String {
            let tree =
                crate::worktrees::git::checkpoint_commit(&self.repo).expect("checkpoint tree");
            self.store
                .record_checkpoint(&self.repo, &self.thread, &TurnId::new(turn), &tree)
                .expect("record checkpoint");
            tree
        }

        fn prepare(&self, turns: &[&str]) -> Result<PreparedRestore, String> {
            let turns: Vec<TurnId> = turns.iter().map(|turn| TurnId::new(*turn)).collect();
            prepare_restore(&self.store, &self.repo, &self.thread, &turns)
        }

        fn prepare_in(&self, workspace: &Path, turns: &[&str]) -> Result<PreparedRestore, String> {
            let turns: Vec<TurnId> = turns.iter().map(|turn| TurnId::new(*turn)).collect();
            prepare_restore(&self.store, workspace, &self.thread, &turns)
        }

        fn blob_path(&self, address: &str) -> PathBuf {
            self.dir
                .path()
                .join("data")
                .join("blobs")
                .join(&address[..2])
                .join(address)
        }
    }

    fn run_restore(prepared: &PreparedRestore) -> FileRestore {
        let outcome = apply_restore(prepared).expect("apply");
        finish_restore(prepared);
        outcome
    }

    #[test]
    fn the_first_removed_turns_checkpoint_decides_a_file_that_changed_back() {
        let fixture = Fixture::new(&[("a.txt", "one"), ("b.txt", "b0")]);
        fixture.checkpoint("turn_1");
        write(&fixture.repo, "a.txt", "two");
        write(&fixture.repo, "b.txt", "b2");
        fixture.checkpoint("turn_2");
        // The file is back to what it held before the selected message.
        write(&fixture.repo, "a.txt", "one");

        let prepared = fixture.prepare(&["turn_1", "turn_2"]).expect("prepare");
        let outcome = run_restore(&prepared);
        assert_eq!(
            read(&fixture.repo, "a.txt"),
            "one",
            "the later checkpoint never overrides the one the selected message names"
        );
        assert_eq!(
            read(&fixture.repo, "b.txt"),
            "b0",
            "a later removed turn's change is undone too"
        );
        assert_eq!(outcome.files, 1, "only the file that differs is written");
    }

    #[test]
    fn captured_bytes_beat_a_checkpoint_that_lacks_the_file() {
        let fixture = Fixture::new(&[(".gitignore", "secret.log\n"), ("README.md", "hi\n")]);
        write(&fixture.repo, "secret.log", "before");
        fixture.checkpoint("turn_1");
        // The turn's tool named the ignored file before changing it.
        fixture
            .store
            .capture(
                &fixture.repo,
                &fixture.thread,
                &TurnId::new("turn_1"),
                &["secret.log".into()],
            )
            .unwrap();
        write(&fixture.repo, "secret.log", "after");
        // The turn then tracked it, so the checkpoint has no entry for it.
        git(&fixture.repo, &["add", "-f", "secret.log"]);

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        run_restore(&prepared);
        assert_eq!(
            read(&fixture.repo, "secret.log"),
            "before",
            "captured pre-turn bytes beat the checkpoint's absence instead of deleting the file"
        );
    }

    #[test]
    fn a_checkpoint_blob_that_cannot_be_read_aborts_instead_of_deleting() {
        let fixture = Fixture::new(&[("keep.txt", "one"), ("a.txt", "before")]);
        let tree = fixture.checkpoint("turn_1");
        write(&fixture.repo, "a.txt", "after");
        // The turn deleted keep.txt, and the checkpoint's blob is gone: the
        // path is in the plan, but its bytes cannot be handed back.
        fs::remove_file(fixture.repo.join("keep.txt")).unwrap();
        let blob = git_output(&fixture.repo, &["rev-parse", &format!("{tree}:keep.txt")]);
        let objects = {
            let common = git_output(&fixture.repo, &["rev-parse", "--git-common-dir"]);
            let common = PathBuf::from(common);
            let common = if common.is_absolute() {
                common
            } else {
                fixture.repo.join(common)
            };
            common.join("objects")
        };
        fs::remove_file(objects.join(&blob[..2]).join(&blob[2..])).expect("remove the blob");

        let error = fixture.prepare(&["turn_1"]).unwrap_err();
        assert!(
            error.contains("could not read keep.txt from the checkpoint"),
            "{error}"
        );
        assert_eq!(
            read(&fixture.repo, "a.txt"),
            "after",
            "a missing checkpoint blob aborts before another file is restored"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_restore_keeps_a_rename_a_symlink_and_an_executable_bit() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new(&[("old.txt", "old\n"), ("run.sh", "#!/bin/sh\n")]);
        write(&fixture.repo, "target.txt", "target\n");
        fs::set_permissions(
            fixture.repo.join("run.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::os::unix::fs::symlink("target.txt", fixture.repo.join("link")).unwrap();
        git(&fixture.repo, &["add", "-A"]);
        git(&fixture.repo, &["commit", "-q", "-m", "modes"]);
        fixture.checkpoint("turn_1");

        // The turn renames a file, drops the executable bit and retargets a link.
        fs::rename(fixture.repo.join("old.txt"), fixture.repo.join("new.txt")).unwrap();
        fs::set_permissions(
            fixture.repo.join("run.sh"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        fs::remove_file(fixture.repo.join("link")).unwrap();
        std::os::unix::fs::symlink("elsewhere", fixture.repo.join("link")).unwrap();

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        run_restore(&prepared);
        assert_eq!(
            read(&fixture.repo, "old.txt"),
            "old\n",
            "the rename is undone"
        );
        assert!(
            !fixture.repo.join("new.txt").exists(),
            "the rename's new path is removed"
        );
        assert_eq!(
            fs::read_link(fixture.repo.join("link")).unwrap(),
            PathBuf::from("target.txt"),
            "the symlink's target is back"
        );
        assert_ne!(
            fs::metadata(fixture.repo.join("run.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0,
            "the executable bit is back"
        );
    }

    #[test]
    fn a_tracked_only_checkpoint_is_not_complete() {
        let fixture = Fixture::new(&[("a.txt", "one")]);
        let manifest_dir = fixture.dir.path().join("data/snapshots/th_1/turn_1");
        fs::create_dir_all(&manifest_dir).unwrap();
        // A manifest written before checkpoints covered the whole worktree:
        // a tree with no completeness flag.
        let manifest = serde_json::json!({
            "thread_id": "th_1",
            "turn_id": "turn_1",
            "workspace": fixture.repo.display().to_string(),
            "captured_at": chrono::Utc::now(),
            "checkpoint": "1234567890abcdef1234567890abcdef12345678",
            "files": {}
        });
        fs::write(
            manifest_dir.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();

        assert!(
            fixture
                .store
                .checkpoint(&fixture.thread, &TurnId::new("turn_1"))
                .is_none(),
            "a tracked-only checkpoint is not complete"
        );
        let error = fixture.prepare(&["turn_1"]).unwrap_err();
        assert!(error.contains("no complete file checkpoint"), "{error}");
    }

    #[test]
    fn a_file_created_after_the_checkpoint_is_deleted() {
        let fixture = Fixture::new(&[("README.md", "hi\n")]);
        fixture.checkpoint("turn_1");
        write(&fixture.repo, "new.txt", "created by the turn");
        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let outcome = run_restore(&prepared);
        assert_eq!(outcome.deleted, 1);
        assert!(!fixture.repo.join("new.txt").exists());
    }

    #[test]
    fn a_file_too_large_to_capture_refuses_when_the_checkpoint_lacks_it() {
        let fixture = Fixture::new(&[(".gitignore", "big.bin\n"), ("README.md", "hi\n")]);
        write(&fixture.repo, "big.bin", "x");
        let tree = fixture.checkpoint("turn_1");
        // The tool named the ignored file, but the capture was over its cap.
        let manifest_dir = fixture.dir.path().join("data/snapshots/th_1/turn_1");
        let manifest = serde_json::json!({
            "thread_id": "th_1",
            "turn_id": "turn_1",
            "workspace": fixture.repo.display().to_string(),
            "captured_at": chrono::Utc::now(),
            "checkpoint": tree,
            "checkpoint_complete": true,
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
        fs::write(
            manifest_dir.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        write(&fixture.repo, "big.bin", "changed");

        let error = fixture.prepare(&["turn_1"]).unwrap_err();
        assert!(error.contains("too large"), "{error}");
        assert_eq!(read(&fixture.repo, "big.bin"), "changed");
    }

    #[test]
    fn a_missing_captured_blob_refuses_before_any_file_changes() {
        let fixture = Fixture::new(&[(".gitignore", "keep.txt\n"), ("README.md", "hi\n")]);
        write(&fixture.repo, "keep.txt", "current");
        let tree = fixture.checkpoint("turn_1");
        let manifest_dir = fixture.dir.path().join("data/snapshots/th_1/turn_1");
        let missing = "0".repeat(64);
        let manifest = serde_json::json!({
            "thread_id": "th_1",
            "turn_id": "turn_1",
            "workspace": fixture.repo.display().to_string(),
            "captured_at": chrono::Utc::now(),
            "checkpoint": tree,
            "checkpoint_complete": true,
            "files": {
                "keep.txt": {
                    "path": "keep.txt",
                    "existed": true,
                    "blob": missing,
                    "etag": missing,
                    "size_bytes": 7,
                    "skipped": false
                }
            }
        });
        fs::write(
            manifest_dir.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();

        let error = fixture.prepare(&["turn_1"]).unwrap_err();
        assert!(error.contains("missing"), "{error}");
        assert_eq!(read(&fixture.repo, "keep.txt"), "current");
    }

    #[cfg(unix)]
    #[test]
    fn a_new_symlink_is_removed_itself_and_its_target_is_never_touched() {
        let fixture = Fixture::new(&[("notes.txt", "keep me\n")]);
        fixture.checkpoint("turn_1");
        std::os::unix::fs::symlink("notes.txt", fixture.repo.join("scratch.txt")).unwrap();

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let outcome = run_restore(&prepared);
        assert_eq!(outcome.deleted, 1);
        assert!(
            fixture.repo.join("scratch.txt").symlink_metadata().is_err(),
            "the link itself was removed"
        );
        assert_eq!(
            read(&fixture.repo, "notes.txt"),
            "keep me\n",
            "the link target was never followed"
        );
    }

    #[test]
    fn a_file_that_became_a_parent_folder_refuses_before_other_files_change() {
        let fixture = Fixture::new(&[("a.txt", "old a"), ("z/child.txt", "child")]);
        fixture.checkpoint("turn_1");
        // The turn replaces the folder with a file and changes a.txt.
        fs::remove_dir_all(fixture.repo.join("z")).unwrap();
        write(&fixture.repo, "z", "now a file");
        write(&fixture.repo, "a.txt", "new a");

        let error = fixture.prepare(&["turn_1"]).unwrap_err();
        assert!(error.contains("is now a file"), "{error}");
        assert_eq!(read(&fixture.repo, "a.txt"), "new a");
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_symlink_unlink_stops_the_restore_and_never_writes_through_it() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new(&[("link.txt", "was a file\n")]);
        fixture.checkpoint("turn_1");
        write(&fixture.repo, "target.txt", "keep me\n");
        fs::remove_file(fixture.repo.join("link.txt")).unwrap();
        std::os::unix::fs::symlink("target.txt", fixture.repo.join("link.txt")).unwrap();
        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");

        // The link's folder refuses the unlink; writing through the link would
        // still reach its target.
        fs::set_permissions(&fixture.repo, fs::Permissions::from_mode(0o555)).unwrap();
        let result = apply_restore(&prepared);
        fs::set_permissions(&fixture.repo, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(result.is_err(), "a failed unlink stops the restore");
        assert_eq!(
            read(&fixture.repo, "target.txt"),
            "keep me\n",
            "the restore never wrote through the link"
        );
        assert_eq!(
            fs::read_link(fixture.repo.join("link.txt")).unwrap(),
            PathBuf::from("target.txt")
        );
    }

    #[test]
    fn a_subdirectory_workspace_resolves_paths_from_the_repository_root() {
        let fixture = Fixture::new(&[("sub/a.txt", "one"), ("top.txt", "top")]);
        fixture.checkpoint("turn_1");
        write(&fixture.repo, "sub/a.txt", "two");
        let workspace = fixture.repo.join("sub");

        let prepared = fixture
            .prepare_in(&workspace, &["turn_1"])
            .expect("prepare");
        run_restore(&prepared);
        assert_eq!(
            read(&fixture.repo, "sub/a.txt"),
            "one",
            "the root-relative path is not joined to the subdirectory twice"
        );
        assert!(
            !fixture.repo.join("sub/sub").exists(),
            "nothing is written under a doubled folder"
        );
    }

    #[test]
    fn a_change_outside_a_subdirectory_workspace_refuses_the_restore() {
        let fixture = Fixture::new(&[("sub/a.txt", "one"), ("top.txt", "top")]);
        fixture.checkpoint("turn_1");
        write(&fixture.repo, "top.txt", "changed");
        write(&fixture.repo, "sub/a.txt", "two");
        let workspace = fixture.repo.join("sub");

        let error = fixture.prepare_in(&workspace, &["turn_1"]).unwrap_err();
        assert!(error.contains("outside the thread's worktree"), "{error}");
        assert_eq!(read(&fixture.repo, "top.txt"), "changed");
        assert_eq!(read(&fixture.repo, "sub/a.txt"), "two");
    }

    #[test]
    fn a_rollback_that_cannot_finish_keeps_its_journal() {
        let fixture = Fixture::new(&[("a.txt", "before")]);
        fixture.checkpoint("turn_1");
        write(&fixture.repo, "a.txt", "after");
        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let restore_id = prepared.restore_id().to_owned();
        // Break the bytes the rollback would put back.
        let record: serde_json::Value =
            serde_json::from_slice(&fs::read(&prepared.record_path).unwrap()).unwrap();
        for entry in record["entries"].as_array().unwrap() {
            if let Some(blob) = entry["backup"]["blob"].as_str() {
                fs::remove_file(fixture.blob_path(blob)).unwrap();
            }
        }
        apply_restore(&prepared).expect("apply");

        let pending = read_pending_restore(&fixture.store, &restore_id).expect("read");
        let error = recover_restore(&fixture.store, &pending, false).unwrap_err();
        assert!(error.contains("missing"), "{error}");
        assert_eq!(
            pending_restore_ids(&fixture.store),
            vec![restore_id],
            "the journal is kept until its rollback is on disk"
        );
    }

    #[test]
    fn a_restore_does_not_commit_a_file_that_was_not_flushed() {
        let fixture = Fixture::new(&[("a.txt", "before")]);
        fixture.checkpoint("turn_1");
        write(&fixture.repo, "a.txt", "after");
        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");

        FAIL_SYNC.with(|flag| flag.set(true));
        let result = apply_restore(&prepared);
        FAIL_SYNC.with(|flag| flag.set(false));

        assert!(
            result.is_err(),
            "an unflushed write does not count as applied"
        );
        assert_eq!(
            read(&fixture.repo, "a.txt"),
            "after",
            "the failed write is rolled back"
        );
    }

    #[test]
    fn an_unreadable_journal_is_an_error_not_an_uncommitted_restore() {
        let fixture = Fixture::new(&[("a.txt", "before")]);
        let restores = fixture.store.restores_dir();
        fs::create_dir_all(&restores).unwrap();
        fs::write(restores.join("restore_broken.json"), b"not json").unwrap();

        let error = read_pending_restore(&fixture.store, "restore_broken").unwrap_err();
        assert!(error.contains("not readable"), "{error}");
    }

    #[test]
    fn a_crash_at_each_step_of_a_restore_is_recovered() {
        let fixture = Fixture::new(&[("a.txt", "before a"), ("b.txt", "before b")]);
        fixture.checkpoint("turn_1");
        write(&fixture.repo, "a.txt", "after a");
        write(&fixture.repo, "b.txt", "after b");

        // Crash before any write: recovery leaves the worktree as it was.
        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let id = prepared.restore_id().to_owned();
        let pending = read_pending_restore(&fixture.store, &id).expect("read");
        recover_restore(&fixture.store, &pending, false).expect("recover");
        assert_eq!(read(&fixture.repo, "a.txt"), "after a");
        assert_eq!(read(&fixture.repo, "b.txt"), "after b");
        assert!(pending_restore_ids(&fixture.store).is_empty());

        // Crash after the first path: every applied path is put back.
        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let id = prepared.restore_id().to_owned();
        apply_op(&prepared.ops[0]).expect("apply the first path");
        assert_eq!(read(&fixture.repo, "a.txt"), "before a");
        let pending = read_pending_restore(&fixture.store, &id).expect("read");
        recover_restore(&fixture.store, &pending, false).expect("recover");
        assert_eq!(read(&fixture.repo, "a.txt"), "after a");
        assert_eq!(read(&fixture.repo, "b.txt"), "after b");

        // Crash after every write but before the commit: all are put back.
        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let id = prepared.restore_id().to_owned();
        apply_restore(&prepared).expect("apply");
        let pending = read_pending_restore(&fixture.store, &id).expect("read");
        recover_restore(&fixture.store, &pending, false).expect("recover");
        assert_eq!(read(&fixture.repo, "a.txt"), "after a");
        assert_eq!(read(&fixture.repo, "b.txt"), "after b");

        // Crash after the cut committed: the files it wrote are kept.
        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let id = prepared.restore_id().to_owned();
        apply_restore(&prepared).expect("apply");
        let pending = read_pending_restore(&fixture.store, &id).expect("read");
        recover_restore(&fixture.store, &pending, true).expect("recover");
        assert_eq!(read(&fixture.repo, "a.txt"), "before a");
        assert_eq!(read(&fixture.repo, "b.txt"), "before b");
        assert!(pending_restore_ids(&fixture.store).is_empty());
    }
}
