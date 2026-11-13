//! Recursive filesystem watching with `notify`.
//!
//! Watchers live per workspace root for the life of the daemon and fan changes
//! out through one broadcast channel; each connection filters what it asked
//! for (`FsWatch`). Events are hints, not truth: the UI re-reads the file or
//! directory it cares about and keeps its own etag, so a missed or duplicated
//! event cannot corrupt state.
//!
//! `.git` is excluded because a single fetch or status can produce hundreds of
//! internal writes and none of them are edits the user made.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, mpsc};

use h4rn_protocol::FsChangeKind;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use super::paths::FsError;

/// One change the watcher observed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsChange {
    /// Canonical workspace root.
    pub workspace: PathBuf,
    /// Workspace-relative path with `/` separators.
    pub path: String,
    /// What happened.
    pub change: FsChangeKind,
}

/// Owns one recursive watcher per workspace and fans events out.
pub struct WatcherHub {
    sender: tokio::sync::broadcast::Sender<FsChange>,
    watchers: Mutex<HashMap<PathBuf, RecommendedWatcher>>,
}

impl WatcherHub {
    /// Creates the hub with a bounded broadcast channel.
    pub fn new() -> Result<Self, FsError> {
        let (sender, _) = tokio::sync::broadcast::channel(1_024);
        Ok(Self {
            sender,
            watchers: Mutex::new(HashMap::new()),
        })
    }

    /// Subscribes to every workspace's changes.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<FsChange> {
        self.sender.subscribe()
    }

    /// Starts a recursive watcher for `root` unless one is already running.
    pub fn ensure(&self, root: &Path) -> Result<(), FsError> {
        let mut watchers = self
            .watchers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if watchers.contains_key(root) {
            return Ok(());
        }
        let sender = self.sender.clone();
        let workspace = root.to_path_buf();
        let callback_root = workspace.clone();
        let (events_tx, events_rx) = mpsc::channel::<notify::Result<Event>>();
        let mut watcher = notify::recommended_watcher(move |event| {
            // `notify`'s callback is synchronous; forwarding through a channel
            // keeps slow consumers off the inotify thread.
            let _ = events_tx.send(event);
        })
        .map_err(|error| FsError::Io(format!("start watcher: {error}")))?;
        watcher
            .watch(&workspace, RecursiveMode::Recursive)
            .map_err(|error| FsError::Io(format!("watch {}: {error}", workspace.display())))?;

        std::thread::Builder::new()
            .name("h4rn-fswatch".into())
            .spawn(move || {
                for result in events_rx {
                    let Ok(event) = result else { continue };
                    for change in translate(&callback_root, &event) {
                        // A send error only means nobody is watching yet.
                        let _ = sender.send(change);
                    }
                }
            })
            .map_err(|error| FsError::Io(format!("spawn watcher: {error}")))?;

        watchers.insert(root.to_path_buf(), watcher);
        Ok(())
    }
}

fn translate(root: &Path, event: &Event) -> Vec<FsChange> {
    let mut changes = Vec::new();
    let kind = match event.kind {
        EventKind::Create(_) => FsChangeKind::Created,
        EventKind::Modify(notify::event::ModifyKind::Name(_)) => FsChangeKind::Renamed,
        EventKind::Modify(_) => FsChangeKind::Modified,
        EventKind::Remove(_) => FsChangeKind::Deleted,
        EventKind::Access(_) | EventKind::Other | EventKind::Any => return changes,
    };
    for (index, path) in event.paths.iter().enumerate() {
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let rel = relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        if rel.is_empty() || rel == ".git" || rel.starts_with(".git/") {
            continue;
        }
        // A rename carries both paths: the first is gone, the last appeared.
        let change = if kind == FsChangeKind::Renamed {
            if index + 1 == event.paths.len() {
                FsChangeKind::Renamed
            } else {
                FsChangeKind::Deleted
            }
        } else {
            kind
        };
        changes.push(FsChange {
            workspace: root.to_path_buf(),
            path: rel,
            change,
        });
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_internals_and_outside_paths_are_ignored() {
        let root = Path::new("/repo");
        let event = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Any),
            paths: vec![
                root.join(".git").join("index"),
                PathBuf::from("/elsewhere/secret"),
                root.join("src").join("main.rs"),
            ],
            attrs: Default::default(),
        };
        let changes = translate(root, &event);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "src/main.rs");
        assert_eq!(changes[0].change, FsChangeKind::Modified);
    }
}
