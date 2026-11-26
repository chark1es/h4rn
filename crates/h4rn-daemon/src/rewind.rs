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
    /// Test-only: make the next this-many durability flushes fail.
    static FAIL_SYNC: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// Test-only: every folder `sync_dir` has flushed, in order.
    static DIR_SYNCS: std::cell::RefCell<Vec<PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
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
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FileRestore {
    /// Files written back or created.
    pub files: usize,
    /// Files deleted because the checkpoint had none.
    pub deleted: usize,
    /// Paths ignored when the checkpoint was taken that the turn touched, left
    /// exactly as the turn left them.
    pub left_alone: Vec<String>,
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
    /// Write these bytes with this executable bit (and this mode, when the
    /// captured pre-turn bits are known and the file has to be recreated).
    File {
        bytes: Vec<u8>,
        executable: bool,
        mode: Option<u32>,
    },
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
    /// The path held these bytes with these permission bits.
    File { bytes: Vec<u8>, mode: u32 },
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
    left_alone: Vec<String>,
    /// The canonical root every backed-up path sits under, so a rollback can
    /// prove a parent is still inside it before writing or removing.
    root: PathBuf,
}

/// One restore journal on disk, before its contents are read.
#[derive(Debug)]
pub(crate) struct PendingJournal {
    restore_id: String,
    thread_id: Option<ThreadId>,
    path: PathBuf,
}

impl PendingJournal {
    /// The durable id the journal's restore commits with.
    pub(crate) fn restore_id(&self) -> &str {
        &self.restore_id
    }

    /// The thread the journal belongs to, read from its name, so recovery can
    /// still name the thread when the journal itself cannot be read.
    pub(crate) fn thread_id(&self) -> Option<&ThreadId> {
        self.thread_id.as_ref()
    }
}

/// A restore journal read back at startup, before anything is touched.
#[derive(Debug)]
pub(crate) struct PendingRestore {
    record: RestoreRecord,
    path: PathBuf,
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
        /// The full permission bits to put back. Older journals recorded only
        /// the executable bit; `mode` is then absent and the bit decides.
        #[serde(default)]
        mode: Option<u32>,
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
    let checkpoint = store
        .checkpoint(thread_id, first)
        .ok_or_else(|| store.checkpoint_refusal(thread_id, first))?;
    // The checkpoint tree cannot see ignored paths, so the paths it had to
    // ignore are recorded beside it. Their absence from the tree says nothing
    // about whether they existed, and the restore never writes, recreates or
    // removes one: a path ignored then is left exactly as the turn left it.
    let ignored = store.ignored(thread_id, first);
    let index = IgnoredIndex::new(&ignored);
    // One checkpoint decides the whole restore: the pre-turn state of the
    // first removed turn. A later removed turn is after that message, so its
    // checkpoint can only describe work the person asked to undo.
    let changes = crate::worktrees::git::checkpoint_changes(&repo, &checkpoint)?;
    // Captures never decide restored bytes. They name the ignored paths the
    // turn touched, so the answer can say what was left as it is, and they
    // lend a recreated file the mode it had before the turn.
    let mut captures: Vec<(String, String, crate::fs::SnapshotEntry)> = Vec::new();
    for turn in turns {
        for entry in store.entries(thread_id, turn) {
            let relative = normalize_relative(&entry.path)
                .map_err(|error| format!("{} cannot be restored: {error}", entry.path))?;
            let absolute = root.join(&relative);
            let Ok(repo_relative) = absolute.strip_prefix(&repo) else {
                continue;
            };
            captures.push((
                path_string(repo_relative)?,
                display_path(&root, &absolute),
                entry,
            ));
        }
    }
    // A recorded ignored folder entry stands for everything beneath it as it
    // was when the checkpoint was taken. The listing cannot say which of those
    // paths existed then — `--directory` collapses the whole folder to one
    // entry — and the turn may remove the ignore rule and create its own file
    // there. So every path under a recorded folder stays out of scope, and is
    // named: leaving a file the turn created is safer than deleting one that
    // was already there.
    let covered =
        |path: &str| index.covers_exactly(path) || !index.folders_covering(path).is_empty();

