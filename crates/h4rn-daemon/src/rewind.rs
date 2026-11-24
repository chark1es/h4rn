//! Editing an earlier prompt: the history cut and the file checkpoint restore.
//!
//! The daemon's thread lifecycle lives in [`crate::threads`]; this module holds
//! the two pieces of a rewind that do not depend on a live session: replaying
//! the journal to find which prompt turns are still live, and putting the
//! workspace back to the snapshot taken before the edited message.
//!
//! A rewind is one durable decision (ADR-047). The caller validates the
//! thread, restores files first and journals only after the restore succeeded,
//! so a failed restore leaves the conversation untouched.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use h4rn_protocol::{Attachment, Event, Item, MessageRole, ThreadId, TurnId};

use crate::fs::{ResolveMode, SnapshotEntry, SnapshotStore, resolve_inside};
use crate::journal::WireEvent;

/// One user prompt that is still part of a thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LivePrompt {
    /// The user message item the transcript and the UI name.
    pub item_id: String,
    /// The turn the message runs in.
    pub turn_id: TurnId,
    /// The text exactly as the person sent it.
    pub text: String,
    /// Attachments that came with it.
    pub attachments: Vec<Attachment>,
    /// True for a message read from a saved conversation, whose provider
    /// message id h4rn never chose.
    pub imported: bool,
}

/// The prompts still live in a thread's journal, oldest first.
///
/// A previous rewind is applied while replaying: the named prompt and every
/// prompt after it are dropped, so a second rewind can move further back but
/// can never select a message the person already removed.
pub(crate) fn live_prompts(events: &[WireEvent]) -> Vec<LivePrompt> {
    let mut prompts = all_prompts(events);
    for wire in events {
        if let Event::ThreadRewound { before_item_id, .. } = &wire.event
            && let Some(index) = prompts
                .iter()
                .position(|prompt| &prompt.item_id == before_item_id)
        {
            prompts.truncate(index);
        }
    }
    prompts
}

/// Every prompt the journal ever held, including ones an earlier rewind removed.
///
/// A second rewind can move further back than the first. The turns the first
/// one removed still have their pre-turn file snapshots, so a later
/// **Revert files too** must be able to put those changes back as well.
pub(crate) fn all_prompts(events: &[WireEvent]) -> Vec<LivePrompt> {
    let mut prompts: Vec<LivePrompt> = Vec::new();
    for wire in events {
        if let Event::ItemCompleted {
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
        } = &wire.event
        {
            prompts.push(LivePrompt {
                item_id: item_id.clone(),
                turn_id: turn_id.clone(),
                text: text.clone(),
                attachments: attachments.clone(),
                imported: at.is_some(),
            });
        }
    }
    prompts
}

/// The turn ids a rewind at `index` removes, in turn order and without repeats.
///
/// A message queued behind a running turn was journaled under that turn's id,
/// so two prompts can name one turn; the lifecycle counts turns, not messages.
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
struct RestoreOp {
    path: String,
    absolute: PathBuf,
    bytes: Option<Vec<u8>>,
}

/// Puts the workspace back to the state before the first removed turn.
///
/// Each path uses the earliest snapshot among the removed turns: a file first
/// touched by turn 3 is restored to how it looked then, which is also how it
/// looked before the edited prompt because turns 1 and 2 never wrote it. A file
/// the snapshot could not capture refuses the whole restore before anything is
/// written.
///
/// The current bytes of every target are read first and put back if a write
/// fails, so a failed restore does not leave a half-reverted tree.
pub(crate) fn restore_files(
    store: &SnapshotStore,
    workspace: &Path,
    thread_id: &ThreadId,
    turns: &[TurnId],
) -> Result<FileRestore, String> {
    let root = workspace.canonicalize().map_err(|error| {
        format!(
            "could not open the workspace {}: {error}",
            workspace.display()
        )
    })?;
    let mut entries: BTreeMap<String, SnapshotEntry> = BTreeMap::new();
    for turn in turns {
        for entry in store.entries(thread_id, turn) {
            entries.entry(entry.path.clone()).or_insert(entry);
        }
    }

    let mut ops = Vec::with_capacity(entries.len());
    for entry in entries.into_values() {
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
        let absolute = resolve_inside(&root, &entry.path, ResolveMode::Create)
            .map_err(|error| format!("{} cannot be restored: {error}", entry.path))?;
        ops.push(RestoreOp {
            path: entry.path,
            absolute,
            bytes,
        });
    }
    // A path that is now a directory is not something a turn snapshot can put
    // back as a file; refuse before touching anything else.
    for op in &ops {
        if op.absolute.is_dir() {
            return Err(format!(
                "{} is now a folder, so it cannot be restored",
                op.path
            ));
        }
    }

    let mut backups: Vec<(PathBuf, Option<Vec<u8>>)> = Vec::with_capacity(ops.len());
    for op in &ops {
        let current = if op.absolute.exists() {
            Some(std::fs::read(&op.absolute).map_err(|error| {
                format!("could not read {} before restoring it: {error}", op.path)
            })?)
        } else {
            None
        };
        backups.push((op.absolute.clone(), current));
    }

    let mut result = FileRestore::default();
    for (index, op) in ops.iter().enumerate() {
        let applied = match &op.bytes {
            Some(bytes) => {
                if let Some(parent) = op.absolute.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
                }
                std::fs::write(&op.absolute, bytes).map_err(|error| error.to_string())
            }
            None => {
                if op.absolute.exists() {
                    std::fs::remove_file(&op.absolute).map_err(|error| error.to_string())
                } else {
                    Ok(())
                }
            }
        };
        if let Err(error) = applied {
            rollback(&backups[..=index]);
            return Err(format!("could not restore {}: {error}", op.path));
        }
        match op.bytes {
            Some(_) => result.files += 1,
            None => result.deleted += 1,
        }
    }
    Ok(result)
}

