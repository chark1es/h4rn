//! This machine's worktree defaults: where new checkouts go and how their
//! branches are named.
//!
//! The settings live in `<data_dir>/worktree-settings.json`, beside the
//! tracker's database. They are per machine because the location is a path on
//! this host, and the daemon that would create the worktree reads them.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use h4rn_protocol::WorktreeSettings;

/// A validated `WorktreeSettings`, shared by the protocol edge and the
/// worktree creator.
#[derive(Clone)]
pub struct WorktreeSettingsStore {
    path: PathBuf,
    value: Arc<RwLock<WorktreeSettings>>,
}

impl WorktreeSettingsStore {
    /// Opens `<data_dir>/worktree-settings.json`.
    ///
    /// An unreadable or malformed file falls back to the defaults with a
    /// warning: a bad preference must not stop the daemon from starting.
    pub fn open(data_dir: &Path) -> Result<Self, String> {
        let path = data_dir.join("worktree-settings.json");
        let value = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<WorktreeSettings>(&bytes) {
                Ok(settings) => normalize(settings)?,
                Err(error) => {
                    tracing::warn!(
                        %error,
                        path = %path.display(),
                        "ignoring unreadable worktree settings"
                    );
                    WorktreeSettings::default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                WorktreeSettings::default()
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    path = %path.display(),
                    "could not read worktree settings; using defaults"
                );
                WorktreeSettings::default()
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
            value: Arc::new(RwLock::new(WorktreeSettings::default())),
        }
    }

    /// The current settings.
    pub fn get(&self) -> WorktreeSettings {
        self.value
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Validates and saves new settings, returning what was saved.
    pub fn set(&self, settings: WorktreeSettings) -> Result<WorktreeSettings, String> {
        let settings = normalize(settings)?;
        if !self.path.as_os_str().is_empty() {
            write_atomic(&self.path, &settings)?;
        }
        *self
            .value
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = settings.clone();
        Ok(settings)
    }
}

/// Trims and validates settings: the location must be absolute or a `~` path,
/// and the branch prefix is kept exactly as typed (non-empty by default).
fn normalize(mut settings: WorktreeSettings) -> Result<WorktreeSettings, String> {
    // An explicit empty prefix is a choice (add nothing); only a missing field
    // falls back to the default, which serde supplies when reading.
    settings.location = match settings.location.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(location) => {
            validate_location(location)?;
            Some(location.to_owned())
        }
    };
    Ok(settings)
}

/// Whether `location` is an absolute path or a path rooted at the home folder.
pub fn validate_location(location: &str) -> Result<(), String> {
    if location == "~" || location.starts_with("~/") {
        return Ok(());
    }
    if location.starts_with('~') {
        return Err(format!(
            "the worktree location `{location}` must be an absolute path or start with `~/`"
        ));
    }
    if !Path::new(location).is_absolute() {
        return Err(format!(
            "the worktree location `{location}` must be an absolute path or start with `~/`"
        ));
    }
    Ok(())
}

/// Expands a validated location into a directory on this machine.
pub fn expand_location(location: &str, home: Option<&Path>) -> Result<PathBuf, String> {
    if location == "~" {
        return home
            .map(Path::to_path_buf)
            .ok_or_else(|| "the home folder could not be found for `~`".to_owned());
    }
    if let Some(rest) = location.strip_prefix("~/") {
        return home
            .map(|home| home.join(rest))
            .ok_or_else(|| "the home folder could not be found for `~`".to_owned());
    }
    Ok(PathBuf::from(location))
}

/// Writes `settings` to `path` through a temporary file beside it, so a reader
/// sees either the old file or the new one.
fn write_atomic(path: &Path, settings: &WorktreeSettings) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(settings)
        .map_err(|error| format!("could not serialise worktree settings: {error}"))?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "worktree-settings.json".to_owned());
    let temporary = path.with_file_name(format!(".{name}.{}.tmp", uuid::Uuid::now_v7().simple()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        Ok::<(), std::io::Error>(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(|error| format!("could not save {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_a_h4rn_prefix_and_the_data_directory() {
        let settings = normalize(WorktreeSettings::default()).expect("defaults");
        assert_eq!(settings.branch_prefix, "h4rn/");
        assert_eq!(settings.location, None);
    }

    #[test]
    fn a_relative_location_is_refused() {
        let settings = WorktreeSettings {
            location: Some("worktrees".into()),
            ..WorktreeSettings::default()
        };
        let error = normalize(settings).expect_err("relative");
        assert!(error.contains("absolute path"));
    }

    #[test]
    fn a_tilde_location_expands_against_the_home_folder() {
        let home = Path::new("/home/tester");
        assert_eq!(
            expand_location("~/wt", Some(home)).expect("expand"),
            home.join("wt")
        );
        assert_eq!(expand_location("~", Some(home)).expect("expand"), home);
        assert_eq!(
            expand_location("/mnt/wt", Some(home)).expect("expand"),
            PathBuf::from("/mnt/wt")
        );
        assert!(expand_location("~/wt", None).is_err());
    }

    #[test]
    fn an_explicit_empty_prefix_is_kept_and_settings_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = WorktreeSettingsStore::open(dir.path()).expect("open");
        assert_eq!(store.get().branch_prefix, "h4rn/");
        let saved = store
            .set(WorktreeSettings {
                location: Some("~/worktrees".into()),
                branch_prefix: String::new(),
            })
            .expect("set");
        assert_eq!(saved.location.as_deref(), Some("~/worktrees"));
        assert_eq!(saved.branch_prefix, "");

        let reopened = WorktreeSettingsStore::open(dir.path()).expect("reopen");
        assert_eq!(reopened.get(), saved);
    }

    #[test]
    fn a_missing_prefix_in_the_file_gets_the_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("worktree-settings.json"),
            r#"{"location":null}"#,
        )
        .expect("write");
        let store = WorktreeSettingsStore::open(dir.path()).expect("open");
        assert_eq!(store.get().branch_prefix, "h4rn/");
    }
}