    let mut targets: BTreeMap<String, Option<crate::worktrees::git::CheckpointValue>> =
        BTreeMap::new();
    let mut left_alone: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for change in changes {
        if covered(&change.path) {
            left_alone.insert(display_path(&root, &repo.join(&change.path)));
            continue;
        }
        targets.insert(change.path, change.value);
    }
    let mut capture_modes: BTreeMap<String, u32> = BTreeMap::new();
    for (path, display, entry) in &captures {
        if covered(path) {
            left_alone.insert(display.clone());
            continue;
        }
        if let Some(mode) = entry.mode {
            capture_modes.insert(path.clone(), mode);
        }
    }
    // A recorded ignored path that is gone now was removed by the turn (or
    // while it ran); it is named so the person knows it was not put back.
    for entry in &ignored {
        let relative = entry.trim_end_matches('/');
        if relative.is_empty() {
            continue;
        }
        let absolute = repo.join(relative);
        if matches!(
            absolute.symlink_metadata(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        ) {
            left_alone.insert(display_path(&root, &absolute));
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
                RestoreChange::File {
                    bytes,
                    executable,
                    mode: capture_modes.get(&path).copied(),
                }
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
                mode: mode_of(&meta),
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
    let record_path = store
        .restore_journal_path(thread_id, &restore_id)
        .map_err(|error| format!("could not name the restore journal: {error}"))?;
    write_record(store, &record_path, thread_id, &root, &repo, &ops, &backups)?;
    Ok(PreparedRestore {
        restore_id,
        ops,
        backups,
        record_path,
        left_alone: left_alone.into_iter().collect(),
        root,
    })
}

/// Applies the prepared changes, rolling them back if any write fails.
pub(crate) fn apply_restore(prepared: &PreparedRestore) -> Result<FileRestore, String> {
    let mut result = FileRestore {
        left_alone: prepared.left_alone.clone(),
        ..FileRestore::default()
    };
    for (index, op) in prepared.ops.iter().enumerate() {
        if op.already_matches() {
            // The path already holds the checkpoint's bytes and mode: nothing
            // to write, and nothing to report as restored.
            continue;
        }
        if let Err(error) = apply_op(op) {
            let rollback_error = rollback(&prepared.backups[..=index], &prepared.root).err();
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

impl RestoreOp {
    /// Whether the path already is exactly what this op would make it.
    fn already_matches(&self) -> bool {
        match &self.change {
            RestoreChange::File {
                bytes, executable, ..
            } => {
                let Ok(meta) = self.absolute.symlink_metadata() else {
                    return false;
                };
                if !meta.is_file() {
                    return false;
                }
                fs::read(&self.absolute).is_ok_and(|current| current == *bytes)
                    && is_executable(&meta) == *executable
            }
            RestoreChange::Symlink { target } => {
                self.absolute
                    .symlink_metadata()
                    .is_ok_and(|meta| meta.file_type().is_symlink())
                    && fs::read_link(&self.absolute).is_ok_and(|current| &current == target)
            }
            RestoreChange::Remove => matches!(
                self.absolute.symlink_metadata(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound
            ),
        }
    }
}

/// Applies one path, writing through nothing.
fn apply_op(op: &RestoreOp) -> Result<(), String> {
    let parent = op
        .absolute
        .parent()
        .ok_or_else(|| format!("{} has no parent folder", op.absolute.display()))?;
    create_dir_all_durable(parent)?;
    match &op.change {
        RestoreChange::File {
            bytes,
            executable,
            mode,
        } => {
            // Keep the permission bits the file has now (a 0600 file stays
            // private). A file the restore has to recreate gets the bits it
            // had before the turn when a capture recorded them, never a
            // blanket 0644.
            let existing_mode = op
                .absolute
                .symlink_metadata()
                .ok()
                .filter(std::fs::Metadata::is_file)
                .map(|meta| mode_of(&meta))
                .or(*mode);
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
            set_restored_mode(&op.absolute, existing_mode, *executable)?;
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
    rollback(&prepared.backups, &prepared.root)?;
    remove_record(&prepared.record_path);
    Ok(())
}

/// Every restore journal still on disk, oldest first.
///
/// The thread is read from the journal's name, so an unreadable journal can
/// still be reported against its thread.
pub(crate) fn pending_journals(store: &SnapshotStore) -> Vec<PendingJournal> {
    let Ok(entries) = fs::read_dir(store.restores_dir()) else {
        return Vec::new();
    };
    let mut journals: Vec<PendingJournal> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let stem = name.strip_suffix(".json")?;
            // New journals are `<restore_id>.<thread>.json`; an older or
            // foreign name has no thread, and its contents are the only source.
            let (restore_id, thread_id) = match stem.rsplit_once('.') {
                Some((restore_id, thread)) if restore_id.starts_with("restore_") => {
                    (restore_id.to_owned(), Some(ThreadId::new(thread)))
                }
                _ => (stem.to_owned(), None),
            };
            Some(PendingJournal {
                restore_id,
                thread_id,
                path: entry.path(),
            })
        })
        .collect();
    journals.sort_by(|left, right| left.path.cmp(&right.path));
    journals
}

/// The ids of every restore journal still on disk, oldest first.
#[cfg(test)]
pub(crate) fn pending_restore_ids(store: &SnapshotStore) -> Vec<String> {
    pending_journals(store)
        .into_iter()
        .map(|journal| journal.restore_id)
        .collect()
}

/// Reads a pending restore journal before anything is touched.
///
/// A journal that cannot be read is an error, never an empty one: recovery
/// must not guess that an unreadable restore was uncommitted.
pub(crate) fn read_pending_journal(journal: &PendingJournal) -> Result<PendingRestore, String> {
    let bytes = fs::read(&journal.path)
        .map_err(|error| format!("could not read the restore journal: {error}"))?;
    let record: RestoreRecord = serde_json::from_slice(&bytes)
        .map_err(|error| format!("the restore journal is not readable: {error}"))?;
    Ok(PendingRestore {
        record,
        path: journal.path.clone(),
    })
}

/// Reads one restore journal by id, for callers that already know the name.
#[cfg(test)]
pub(crate) fn read_pending_restore(
    store: &SnapshotStore,
    restore_id: &str,
) -> Result<PendingRestore, String> {
    let journal = pending_journals(store)
        .into_iter()
        .find(|journal| journal.restore_id == restore_id)
        .ok_or_else(|| format!("the restore journal {restore_id} is not on disk"))?;
    read_pending_journal(&journal)
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
        // The journal's entry paths are relative to the root it recorded, but
        // recovery writes them. That root must still be the workspace's own
        // repository root; anything else could write outside the worktree.
        if !record_root_matches(&pending.record) {
            return Err(format!(
                "the restore journal's saved root {} is not the repository root of {}, so its files were left alone",
                pending.record.base().display(),
                pending.workspace().display()
            ));
        }
        rollback_record(store, &pending.record)?;
    }
    remove_file_durable(&pending.path)
}

/// Whether a journal's saved root is still the workspace's repository root.
fn record_root_matches(record: &RestoreRecord) -> bool {
    let Some(toplevel) = crate::worktrees::git::toplevel(Path::new(&record.workspace)) else {
        return false;
    };
    crate::worktrees::git::norm(record.base()) == toplevel
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
            Backup::File { bytes, mode } => BackupRecord::File {
                blob: store
                    .store_blob(bytes)
                    .map_err(|error| format!("could not keep {} for rollback: {error}", ops[index].path))?,
                mode: Some(*mode),
                executable: mode & 0o100 != 0,
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
        // The folder the journal recorded may be a symlink by now: recovery
        // must not write or remove through it.
        preflight_recorded_target(base, &absolute, &entry.path)?;
        match &entry.backup {
            BackupRecord::Absent => match absolute.symlink_metadata() {
                Ok(_) => remove_file_durable(&absolute)
                    .map_err(|error| format!("could not put {} back: {error}", entry.path))?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!("could not inspect {}: {error}", entry.path));
                }
            },
            BackupRecord::File {
                blob,
                mode,
                executable,
            } => {
                let bytes = store.read_blob(blob).ok_or_else(|| {
                    format!(
                        "the bytes kept to put {} back are missing from the restore journal",
                        entry.path
                    )
                })?;
                // Put back the exact bits the file had, not a default mode,
                // and never write through a link that now sits at the path.
                let mode = mode.unwrap_or(if *executable { 0o755 } else { 0o644 });
                write_replacing(&absolute, &bytes, mode)
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
/// worktree, or fail after earlier files had already changed. With no link on
/// the way, the nearest folder the write or remove goes through must still
/// canonicalise inside the root.
fn preflight_target(root: &Path, relative: &Path, label: &str) -> Result<(), String> {
    // The root itself is checked too: a journal's recorded root that has since
    // become a link would otherwise canonicalise to its target, and every
    // check below would pass inside the wrong folder.
    match root.symlink_metadata() {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(format!(
                "the worktree root {} is now a symlink; refusing to restore {label} through it",
                root.display()
            ));
        }
        Ok(meta) if !meta.is_dir() => {
            return Err(format!(
                "the worktree root {} is no longer a folder, so {label} cannot be restored",
                root.display()
            ));
        }
        Ok(_) => {}
        Err(error) => {
            return Err(format!(
                "could not inspect the worktree root {} before restoring {label}: {error}",
                root.display()
            ));
        }
    }
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
                // A link (or any other non-file) at the leaf is replaced
                // itself, never followed; only its parents matter below.
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
                // The rest of the chain will be created fresh; nothing can be
                // a symlink until it exists.
                break;
            }
            Err(error) => {
                return Err(format!("could not inspect {label}: {error}"));
            }
        }
    }
    let real_root = root.canonicalize().map_err(|error| {
        format!(
            "could not open the worktree root {}: {error}",
            root.display()
        )
    })?;
    let mut existing = root.join(relative);
    existing.pop();
    loop {
        match existing.canonicalize() {
            Ok(real) => {
                return if real.starts_with(&real_root) {
                    Ok(())
                } else {
                    Err(format!(
                        "{label} is outside the worktree root; refusing to restore through it"
                    ))
                };
            }
            Err(_) if existing.pop() => {}
            Err(_) => break,
        }
    }
    Err(format!(
        "{label} has no folder inside the worktree root to be restored through"
    ))
}

/// Refuses a journal target whose path now leads outside the recorded root.
///
/// Journal paths are relative to the root saved beside them, and recovery
/// writes them long after the fact: any folder on the way may have become a
/// symlink. [`preflight_target`] walks every parent without following a link
/// and proves the nearest real folder is still inside the root.
fn preflight_recorded_target(root: &Path, absolute: &Path, label: &str) -> Result<(), String> {
    let relative = absolute.strip_prefix(root).map_err(|_| {
        format!("{label} is outside the restore journal's root, so it cannot be put back")
    })?;
    if relative.as_os_str().is_empty() {
        return Err(format!("{label} names the restore journal's root itself"));
    }
    preflight_target(root, relative, label)
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

/// A file's Unix permission bits.
#[cfg(unix)]
fn mode_of(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o7777
}

/// A platform without Unix permission bits has no mode to keep.
#[cfg(not(unix))]
fn mode_of(_meta: &fs::Metadata) -> u32 {
    0
}

/// Whether git calls the file executable: the owner's execute bit, as git
/// itself tests it. A group or other bit alone is not enough.
#[cfg(unix)]
fn is_executable(meta: &fs::Metadata) -> bool {
    mode_of(meta) & 0o100 != 0
}

/// Whether the file is executable.
#[cfg(not(unix))]
fn is_executable(_meta: &fs::Metadata) -> bool {
    false
}

/// Sets a restored file's executable bit while keeping the bits it had.
///
/// A capture can hold only the executable state; the rest of a file's mode
/// belongs to the person (a 0600 `.env` must not become world-readable), so
/// only the executable bit is toggled. A file that did not exist before gets
/// the conventional 0644 or 0755.
#[cfg(unix)]
fn set_restored_mode(path: &Path, existing: Option<u32>, executable: bool) -> Result<(), String> {
    let mode = match existing {
        // Set exec bit where the file is readable, so a 0600 file that is
        // executable becomes 0700, never 0711.
        Some(mode) if executable => mode | ((mode & 0o444) >> 2),
        Some(mode) => mode & !0o111,
        None if executable => 0o755,
        None => 0o644,
    };
    set_mode(path, mode)
}

/// A platform without Unix permission bits keeps them as the filesystem gave
/// them.
#[cfg(not(unix))]
fn set_restored_mode(
    _path: &Path,
    _existing: Option<u32>,
    _executable: bool,
) -> Result<(), String> {
    Ok(())
}

/// Sets a file's permission bits exactly.
#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| format!("could not restore the mode of {}: {error}", path.display()))
}

/// A platform without Unix permission bits has no mode to set.
#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), String> {
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

/// Writes a file without ever following a link that now sits at its path.
///
/// The bytes go to a temporary file in the same folder and are renamed into
/// place, so a symlink the restore created is replaced rather than written
/// through into its target. A folder or another non-file at the path refuses.
fn write_replacing(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent folder", path.display()))?;
    create_dir_all_durable(parent)?;
    match path.symlink_metadata() {
        Ok(meta) if meta.is_file() => {}
        Ok(meta) if meta.is_dir() => {
            return Err(format!(
                "{} is now a folder, so it cannot be put back",
                path.display()
            ));
        }
        // A link (or any other non-file) is replaced by the rename below,
        // never followed.
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!("could not inspect {}: {error}", path.display()));
        }
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let staged = parent.join(format!(
        ".{name}.{}.h4rn-rollback.tmp",
        uuid::Uuid::now_v7()
    ));
    let restored = write_durable(&staged, bytes)
        .and_then(|()| set_mode(&staged, mode))
        .and_then(|()| {
            fs::rename(&staged, path)
                .map_err(|error| format!("could not put {} back: {error}", path.display()))
        })
        .and_then(|()| sync_dir(parent));
    if restored.is_err() {
        let _ = fs::remove_file(&staged);
    }
    restored
}

/// Creates a folder chain one level at a time, flushing each new folder in its
/// parent, so a crash right after the restore cannot lose a folder the journal
/// says exists.
fn create_dir_all_durable(dir: &Path) -> Result<(), String> {
    let mut missing: Vec<PathBuf> = Vec::new();
    let mut current = dir.to_path_buf();
    loop {
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.is_dir() => break,
            Ok(_) => {
                return Err(format!(
                    "could not create {}: it is not a folder",
                    current.display()
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(current.clone());
            }
            Err(error) => {
                return Err(format!("could not inspect {}: {error}", current.display()));
            }
        }
        match current.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => current = parent.to_path_buf(),
            _ => break,
        }
    }
    for path in missing.iter().rev() {
        fs::create_dir(path)
            .map_err(|error| format!("could not create {}: {error}", path.display()))?;
        if let Some(parent) = path.parent() {
            sync_dir(parent)?;
        }
    }
    Ok(())
}

/// Flushes a file's contents to disk.
fn sync_file(file: &mut fs::File, path: &Path) -> Result<(), String> {
    #[cfg(test)]
    if FAIL_SYNC.with(|flag| {
        let remaining = flag.get();
        if remaining == 0 {
            false
        } else {
            flag.set(remaining - 1);
            true
        }
    }) {
        return Err(format!("could not flush {} (injected)", path.display()));
    }
    file.sync_all()
        .map_err(|error| format!("could not flush {}: {error}", path.display()))
}

/// Makes a folder's own entries durable: a file created in it, a rename, an
/// unlink.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> Result<(), String> {
    #[cfg(test)]
    DIR_SYNCS.with(|dirs| dirs.borrow_mut().push(dir.to_path_buf()));
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
fn rollback(backups: &[(PathBuf, Backup)], root: &Path) -> Result<(), String> {
    let mut first_error = None;
    for (path, backup) in backups {
        let label = path.display().to_string();
        // A folder on the way may have become a symlink since the journal was
        // written. Nothing is put back through one: the bytes would land
        // outside the worktree.
        let restored = match preflight_recorded_target(root, path, &label) {
            Err(error) => Err(error),
            Ok(()) => match backup {
                Backup::File { bytes, mode } => write_replacing(path, bytes, *mode),
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
            },
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

/// The checkpoint's ignored listing, indexed for fast lookups.
///
/// An exact entry is an ignored file. An entry with a trailing `/` is a folder
/// `--directory` collapsed, and covers every path beneath it. Looking a path up
/// costs one hash lookup per ancestor, never a scan of the whole listing.
#[derive(Debug, Default)]
struct IgnoredIndex {
    exact: std::collections::HashSet<String>,
    folders: std::collections::HashSet<String>,
}

impl IgnoredIndex {
    fn new(ignored: &[String]) -> Self {
        let mut index = Self::default();
        for entry in ignored {
            match entry.strip_suffix('/') {
                Some(folder) if !folder.is_empty() => {
                    index.folders.insert(folder.to_owned());
                }
                Some(_) => {}
                None => {
                    index.exact.insert(entry.clone());
                }
            }
        }
        index
    }

    /// Whether the path itself was a recorded ignored file, or the folder
    /// entry itself.
    fn covers_exactly(&self, path: &str) -> bool {
        self.exact.contains(path) || self.folders.contains(path)
    }

    /// The recorded ignored folders that contain `path`, deepest last.
    fn folders_covering(&self, path: &str) -> Vec<String> {
        let mut folders = Vec::new();
        let mut rest = path;
        while let Some((parent, _)) = rest.rsplit_once('/') {
            if self.folders.contains(parent) {
                folders.push(parent.to_owned());
            }
            rest = parent;
        }
        folders
    }
}

/// A repository-relative path as the person's workspace sees it, when it is
/// inside that workspace.
fn display_path(root: &Path, absolute: &Path) -> String {
    absolute
        .strip_prefix(root)
        .unwrap_or(absolute)
        .display()
        .to_string()
}

/// The ignored paths the removed turns touched, left exactly as they are.
///
/// This is what the dialog says before a restore, so it only uses what is
/// already recorded: the captures a tool's path named, and a recorded ignored
/// file that is gone now (the turn removed it). It writes nothing and needs no
/// checkpoint plan.
pub(crate) fn ignored_paths_touched(
    store: &SnapshotStore,
    workspace: &Path,
    thread_id: &ThreadId,
    turns: &[TurnId],
) -> Vec<String> {
    let Some(first) = turns.first() else {
        return Vec::new();
    };
    let Ok(root) = workspace.canonicalize() else {
        return Vec::new();
    };
    let Some(repo) = crate::worktrees::git::toplevel(&root) else {
        return Vec::new();
    };
    let ignored = store.ignored(thread_id, first);
    let index = IgnoredIndex::new(&ignored);
    let mut touched: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut captures: Vec<(String, String)> = Vec::new();
    for turn in turns {
        for entry in store.entries(thread_id, turn) {
            let Ok(relative) = normalize_relative(&entry.path) else {
                continue;
            };
            let absolute = root.join(&relative);
            let Ok(repo_relative) = absolute.strip_prefix(&repo) else {
                continue;
            };
            let Ok(path) = path_string(repo_relative) else {
                continue;
            };
            captures.push((path, display_path(&root, &absolute)));
        }
    }
    for (path, display) in &captures {
        // A recorded ignored folder covers everything beneath it as it stood
        // at checkpoint time, so a path there is named whether or not the
        // rules still ignore it.
        if index.covers_exactly(path) || !index.folders_covering(path).is_empty() {
            touched.insert(display.clone());
        }
    }
    for entry in &ignored {
        let relative = entry.trim_end_matches('/');
        if relative.is_empty() {
            continue;
        }
        let absolute = repo.join(relative);
        if matches!(
            absolute.symlink_metadata(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        ) {
            touched.insert(display_path(&root, &absolute));
        }
    }
    touched.into_iter().collect()
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
            let checkpoint = crate::worktrees::git::checkpoint_commit(&self.repo)
                .expect("checkpoint")
                .expect("checkpoint tree");
            self.store
                .record_checkpoint(
                    &self.repo,
                    &self.thread,
                    &TurnId::new(turn),
                    &checkpoint.tree,
                    &checkpoint.ignored,
                )
                .expect("record checkpoint");
            checkpoint.tree
        }

        /// Records a checkpoint captured from a workspace inside the repo.
        fn checkpoint_in(&self, workspace: &Path, turn: &str) {
            let checkpoint = crate::worktrees::git::checkpoint_commit(workspace)
                .expect("checkpoint")
                .expect("checkpoint tree");
            self.store
                .record_checkpoint(
                    workspace,
                    &self.thread,
                    &TurnId::new(turn),
                    &checkpoint.tree,
                    &checkpoint.ignored,
                )
                .expect("record checkpoint");
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
    fn a_force_added_ignored_file_is_left_as_it_is() {
        let fixture = Fixture::new(&[(".gitignore", "secret.log\n"), ("README.md", "hi\n")]);
        write(&fixture.repo, "secret.log", "before\n");
        fixture.checkpoint("turn_1");
        write(&fixture.repo, "secret.log", "after\n");
        // The turn forces it into the index, so git can see it now.
        git(&fixture.repo, &["add", "-f", "secret.log"]);

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let outcome = run_restore(&prepared);
        assert_eq!(
            read(&fixture.repo, "secret.log"),
            "after\n",
            "a path ignored at checkpoint time is never written, recreated or removed"
        );
        assert_eq!(outcome.files + outcome.deleted, 0);
        assert_eq!(outcome.left_alone, vec!["secret.log".to_owned()]);
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
    fn an_ignored_file_too_large_to_capture_is_left_as_it_is() {
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
            "ignored": ["big.bin"],
            "files": {
                "big.bin": {
                    "path": "big.bin",
                    "existed": true,
                    "blob": null,
                    "etag": null,
                    "size_bytes": 99999999,
                    "skipped": true,
                    "mode": 420,
                    "pre_turn": true
                }
            }
        });
        fs::write(
            manifest_dir.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        write(&fixture.repo, "big.bin", "changed");

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let outcome = run_restore(&prepared);
        assert_eq!(
            read(&fixture.repo, "big.bin"),
            "changed",
            "an ignored path is never restored from a capture"
        );
        assert_eq!(outcome.left_alone, vec!["big.bin".to_owned()]);
    }

    #[test]
    fn a_missing_capture_blob_does_not_decide_restored_bytes() {
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
            "ignored": ["keep.txt"],
            "files": {
                "keep.txt": {
                    "path": "keep.txt",
                    "existed": true,
                    "blob": missing,
                    "etag": missing,
                    "size_bytes": 7,
                    "skipped": false,
                    "mode": 420,
                    "pre_turn": true
                }
            }
        });
        fs::write(
            manifest_dir.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let outcome = run_restore(&prepared);
        assert_eq!(
            read(&fixture.repo, "keep.txt"),
            "current",
            "a capture whose blob is gone never decides what an ignored path holds"
        );
        assert_eq!(outcome.left_alone, vec!["keep.txt".to_owned()]);
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
        // The link points outside the worktree: it is not in the plan, so no
        // rollback touches the target. Only the failed unlink can keep the
        // restore from writing through the link.
        let outside = fixture.dir.path().join("outside.txt");
        fs::write(&outside, "keep me\n").unwrap();
        fs::remove_file(fixture.repo.join("link.txt")).unwrap();
        std::os::unix::fs::symlink(&outside, fixture.repo.join("link.txt")).unwrap();
        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");

        // The link's folder refuses the unlink; writing through the link would
        // still reach its target.
        fs::set_permissions(&fixture.repo, fs::Permissions::from_mode(0o555)).unwrap();
        let result = apply_restore(&prepared);
        fs::set_permissions(&fixture.repo, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(result.is_err(), "a failed unlink stops the restore");
        assert_eq!(
            fs::read_to_string(&outside).unwrap(),
            "keep me\n",
            "the restore never wrote through the link into its target"
        );
        assert_eq!(
            fs::read_link(fixture.repo.join("link.txt")).unwrap(),
            outside
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

        FAIL_SYNC.with(|flag| flag.set(1));
        let result = apply_restore(&prepared);
        FAIL_SYNC.with(|flag| flag.set(0));

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

    // ---- council round 1 fixes -------------------------------------------

    #[test]
    fn a_staged_rename_keeps_its_old_path() {
        let fixture = Fixture::new(&[("a.txt", "a\n")]);
        fixture.checkpoint("turn_1");
        // A staged rename makes `git diff` detect a rename and print only the
        // new path, which would delete the file without putting the old one
        // back.
        git(&fixture.repo, &["mv", "a.txt", "c.txt"]);

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let outcome = run_restore(&prepared);
        assert_eq!(
            read(&fixture.repo, "a.txt"),
            "a\n",
            "the staged rename's old path is back"
        );
        assert!(
            !fixture.repo.join("c.txt").exists(),
            "the staged rename's new path is removed"
        );
        assert_eq!(outcome.files, 1);
        assert_eq!(outcome.deleted, 1);
    }

    #[test]
    fn a_plain_move_staged_with_git_add_keeps_its_old_path() {
        let fixture = Fixture::new(&[("a.txt", "a\n")]);
        fixture.checkpoint("turn_1");
        fs::rename(fixture.repo.join("a.txt"), fixture.repo.join("c.txt")).unwrap();
        git(&fixture.repo, &["add", "-A"]);

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        run_restore(&prepared);
        assert_eq!(read(&fixture.repo, "a.txt"), "a\n");
        assert!(!fixture.repo.join("c.txt").exists());
    }

    #[test]
    fn emptying_gitignore_leaves_an_ignored_file_in_place() {
        let fixture = Fixture::new(&[(".gitignore", ".env\n"), ("README.md", "hi\n")]);
        write(&fixture.repo, ".env", "SECRET=1\n");
        fixture.checkpoint("turn_1");
        // The turn empties .gitignore, so .env becomes an ordinary untracked
        // path. The tree cannot see it, but it existed all along.
        write(&fixture.repo, ".gitignore", "");

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let outcome = run_restore(&prepared);
        assert_eq!(
            read(&fixture.repo, ".env"),
            "SECRET=1\n",
            "a file that was ignored at checkpoint time is left exactly as it is"
        );
        assert_eq!(outcome.deleted, 0);
        assert_eq!(outcome.left_alone, vec![".env".to_owned()]);
    }

    #[test]
    fn forcing_an_ignored_file_into_the_index_leaves_it_in_place() {
        let fixture = Fixture::new(&[(".gitignore", "secret.log\n"), ("README.md", "hi\n")]);
        write(&fixture.repo, "secret.log", "before\n");
        fixture.checkpoint("turn_1");
        // `git add -f` makes the ignored file look new beside the tree.
        git(&fixture.repo, &["add", "-f", "secret.log"]);

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let outcome = run_restore(&prepared);
        assert_eq!(read(&fixture.repo, "secret.log"), "before\n");
        assert_eq!(outcome.left_alone, vec!["secret.log".to_owned()]);
    }

    #[test]
    fn a_subdirectory_workspace_leaves_its_ignored_paths_alone() {
        let fixture = Fixture::new(&[(".gitignore", "sub/secret.log\n"), ("sub/a.txt", "one")]);
        write(&fixture.repo, "sub/secret.log", "before\n");
        // The checkpoint is captured where the thread runs, a subdirectory of
        // the repository; its ignored paths must still resolve from the root.
        fixture.checkpoint_in(&fixture.repo.join("sub"), "turn_1");
        git(&fixture.repo, &["add", "-f", "sub/secret.log"]);

        let workspace = fixture.repo.join("sub");
        let prepared = fixture
            .prepare_in(&workspace, &["turn_1"])
            .expect("prepare");
        let outcome = run_restore(&prepared);
        assert_eq!(read(&fixture.repo, "sub/secret.log"), "before\n");
        assert_eq!(
            outcome.left_alone,
            vec!["secret.log".to_owned()],
            "the note names the path as the workspace sees it"
        );
    }

    #[test]
    fn an_ignored_folder_the_turn_added_to_is_never_wiped() {
        let fixture = Fixture::new(&[(".gitignore", "build/\n"), ("README.md", "hi\n")]);
        write(&fixture.repo, "build/keep.o", "keep\n");
        fixture.checkpoint("turn_1");
        write(&fixture.repo, "build/new.o", "new\n");
        git(&fixture.repo, &["add", "-f", "build/new.o"]);

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let outcome = run_restore(&prepared);
        assert_eq!(
            read(&fixture.repo, "build/keep.o"),
            "keep\n",
            "the ignored folder's existing content is never wiped"
        );
        assert_eq!(read(&fixture.repo, "build/new.o"), "new\n");
        assert_eq!(outcome.deleted, 0);
        assert_eq!(outcome.left_alone, vec!["build/new.o".to_owned()]);
    }

    #[test]
    fn a_mid_turn_capture_never_resurrects_a_file_the_turn_created() {
        let fixture = Fixture::new(&[("README.md", "hi\n")]);
        fixture.checkpoint("turn_1");
        // A shell command creates the file; a later tool names it, so the
        // capture holds mid-turn bytes. The complete checkpoint's absence is
        // the earlier evidence: the turn created it.
        write(&fixture.repo, "new.txt", "created\n");
        fixture
            .store
            .capture(
                &fixture.repo,
                &fixture.thread,
                &TurnId::new("turn_1"),
                &["new.txt".into()],
            )
            .unwrap();

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let outcome = run_restore(&prepared);
        assert_eq!(outcome.deleted, 1);
        assert!(
            !fixture.repo.join("new.txt").exists(),
            "a mid-turn capture never brings back a file the turn created"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_cp_p_over_an_ignored_file_is_not_restored_to_mid_turn_bytes() {
        use std::time::{Duration, SystemTime};

        let fixture = Fixture::new(&[(".gitignore", "secret.log\n"), ("README.md", "hi\n")]);
        write(&fixture.repo, "secret.log", "before\n");
        fixture.checkpoint("turn_1");
        // The turn copies another file over the ignored one with `cp -p`, so
        // the ignored path keeps an older mtime while its bytes are new. A
        // later tool then names it and the capture holds mid-turn bytes.
        write(&fixture.repo, "copied.txt", "mid-turn\n");
        let stamp = SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(fixture.repo.join("copied.txt"))
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(stamp))
            .unwrap();
        fs::copy(
            fixture.repo.join("copied.txt"),
            fixture.repo.join("secret.log"),
        )
        .unwrap();
        std::fs::File::options()
            .write(true)
            .open(fixture.repo.join("secret.log"))
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(stamp))
            .unwrap();
        fixture
            .store
            .capture(
                &fixture.repo,
                &fixture.thread,
                &TurnId::new("turn_1"),
                &["secret.log".into()],
            )
            .unwrap();
        git(&fixture.repo, &["add", "-f", "secret.log"]);

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let outcome = run_restore(&prepared);
        assert_eq!(
            read(&fixture.repo, "secret.log"),
            "mid-turn\n",
            "an old mtime is never proof of pre-turn bytes; the ignored file is left alone"
        );
        assert_eq!(outcome.left_alone, vec!["secret.log".to_owned()]);
    }

    #[test]
    fn a_crlf_file_comes_back_as_its_exact_bytes() {
        let fixture = Fixture::new(&[(".gitattributes", "*.bat text eol=crlf\n")]);
        write(&fixture.repo, "script.bat", "a\r\nb\r\n");
        git(&fixture.repo, &["add", "-A"]);
        git(&fixture.repo, &["commit", "-q", "-m", "crlf"]);
        fixture.checkpoint("turn_1");
        // The turn rewrites it with bare newlines and different lines.
        write(&fixture.repo, "script.bat", "a\nb\nc\n");

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        run_restore(&prepared);
        assert_eq!(
            fs::read(fixture.repo.join("script.bat")).unwrap(),
            b"a\r\nb\r\n",
            "raw bytes go in and raw bytes come back"
        );
    }

    #[test]
    fn info_attributes_never_convert_the_checkpoint() {
        let fixture = Fixture::new(&[("README.md", "hi\n")]);
        // `.git/info/attributes` outranks `.gitattributes`, so it would
        // convert what `git add -A` stored; the plumbing builder never reads
        // it.
        write(
            &fixture.repo,
            ".git/info/attributes",
            "*.txt text eol=crlf\n",
        );
        write(&fixture.repo, "notes.txt", "one\r\ntwo\r\n");
        fixture.checkpoint("turn_1");
        write(&fixture.repo, "notes.txt", "one\ntwo\nthree\n");

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        run_restore(&prepared);
        assert_eq!(
            fs::read(fixture.repo.join("notes.txt")).unwrap(),
            b"one\r\ntwo\r\n",
            "a local attributes file never converts the stored bytes"
        );
    }

    #[test]
    fn a_global_attributes_file_never_converts_the_checkpoint() {
        run_with_global_crlf_attributes(
            "a_global_attributes_file_is_never_applied_to_the_checkpoint",
        );
    }

    #[test]
    #[ignore = "run by a_global_attributes_file_never_converts_the_checkpoint with a temporary global config"]
    fn a_global_attributes_file_is_never_applied_to_the_checkpoint() {
        let fixture = Fixture::new(&[("README.md", "hi\n")]);
        write(&fixture.repo, "notes.txt", "one\r\ntwo\r\n");
        fixture.checkpoint("turn_1");
        write(&fixture.repo, "notes.txt", "one\ntwo\nthree\n");

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        run_restore(&prepared);
        assert_eq!(
            fs::read(fixture.repo.join("notes.txt")).unwrap(),
            b"one\r\ntwo\r\n",
            "a global attributes file never converts the checkpoint's bytes"
        );
    }

    #[test]
    fn core_autocrlf_input_keeps_an_untracked_crlf_file() {
        let fixture = Fixture::new(&[("README.md", "hi\n")]);
        git(&fixture.repo, &["config", "core.autocrlf", "input"]);
        write(&fixture.repo, "notes.txt", "one\r\ntwo\r\n");
        fixture.checkpoint("turn_1");
        // The turn rewrites the untracked file with bare newlines.
        write(&fixture.repo, "notes.txt", "one\ntwo\nthree\n");

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        run_restore(&prepared);
        assert_eq!(
            fs::read(fixture.repo.join("notes.txt")).unwrap(),
            b"one\r\ntwo\r\n",
            "core.autocrlf never converts the checkpointed or restored bytes"
        );
    }

    #[test]
    fn text_auto_keeps_an_untracked_crlf_file() {
        let fixture = Fixture::new(&[
            (".gitattributes", "*.txt text=auto\n"),
            ("README.md", "hi\n"),
        ]);
        write(&fixture.repo, "notes.txt", "one\r\ntwo\r\n");
        fixture.checkpoint("turn_1");
        write(&fixture.repo, "notes.txt", "one\ntwo\nthree\n");

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        run_restore(&prepared);
        assert_eq!(
            fs::read(fixture.repo.join("notes.txt")).unwrap(),
            b"one\r\ntwo\r\n",
            "a text=auto rule does not change the restored bytes"
        );
    }

    #[test]
    fn a_turn_adding_a_crlf_rule_does_not_change_the_restored_bytes() {
        let fixture = Fixture::new(&[("build.sh", "#!/bin/sh\necho one\n")]);
        fixture.checkpoint("turn_1");
        // The turn adds an eol rule and rewrites the script with CRLF.
        write(&fixture.repo, ".gitattributes", "*.sh text eol=crlf\n");
        write(&fixture.repo, "build.sh", "#!/bin/sh\r\necho changed\r\n");

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        run_restore(&prepared);
        assert_eq!(
            fs::read(fixture.repo.join("build.sh")).unwrap(),
            b"#!/bin/sh\necho one\n",
            "the rule the turn added does not convert the pre-turn bytes"
        );
        assert!(!fixture.repo.join(".gitattributes").exists());
    }

    #[test]
    fn a_filtered_file_comes_back_as_its_exact_bytes() {
        let fixture = Fixture::new(&[("README.md", "hi\n")]);
        git(
            &fixture.repo,
            &["config", "filter.extra.clean", "sed 's/^SMUDGED //'"],
        );
        git(
            &fixture.repo,
            &["config", "filter.extra.smudge", "sed 's/^/SMUDGED /'"],
        );
        write(&fixture.repo, ".gitattributes", "*.bin filter=extra\n");
        write(&fixture.repo, "data.bin", "SMUDGED payload\n");
        git(&fixture.repo, &["add", "-A"]);
        git(&fixture.repo, &["commit", "-q", "-m", "filter"]);
        fixture.checkpoint("turn_1");
        write(&fixture.repo, "data.bin", "SMUDGED changed\n");

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        run_restore(&prepared);
        assert_eq!(
            read(&fixture.repo, "data.bin"),
            "SMUDGED payload\n",
            "the raw bytes are restored, not the clean form a filter would store"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_restore_keeps_a_private_files_mode() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new(&[("secret.env", "one\n")]);
        fs::set_permissions(
            fixture.repo.join("secret.env"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        fixture.checkpoint("turn_1");
        write(&fixture.repo, "secret.env", "two\n");

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        run_restore(&prepared);
        assert_eq!(read(&fixture.repo, "secret.env"), "one\n");
        assert_eq!(
            fs::metadata(fixture.repo.join("secret.env"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "a restored private file stays private"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_abort_keeps_a_private_files_mode() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new(&[("secret.env", "before\n")]);
        fs::set_permissions(
            fixture.repo.join("secret.env"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        fixture.checkpoint("turn_1");
        write(&fixture.repo, "secret.env", "after\n");
        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");

        FAIL_SYNC.with(|flag| flag.set(1));
        let result = apply_restore(&prepared);
        FAIL_SYNC.with(|flag| flag.set(0));

        assert!(result.is_err());
        assert_eq!(read(&fixture.repo, "secret.env"), "after\n");
        assert_eq!(
            fs::metadata(fixture.repo.join("secret.env"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "the rollback puts back the exact bits the file had"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_recreated_file_keeps_its_captured_private_mode() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new(&[("secret.env", "one\n")]);
        fs::set_permissions(
            fixture.repo.join("secret.env"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        fixture.checkpoint("turn_1");
        fixture
            .store
            .capture(
                &fixture.repo,
                &fixture.thread,
                &TurnId::new("turn_1"),
                &["secret.env".into()],
            )
            .unwrap();
        // The turn deletes the file, so the restore has to recreate it.
        fs::remove_file(fixture.repo.join("secret.env")).unwrap();

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        run_restore(&prepared);
        assert_eq!(read(&fixture.repo, "secret.env"), "one\n");
        assert_eq!(
            fs::metadata(fixture.repo.join("secret.env"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "a recreated file keeps the bits it had, never a blanket 0644"
        );
    }

    #[test]
    fn a_new_folder_chain_is_flushed_in_each_parent() {
        let fixture = Fixture::new(&[("deep/a/b/keep.txt", "keep\n")]);
        fixture.checkpoint("turn_1");
        // The turn deletes the whole folder tree, so the restore recreates it.
        fs::remove_dir_all(fixture.repo.join("deep")).unwrap();
        DIR_SYNCS.with(|dirs| dirs.borrow_mut().clear());

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        run_restore(&prepared);
        assert_eq!(read(&fixture.repo, "deep/a/b/keep.txt"), "keep\n");
        let synced = DIR_SYNCS.with(|dirs| dirs.borrow().clone());
        for folder in ["deep", "deep/a", "deep/a/b"] {
            assert!(
                synced.contains(&fixture.repo.join(folder)),
                "{folder} was created without flushing it in its parent: {synced:?}"
            );
        }
    }

    #[test]
    fn an_unchanged_captured_file_is_not_counted_as_restored() {
        let fixture = Fixture::new(&[(".gitignore", "secret.log\n"), ("README.md", "hi\n")]);
        write(&fixture.repo, "secret.log", "same\n");
        fixture.checkpoint("turn_1");
        fixture
            .store
            .capture(
                &fixture.repo,
                &fixture.thread,
                &TurnId::new("turn_1"),
                &["secret.log".into()],
            )
            .unwrap();

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let outcome = run_restore(&prepared);
        assert_eq!(
            outcome.files + outcome.deleted,
            0,
            "a path that already matches is neither rewritten nor reported"
        );
        assert_eq!(read(&fixture.repo, "secret.log"), "same\n");
    }

    #[test]
    fn recovery_refuses_a_journal_whose_root_is_not_the_workspace_root() {
        let fixture = Fixture::new(&[("sub/a.txt", "one"), ("top.txt", "top")]);
        fixture.checkpoint("turn_1");
        write(&fixture.repo, "sub/a.txt", "two");
        let workspace = fixture.repo.join("sub");
        let prepared = fixture
            .prepare_in(&workspace, &["turn_1"])
            .expect("prepare");
        let restore_id = prepared.restore_id().to_owned();
        // A journal whose saved root is not the workspace's repo root would
        // write outside the worktree; recovery must refuse it.
        let mut record: serde_json::Value =
            serde_json::from_slice(&fs::read(&prepared.record_path).unwrap()).unwrap();
        record["root"] =
            serde_json::json!(fixture.dir.path().join("elsewhere").display().to_string());
        fs::write(&prepared.record_path, serde_json::to_vec(&record).unwrap()).unwrap();
        apply_restore(&prepared).expect("apply");

        let pending = read_pending_restore(&fixture.store, &restore_id).expect("read");
        let error = recover_restore(&fixture.store, &pending, false).unwrap_err();
        assert!(error.contains("saved root"), "{error}");
        assert_eq!(
            read(&fixture.repo, "sub/a.txt"),
            "one",
            "a refused rollback leaves the applied restore's files"
        );
        assert_eq!(
            pending_restore_ids(&fixture.store),
            vec![restore_id],
            "the journal is kept"
        );
    }

    // ---- council round 2: ignored files are out of scope -------------------

    #[test]
    fn an_ignored_env_the_turn_deleted_stays_deleted_and_is_named() {
        let fixture = Fixture::new(&[(".gitignore", ".env\n"), ("README.md", "hi\n")]);
        write(&fixture.repo, ".env", "SECRET=1\n");
        fixture.checkpoint("turn_1");
        fs::remove_file(fixture.repo.join(".env")).unwrap();

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let outcome = run_restore(&prepared);
        assert!(
            !fixture.repo.join(".env").exists(),
            "a restore never recreates an ignored path"
        );
        assert_eq!(outcome.files + outcome.deleted, 0);
        assert_eq!(outcome.left_alone, vec![".env".to_owned()]);
    }

    #[test]
    fn the_plan_names_an_ignored_path_a_tool_captured() {
        let fixture = Fixture::new(&[(".gitignore", ".env\n"), ("README.md", "hi\n")]);
        write(&fixture.repo, ".env", "SECRET=1\n");
        fixture.checkpoint("turn_1");
        fixture
            .store
            .capture(
                &fixture.repo,
                &fixture.thread,
                &TurnId::new("turn_1"),
                &[".env".into()],
            )
            .unwrap();
        write(&fixture.repo, ".env", "changed\n");

        assert_eq!(
            ignored_paths_touched(
                &fixture.store,
                &fixture.repo,
                &fixture.thread,
                &[TurnId::new("turn_1")]
            ),
            vec![".env".to_owned()]
        );
    }

    #[test]
    fn a_file_already_in_a_recorded_ignored_folder_survives_the_restore() {
        let fixture = Fixture::new(&[(".gitignore", "build/\n"), ("README.md", "hi\n")]);
        write(&fixture.repo, "build/old.log", "old\n");
        // `--directory` collapses the ignored folder to one entry, so the
        // listing cannot say which files were already inside it.
        fixture.checkpoint("turn_1");
        // The turn removes the rule, making the folder's old file visible.
        write(&fixture.repo, ".gitignore", "");

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let outcome = run_restore(&prepared);
        assert_eq!(
            read(&fixture.repo, "build/old.log"),
            "old\n",
            "a file that was already in the folder is never deleted"
        );
        assert_eq!(read(&fixture.repo, ".gitignore"), "build/\n");
        assert_eq!(
            outcome.files + outcome.deleted,
            1,
            "only .gitignore is put back"
        );
        assert_eq!(outcome.left_alone, vec!["build/old.log".to_owned()]);
    }

    #[test]
    fn a_file_the_turn_created_inside_a_recorded_ignored_folder_is_left_alone() {
        let fixture = Fixture::new(&[(".gitignore", "build/\n"), ("README.md", "hi\n")]);
        write(&fixture.repo, "build/old.log", "old\n");
        // `--directory` collapses the ignored folder to one entry.
        fixture.checkpoint("turn_1");
        // The turn un-ignores the folder and creates its own file in it.
        write(&fixture.repo, ".gitignore", "");
        write(
            &fixture.repo,
            "build/notes.txt",
            "the turn's own new file\n",
        );
        fixture
            .store
            .capture(
                &fixture.repo,
                &fixture.thread,
                &TurnId::new("turn_1"),
                &["build/old.log".into()],
            )
            .unwrap();
        assert_eq!(
            ignored_paths_touched(
                &fixture.store,
                &fixture.repo,
                &fixture.thread,
                &[TurnId::new("turn_1")]
            ),
            vec!["build/old.log".to_owned()],
            "the dialog names a captured path under a recorded folder"
        );

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let outcome = run_restore(&prepared);
        assert!(
            fixture.repo.join("build/notes.txt").exists(),
            "a path under a recorded folder is left exactly as the turn left it"
        );
        assert_eq!(
            read(&fixture.repo, "build/notes.txt"),
            "the turn's own new file\n"
        );
        assert_eq!(
            read(&fixture.repo, "build/old.log"),
            "old\n",
            "and so is the file that was already there"
        );
        assert_eq!(outcome.deleted, 0);
        assert_eq!(
            outcome.left_alone,
            vec!["build/notes.txt".to_owned(), "build/old.log".to_owned()],
            "both paths are named in the note"
        );
    }

    #[test]
    fn more_ignored_paths_than_the_cap_refuse_the_restore() {
        let fixture = Fixture::new(&[(".gitignore", "*.log\n"), ("README.md", "hi\n")]);
        for index in 0..=crate::worktrees::git::MAX_CHECKPOINT_IGNORED {
            fs::write(fixture.repo.join(format!("f{index:05}.log")), "x").unwrap();
        }

        let reason = crate::worktrees::git::checkpoint_commit(&fixture.repo).unwrap_err();
        assert!(
            reason.contains(&format!(
                "more than the {}",
                crate::worktrees::git::MAX_CHECKPOINT_IGNORED
            )),
            "{reason}"
        );
        fixture
            .store
            .record_checkpoint_incomplete(
                &fixture.repo,
                &fixture.thread,
                &TurnId::new("turn_1"),
                &reason,
            )
            .unwrap();
        write(&fixture.repo, "a.txt", "changed");
        let refused = fixture.prepare(&["turn_1"]).unwrap_err();
        assert!(refused.contains("no complete file checkpoint"), "{refused}");
        assert!(refused.contains("10000"), "{refused}");
        assert_eq!(read(&fixture.repo, "a.txt"), "changed");
    }

    #[test]
    fn a_path_ending_in_a_carriage_return_gets_its_own_bytes_back() {
        // Git's line reader drops a trailing `\r`, so in a `--stdin-paths`
        // batch `a.txt\r` would be hashed as `a.txt`.
        let fixture = Fixture::new(&[("a.txt", "plain before"), ("a.txt\r", "cr before")]);
        fixture.checkpoint("turn_1");
        write(&fixture.repo, "a.txt", "plain after");
        write(&fixture.repo, "a.txt\r", "cr after");

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let outcome = run_restore(&prepared);
        assert_eq!(read(&fixture.repo, "a.txt"), "plain before");
        assert_eq!(
            read(&fixture.repo, "a.txt\r"),
            "cr before",
            "the path with a carriage return gets its own bytes, not a.txt's"
        );
        assert_eq!(outcome.files, 2);
    }

    #[test]
    fn a_change_to_only_the_carriage_return_path_is_undone() {
        let fixture = Fixture::new(&[("a.txt", "plain before"), ("a.txt\r", "cr before")]);
        fixture.checkpoint("turn_1");
        write(&fixture.repo, "a.txt\r", "cr after");

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let outcome = run_restore(&prepared);
        assert_eq!(
            read(&fixture.repo, "a.txt\r"),
            "cr before",
            "a change to the carriage-return path alone is seen and undone"
        );
        assert_eq!(read(&fixture.repo, "a.txt"), "plain before");
        assert_eq!(outcome.files, 1);
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_makes_the_checkpoint_incomplete_and_is_never_deleted() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new(&[("README.md", "hi\n")]);
        let key = fixture.repo.join("private.key");
        fs::write(&key, "secret\n").unwrap();
        fs::set_permissions(&key, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::File::open(&key).is_ok() {
            fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
            eprintln!(
                "skipped: this user can read a mode-000 file (running as root?), \
                 so an unreadable file cannot be set up"
            );
            return;
        }

        // What a turn's start does with the capture (see threads.rs).
        let turn = TurnId::new("turn_1");
        match crate::worktrees::git::checkpoint_commit(&fixture.repo) {
            Ok(Some(checkpoint)) => fixture
                .store
                .record_checkpoint(
                    &fixture.repo,
                    &fixture.thread,
                    &turn,
                    &checkpoint.tree,
                    &checkpoint.ignored,
                )
                .unwrap(),
            Ok(None) => {}
            Err(reason) => {
                assert!(reason.contains("private.key"), "{reason}");
                fixture
                    .store
                    .record_checkpoint_incomplete(&fixture.repo, &fixture.thread, &turn, &reason)
                    .unwrap();
            }
        }
        // The turn makes the key readable and changes nothing else.
        fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();

        let prepared = fixture.prepare(&["turn_1"]);
        if let Ok(prepared) = &prepared {
            run_restore(prepared);
        }
        assert_eq!(
            read(&fixture.repo, "private.key"),
            "secret\n",
            "a file the checkpoint could not read is never deleted"
        );
        let refused = prepared.expect_err("the restore refuses");
        assert!(refused.contains("no complete file checkpoint"), "{refused}");
        assert!(refused.contains("private.key"), "{refused}");
    }

    /// Takes `turn`'s checkpoint as a turn's start does (see threads.rs): a
    /// reason the capture cannot be complete is recorded, and returned.
    ///
    /// Only the Unix permission tests use it, so it is Unix-only too.
    #[cfg(unix)]
    fn capture_turn(fixture: &Fixture, turn: &str) -> Option<String> {
        let turn = TurnId::new(turn);
        match crate::worktrees::git::checkpoint_commit(&fixture.repo) {
            Ok(Some(checkpoint)) => {
                fixture
                    .store
                    .record_checkpoint(
                        &fixture.repo,
                        &fixture.thread,
                        &turn,
                        &checkpoint.tree,
                        &checkpoint.ignored,
                    )
                    .unwrap();
                None
            }
            Ok(None) => None,
            Err(reason) => {
                fixture
                    .store
                    .record_checkpoint_incomplete(&fixture.repo, &fixture.thread, &turn, &reason)
                    .unwrap();
                Some(reason)
            }
        }
    }

    /// A folder set to mode 000 that goes back to 0755 when dropped, so the
    /// temporary directory can always be removed. `None` (with a note) when
    /// this user can list it anyway (running as root): the test cannot be set
    /// up.
    #[cfg(unix)]
    struct LockedFolder(PathBuf);

    #[cfg(unix)]
    impl LockedFolder {
        fn lock(path: &Path) -> Option<Self> {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
            let locked = Self(path.to_owned());
            if fs::read_dir(path).is_ok() {
                eprintln!(
                    "skipped: this user can list a mode-000 folder (running as root?), \
                     so an unreadable folder cannot be set up"
                );
                return None;
            }
            Some(locked)
        }
    }

    #[cfg(unix)]
    impl Drop for LockedFolder {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }

    /// Checkpoints turn_1 with `folder` locked, lets the "turn" unlock it, and
    /// checks that the restore refuses naming `named` and deletes none of
    /// `kept` (path, text).
    #[cfg(unix)]
    fn locked_folder_refuses(fixture: &Fixture, folder: &str, named: &str, kept: &[(&str, &str)]) {
        let Some(locked) = LockedFolder::lock(&fixture.repo.join(folder)) else {
            return;
        };
        if let Some(reason) = capture_turn(fixture, "turn_1") {
            assert!(reason.contains(named), "{reason}");
        }
        // The turn runs `chmod 755` on the folder and changes nothing else.
        drop(locked);

        let prepared = fixture.prepare(&["turn_1"]);
        if let Ok(prepared) = &prepared {
            run_restore(prepared);
        }
        for (path, text) in kept {
            assert!(
                fixture.repo.join(path).exists(),
                "{path} must not be deleted by the restore"
            );
            assert_eq!(read(&fixture.repo, path), *text);
        }
        let refused = prepared.expect_err("the restore refuses");
        assert!(refused.contains("no complete file checkpoint"), "{refused}");
        assert!(refused.contains(named), "{refused}");
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_folder_with_tracked_and_untracked_files_refuses_and_deletes_nothing() {
        let fixture = Fixture::new(&[("README.md", "hi\n"), ("sec/tracked.key", "tracked\n")]);
        write(&fixture.repo, "sec/untracked.key", "untracked\n");
        locked_folder_refuses(
            &fixture,
            "sec",
            "sec/",
            &[
                ("sec/tracked.key", "tracked\n"),
                ("sec/untracked.key", "untracked\n"),
            ],
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_folder_of_untracked_files_refuses_and_deletes_nothing() {
        // `ls-files` warns that it cannot open the folder and exits 0; only
        // that warning shows the listing missed these files.
        let fixture = Fixture::new(&[("README.md", "hi\n")]);
        write(&fixture.repo, "sec/untracked.key", "untracked\n");
        write(&fixture.repo, "sec/other.key", "other\n");
        locked_folder_refuses(
            &fixture,
            "sec",
            "sec/",
            &[
                ("sec/untracked.key", "untracked\n"),
                ("sec/other.key", "other\n"),
            ],
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_tracked_file_in_an_unreadable_ignored_folder_refuses_and_is_kept() {
        // `ls-files` never opens an ignored folder, so it prints no warning;
        // only the tracked file's own failed lookup shows it cannot be seen.
        let fixture = Fixture::new(&[("README.md", "hi\n"), (".gitignore", "sec/\n")]);
        write(&fixture.repo, "sec/tracked.key", "tracked\n");
        git(&fixture.repo, &["add", "-f", "sec/tracked.key"]);
        git(&fixture.repo, &["commit", "-q", "-m", "force-add"]);
        locked_folder_refuses(
            &fixture,
            "sec",
            "sec/tracked.key",
            &[("sec/tracked.key", "tracked\n")],
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_ignored_file_in_a_folder_git_cannot_list_is_never_deleted() {
        use std::os::unix::fs::PermissionsExt;
        // An ignored folder that also holds a tracked file is walked by the
        // ignored listing. At mode 0100 the tracked file can still be read,
        // but the folder cannot be listed, so `secret.key` would go
        // unrecorded as ignored.
        let fixture = Fixture::new(&[("README.md", "hi\n"), (".gitignore", "sec/\n")]);
        write(&fixture.repo, "sec/tracked.txt", "tracked\n");
        git(&fixture.repo, &["add", "-f", "sec/tracked.txt"]);
        git(&fixture.repo, &["commit", "-q", "-m", "force-add"]);
        write(&fixture.repo, "sec/secret.key", "secret\n");
        let Some(locked) = LockedFolder::lock(&fixture.repo.join("sec")) else {
            return;
        };
        fs::set_permissions(fixture.repo.join("sec"), fs::Permissions::from_mode(0o100)).unwrap();
        if let Some(reason) = capture_turn(&fixture, "turn_1") {
            assert!(reason.contains("sec/"), "{reason}");
        }
        // The turn opens the folder up and stops ignoring it.
        drop(locked);
        write(&fixture.repo, ".gitignore", "");

        let prepared = fixture.prepare(&["turn_1"]);
        if let Ok(prepared) = &prepared {
            run_restore(prepared);
        }
        assert!(
            fixture.repo.join("sec/secret.key").exists(),
            "an ignored file git could not list is never deleted"
        );
        let refused = prepared.expect_err("the restore refuses");
        assert!(refused.contains("no complete file checkpoint"), "{refused}");
        assert!(refused.contains("sec/"), "{refused}");
    }

    #[cfg(unix)]
    #[test]
    fn a_file_unreadable_only_now_is_named_as_the_worktree_now() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new(&[("README.md", "hi\n"), ("notes.txt", "before\n")]);
        fixture.checkpoint("turn_1");
        // The turn locks the file the checkpoint read without trouble.
        let notes = fixture.repo.join("notes.txt");
        fs::set_permissions(&notes, fs::Permissions::from_mode(0o000)).unwrap();
        let readable = fs::File::open(&notes).is_ok();
        let refused = fixture.prepare(&["turn_1"]).map(|_| ());
        fs::set_permissions(&notes, fs::Permissions::from_mode(0o644)).unwrap();
        if readable {
            eprintln!(
                "skipped: this user can read a mode-000 file (running as root?), \
                 so an unreadable file cannot be set up"
            );
            return;
        }
        let refused = refused.expect_err("the plan cannot see notes.txt");
        assert!(
            refused.starts_with("the worktree as it is now: "),
            "{refused}"
        );
        assert!(refused.contains("notes.txt"), "{refused}");
        assert!(!refused.contains("checkpoint"), "{refused}");
        assert_eq!(read(&fixture.repo, "notes.txt"), "before\n");
    }

    #[test]
    fn an_untracked_nested_repository_is_left_out_of_the_plan() {
        let fixture = Fixture::new(&[("README.md", "hi\n")]);
        // The nested repository is part of the checkpoint, so only the
        // gitlink skip keeps it out of the plan.
        let nested = fixture.repo.join("nested");
        fs::create_dir_all(&nested).unwrap();
        git(&nested, &["init", "-q", "-b", "main"]);
        git(&nested, &["config", "user.email", "t@example.invalid"]);
        git(&nested, &["config", "user.name", "h4rn test"]);
        write(&nested, "keep.txt", "nested\n");
        git(&nested, &["add", "-A"]);
        git(&nested, &["commit", "-q", "-m", "nested"]);
        fixture.checkpoint("turn_1");

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let outcome = run_restore(&prepared);
        assert_eq!(outcome.files + outcome.deleted, 0);
        assert_eq!(
            read(&fixture.repo, "nested/keep.txt"),
            "nested\n",
            "a nested repository the turn did not touch is left alone"
        );
    }

    /// A checkpoint with a link, and a turn that replaced the link with a file.
    ///
    /// The journal's backup for the link path is therefore a regular file,
    /// while a restore puts the link back first: a rollback that writes the
    /// backup must replace the link, never write through it.
    #[cfg(unix)]
    fn link_rollback_fixture() -> Fixture {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new(&[("target.txt", "outside bytes\n"), ("z.txt", "before z\n")]);
        symlink("target.txt", fixture.repo.join("a_link")).unwrap();
        git(&fixture.repo, &["add", "-A"]);
        git(&fixture.repo, &["commit", "-q", "-m", "link"]);
        fixture.checkpoint("turn_1");
        fs::remove_file(fixture.repo.join("a_link")).unwrap();
        write(&fixture.repo, "a_link", "turn wrote this\n");
        write(&fixture.repo, "z.txt", "after z\n");
        fixture
    }

    #[cfg(unix)]
    #[test]
    fn a_live_rollback_replaces_a_link_instead_of_writing_through_it() {
        let fixture = link_rollback_fixture();
        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        // The restore puts the link back first; the next write fails.
        apply_op(&prepared.ops[0]).expect("restore the link first");
        FAIL_SYNC.with(|flag| flag.set(1));
        let result = apply_restore(&prepared);
        FAIL_SYNC.with(|flag| flag.set(0));

        assert!(result.is_err(), "the second write fails and rolls back");
        assert_eq!(
            read(&fixture.repo, "target.txt"),
            "outside bytes\n",
            "the rollback never wrote through the link into its target"
        );
        assert_eq!(
            read(&fixture.repo, "a_link"),
            "turn wrote this\n",
            "the rollback replaced the link with the bytes the path held"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_startup_rollback_replaces_a_link_instead_of_writing_through_it() {
        let fixture = link_rollback_fixture();
        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let restore_id = prepared.restore_id().to_owned();
        apply_op(&prepared.ops[0]).expect("restore the link first");

        let pending = read_pending_restore(&fixture.store, &restore_id).expect("read");
        recover_restore(&fixture.store, &pending, false).expect("recover");

        assert_eq!(
            read(&fixture.repo, "target.txt"),
            "outside bytes\n",
            "startup recovery never wrote through the link into its target"
        );
        assert_eq!(read(&fixture.repo, "a_link"), "turn wrote this\n");
        assert!(pending_restore_ids(&fixture.store).is_empty());
    }

    /// Replaces `deep` in `repo` with a link to `outside`, as a crash and a
    /// later move could leave it before recovery runs.
    #[cfg(unix)]
    fn swap_deep_for_a_link(fixture: &Fixture, outside: &Path) {
        use std::os::unix::fs::symlink;

        fs::rename(
            fixture.repo.join("deep"),
            fixture.dir.path().join("deep-moved"),
        )
        .unwrap();
        symlink(outside, fixture.repo.join("deep")).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn startup_recovery_never_writes_through_a_symlinked_parent() {
        let fixture = Fixture::new(&[("deep/a/a.txt", "before\n")]);
        fixture.checkpoint("turn_1");
        write(&fixture.repo, "deep/a/a.txt", "after\n");
        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let restore_id = prepared.restore_id().to_owned();

        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(outside.path().join("a")).unwrap();
        swap_deep_for_a_link(&fixture, outside.path());

        let pending = read_pending_restore(&fixture.store, &restore_id).expect("read");
        let error = recover_restore(&fixture.store, &pending, false).unwrap_err();
        assert!(error.contains("symlinked folder"), "{error}");
        assert!(
            !outside.path().join("a/a.txt").exists(),
            "recovery never writes the backup outside the worktree"
        );
        assert_eq!(
            pending_restore_ids(&fixture.store),
            vec![restore_id],
            "the journal is kept"
        );
    }

    #[cfg(unix)]
    #[test]
    fn startup_recovery_never_removes_through_a_symlinked_parent() {
        let fixture = Fixture::new(&[("README.md", "hi\n")]);
        fixture.checkpoint("turn_1");
        // The turn created the file, so its backup is Absent and recovery
        // would remove it.
        write(&fixture.repo, "deep/a/new.txt", "the turn's file\n");
        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let restore_id = prepared.restore_id().to_owned();

        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(outside.path().join("a")).unwrap();
        fs::write(outside.path().join("a/new.txt"), "outside\n").unwrap();
        swap_deep_for_a_link(&fixture, outside.path());

        let pending = read_pending_restore(&fixture.store, &restore_id).expect("read");
        let error = recover_restore(&fixture.store, &pending, false).unwrap_err();
        assert!(error.contains("symlinked folder"), "{error}");
        assert_eq!(
            read(outside.path(), "a/new.txt"),
            "outside\n",
            "recovery never removes a file outside the worktree"
        );
        assert_eq!(
            pending_restore_ids(&fixture.store),
            vec![restore_id],
            "the journal is kept"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_live_rollback_never_writes_through_a_symlinked_parent() {
        let fixture = Fixture::new(&[("deep/a/a.txt", "before\n")]);
        fixture.checkpoint("turn_1");
        write(&fixture.repo, "deep/a/a.txt", "after\n");
        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let restore_id = prepared.restore_id().to_owned();

        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(outside.path().join("a")).unwrap();
        swap_deep_for_a_link(&fixture, outside.path());

        let error = abort_restore(&prepared).unwrap_err();
        assert!(error.contains("symlinked folder"), "{error}");
        assert!(
            !outside.path().join("a/a.txt").exists(),
            "a live rollback never writes outside the worktree"
        );
        assert_eq!(
            pending_restore_ids(&fixture.store),
            vec![restore_id],
            "the journal stays until the rollback can be done"
        );
    }

    #[test]
    fn a_failed_rollback_keeps_its_journal() {
        let fixture = Fixture::new(&[("a.txt", "before")]);
        fixture.checkpoint("turn_1");
        write(&fixture.repo, "a.txt", "after");
        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let restore_id = prepared.restore_id().to_owned();

        // The restored write and the rollback's write both fail to flush.
        FAIL_SYNC.with(|flag| flag.set(2));
        let result = apply_restore(&prepared);
        FAIL_SYNC.with(|flag| flag.set(0));

        let error = result.unwrap_err();
        assert!(error.contains("restore record is kept"), "{error}");
        assert_eq!(
            pending_restore_ids(&fixture.store),
            vec![restore_id],
            "the journal stays until the rollback is on disk"
        );
    }

    // ---- 10d: the plan is attribute-free, fast and git's own rules ---------

    #[test]
    fn info_attributes_never_hide_a_line_ending_change_from_the_plan() {
        let fixture = Fixture::new(&[("notes.txt", "a\nb\n")]);
        // Under this rule git's own diff normalises the turn's CRLF back to
        // the checkpoint's LF and calls the file unchanged.
        write(
            &fixture.repo,
            ".git/info/attributes",
            "*.txt text eol=crlf\n",
        );
        fixture.checkpoint("turn_1");
        write(&fixture.repo, "notes.txt", "a\r\nb\r\n");

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let outcome = run_restore(&prepared);
        assert_eq!(
            fs::read(fixture.repo.join("notes.txt")).unwrap(),
            b"a\nb\n",
            "a line-ending change under a local attribute rule is put back"
        );
        assert_eq!(outcome.files, 1);
    }

    /// Runs one ignored child test with a temporary global config whose
    /// `core.attributesFile` says `*.txt text eol=crlf`.
    ///
    /// The daemon's git calls drop an inherited `GIT_CONFIG_GLOBAL`, so the
    /// child also gets `XDG_CONFIG_HOME` (and `HOME`) pointing at the same
    /// temporary config. Nothing outside the temp dir is read or written.
    fn run_with_global_crlf_attributes(test: &str) {
        let scratch = tempfile::tempdir().unwrap();
        let config_home = scratch.path().join("xdg");
        fs::create_dir_all(config_home.join("git")).unwrap();
        let attributes = scratch.path().join("global-attributes");
        fs::write(&attributes, "*.txt text eol=crlf\n").unwrap();
        let global = config_home.join("git/config");
        fs::write(
            &global,
            format!("[core]\n\tattributesFile = {}\n", attributes.display()),
        )
        .unwrap();

        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            &format!("rewind::tests::{test}"),
            "--ignored",
            "--test-threads=1",
        ]);
        command.env("GIT_CONFIG_GLOBAL", &global);
        command.env("GIT_CONFIG_NOSYSTEM", "1");
        command.env("XDG_CONFIG_HOME", &config_home);
        command.env("HOME", scratch.path());
        let output = command.output().expect("run the child test");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn a_global_attributes_file_never_hides_a_line_ending_change_from_the_plan() {
        run_with_global_crlf_attributes(
            "a_global_attributes_file_never_hides_a_line_ending_change_child",
        );
    }

    #[test]
    #[ignore = "run by a_global_attributes_file_never_hides_a_line_ending_change_from_the_plan"]
    fn a_global_attributes_file_never_hides_a_line_ending_change_child() {
        let fixture = Fixture::new(&[("notes.txt", "a\nb\n")]);
        fixture.checkpoint("turn_1");
        write(&fixture.repo, "notes.txt", "a\r\nb\r\n");

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let outcome = run_restore(&prepared);
        assert_eq!(
            fs::read(fixture.repo.join("notes.txt")).unwrap(),
            b"a\nb\n",
            "a line-ending change under a global attribute rule is put back"
        );
        assert_eq!(outcome.files, 1);
    }

    #[test]
    fn a_checkpoint_of_thousands_of_files_is_fast() {
        const FILES: usize = 3_000;
        let fixture = Fixture::new(&[("README.md", "hi\n")]);
        for index in 0..FILES {
            write(
                &fixture.repo,
                &format!("src/d{}/f{index:05}.txt", index % 30),
                &format!("file {index}\n"),
            );
        }
        git(&fixture.repo, &["add", "-A"]);
        git(&fixture.repo, &["commit", "-q", "-m", "many"]);
        // Half the files change, so the hashing is real work and not only
        // objects git already has.
        for index in (0..FILES).step_by(2) {
            write(
                &fixture.repo,
                &format!("src/d{}/f{index:05}.txt", index % 30),
                &format!("changed {index}\n"),
            );
        }

        let started = std::time::Instant::now();
        let checkpoint = crate::worktrees::git::checkpoint_commit(&fixture.repo)
            .expect("checkpoint")
            .expect("checkpoint tree");
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "a {FILES}-file checkpoint took {elapsed:?}"
        );
        let listed = git_output(
            &fixture.repo,
            &["ls-tree", "-r", "--name-only", &checkpoint.tree],
        );
        assert_eq!(
            listed.lines().count(),
            FILES + 1,
            "every file is in the tree"
        );
        assert_eq!(
            git_output(
                &fixture.repo,
                &[
                    "cat-file",
                    "blob",
                    &format!("{}:src/d0/f00000.txt", checkpoint.tree)
                ],
            ),
            "changed 0"
        );
    }

    #[test]
    fn a_path_hash_object_cannot_read_by_line_still_gets_its_own_bytes() {
        let fixture = Fixture::new(&[("README.md", "hi\n")]);
        write(&fixture.repo, "\"quoted\".txt", "quoted bytes\n");
        #[cfg(unix)]
        write(&fixture.repo, "two\nlines.txt", "newline bytes\n");
        let checkpoint = crate::worktrees::git::checkpoint_commit(&fixture.repo)
            .expect("checkpoint")
            .expect("checkpoint tree");
        assert_eq!(
            git_output(
                &fixture.repo,
                &[
                    "cat-file",
                    "blob",
                    &format!("{}:\"quoted\".txt", checkpoint.tree)
                ],
            ),
            "quoted bytes"
        );
        #[cfg(unix)]
        assert_eq!(
            git_output(
                &fixture.repo,
                &[
                    "cat-file",
                    "blob",
                    &format!("{}:two\nlines.txt", checkpoint.tree)
                ],
            ),
            "newline bytes"
        );
    }

    /// Adds a gitlink at `sub` to the index with an empty folder, as a clone
    /// leaves a submodule nobody ran `git submodule update` for.
    fn add_uninitialised_submodule(fixture: &Fixture) -> String {
        let pointer = git_output(&fixture.repo, &["rev-parse", "HEAD"]);
        git(
            &fixture.repo,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{pointer},sub"),
            ],
        );
        git(&fixture.repo, &["commit", "-q", "-m", "submodule"]);
        fs::create_dir_all(fixture.repo.join("sub")).unwrap();
        pointer
    }

    #[test]
    fn an_uninitialised_submodule_does_not_block_a_restore() {
        let fixture = Fixture::new(&[("a.txt", "before\n")]);
        let pointer = add_uninitialised_submodule(&fixture);
        let tree = fixture.checkpoint("turn_1");
        assert_eq!(
            git_output(&fixture.repo, &["rev-parse", &format!("{tree}:sub")]),
            pointer,
            "the checkpoint records the index's gitlink, not the parent's HEAD"
        );
        // The turn edits a file and commits, so the parent's HEAD moves.
        write(&fixture.repo, "a.txt", "after\n");
        git(&fixture.repo, &["commit", "-q", "-am", "turn"]);

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        run_restore(&prepared);
        assert_eq!(read(&fixture.repo, "a.txt"), "before\n");
        assert!(
            fixture.repo.join("sub").is_dir(),
            "the submodule is left alone"
        );
    }

    #[cfg(unix)]
    fn chmod(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn only_the_owner_bit_makes_a_checkpointed_file_executable() {
        let fixture = Fixture::new(&[("README.md", "hi\n")]);
        write(&fixture.repo, "group.sh", "echo group\n");
        chmod(&fixture.repo.join("group.sh"), 0o654);
        let checkpoint = crate::worktrees::git::checkpoint_commit(&fixture.repo)
            .expect("checkpoint")
            .expect("checkpoint tree");
        let entry = git_output(&fixture.repo, &["ls-tree", &checkpoint.tree, "group.sh"]);
        assert!(
            entry.starts_with("100644 "),
            "git records a file without the owner bit as 100644: {entry}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_restore_puts_back_the_owner_executable_bit() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new(&[("README.md", "hi\n")]);
        write(&fixture.repo, "run.sh", "echo run\n");
        chmod(&fixture.repo.join("run.sh"), 0o755);
        git(&fixture.repo, &["add", "-A"]);
        git(&fixture.repo, &["commit", "-q", "-m", "exec"]);
        fixture.checkpoint("turn_1");
        // Group and others keep their bit; git calls this not executable.
        chmod(&fixture.repo.join("run.sh"), 0o655);

        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let outcome = run_restore(&prepared);
        let mode = fs::metadata(fixture.repo.join("run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o100, 0, "the owner bit is back: {mode:o}");
        assert_eq!(outcome.files, 1);
    }

    #[cfg(unix)]
    #[test]
    fn recovery_refuses_a_root_that_became_a_symlink() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new(&[("a.txt", "before\n")]);
        fixture.checkpoint("turn_1");
        write(&fixture.repo, "a.txt", "after\n");
        let prepared = fixture.prepare(&["turn_1"]).expect("prepare");
        let restore_id = prepared.restore_id().to_owned();

        // The checkout moved and a link to it took its place.
        let moved = fixture.dir.path().join("moved");
        fs::rename(&fixture.repo, &moved).unwrap();
        symlink(&moved, &fixture.repo).unwrap();

        let pending = read_pending_restore(&fixture.store, &restore_id).expect("read");
        let error = recover_restore(&fixture.store, &pending, false).unwrap_err();
        assert!(error.contains("symlink"), "{error}");
        assert_eq!(
            read(&moved, "a.txt"),
            "after\n",
            "recovery never writes through a linked root"
        );
        assert_eq!(
            pending_restore_ids(&fixture.store),
            vec![restore_id],
            "the journal is kept"
        );
    }
}