/// Puts current bytes back after a failed restore, best effort.
fn rollback(backups: &[(PathBuf, Option<Vec<u8>>)]) {
    for (path, bytes) in backups {
        match bytes {
            Some(bytes) => {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(path, bytes);
            }
            None => {
                let _ = std::fs::remove_file(path);
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
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
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

    #[test]
    fn a_rewind_drops_the_named_prompt_and_every_prompt_after_it() {
        let events = vec![
            user(1, "item_user_1", "turn_1", "first"),
            user(2, "item_user_2", "turn_2", "second"),
            user(3, "item_user_3", "turn_3", "third"),
            event(
                4,
                Event::ThreadRewound {
                    thread_id: ThreadId::new("th_1"),
                    before_item_id: "item_user_2".into(),
                    files_restored: false,
                    at: chrono::Utc::now(),
                },
            ),
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
    fn the_whole_journal_still_names_turns_an_earlier_rewind_removed() {
        let events = vec![
            user(1, "item_user_1", "turn_1", "first"),
            user(2, "item_user_2", "turn_2", "second"),
            user(3, "item_user_3", "turn_3", "third"),
            event(
                4,
                Event::ThreadRewound {
                    thread_id: ThreadId::new("th_1"),
                    before_item_id: "item_user_3".into(),
                    files_restored: false,
                    at: chrono::Utc::now(),
                },
            ),
        ];
        assert_eq!(live_prompts(&events).len(), 2);
        let all = all_prompts(&events);
        assert_eq!(all.len(), 3);
        assert_eq!(
            removed_turns(&all, 1)
                .iter()
                .map(TurnId::as_str)
                .collect::<Vec<_>>(),
            ["turn_2", "turn_3"],
            "a later file restore can still reach the turn the first rewind removed"
        );
        assert!(!all[0].imported);
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
        // Turn 2 first sees the file; turn 3 sees it again, later.
        store
            .capture(
                &workspace,
                &thread,
                &TurnId::new("turn_2"),
                &["a.txt".into()],
            )
            .unwrap();
        std::fs::write(workspace.join("a.txt"), "skipped middle").unwrap();
        store
            .capture(
                &workspace,
                &thread,
                &TurnId::new("turn_3"),
                &["a.txt".into()],
            )
            .unwrap();
        std::fs::write(workspace.join("a.txt"), "after").unwrap();

        let outcome = restore_files(
            &store,
            &workspace,
            &thread,
            &[TurnId::new("turn_2"), TurnId::new("turn_3")],
        )
        .expect("restore");
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
        let outcome = restore_files(&store, &workspace, &thread, std::slice::from_ref(&turn))
            .expect("restore");
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
        let refused = restore_files(&store, &workspace, &thread, &[TurnId::new("turn_big")]);
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
        let refused = restore_files(&store, &workspace, &thread, &[TurnId::new("turn_1")]);
        assert!(refused.unwrap_err().contains("missing"));
        assert_eq!(
            std::fs::read_to_string(workspace.join("keep.txt")).unwrap(),
            "current"
        );
    }
}
