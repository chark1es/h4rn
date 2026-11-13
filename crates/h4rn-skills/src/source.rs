//! Reading a skill from a local folder, a pinned git commit or a harness
//! directory.
//!
//! Git is invoked as a subprocess instead of pulling a full git library into
//! the daemon. The commit is resolved to a full id and recorded with the
//! version, so a sync or reinstall cannot silently follow a moving branch.

use std::path::{Path, PathBuf};
use std::process::Command;

use h4rn_protocol::SkillSource;

use crate::error::SkillError;
use crate::model::{SkillBundle, read_bundle};

/// A bundle plus the canonical source that should be recorded.
#[derive(Clone, Debug, PartialEq)]
pub struct LoadedSource {
    /// Parsed bundle.
    pub bundle: SkillBundle,
    /// Source with parent paths and commits resolved where possible.
    pub source: SkillSource,
}

/// Loads one source into a bundle.
pub fn load_source(source: &SkillSource) -> Result<LoadedSource, SkillError> {
    match source {
        SkillSource::LocalFolder { path } => {
            let root = PathBuf::from(path);
            let bundle = read_bundle(&root)?;
            Ok(LoadedSource {
                bundle,
                source: SkillSource::LocalFolder {
                    path: absolute(&root),
                },
            })
        }
        SkillSource::HarnessDir { harness, path } => {
            let root = PathBuf::from(path);
            let bundle = read_bundle(&root)?;
            Ok(LoadedSource {
                bundle,
                source: SkillSource::HarnessDir {
                    harness: *harness,
                    path: absolute(&root),
                },
            })
        }
        SkillSource::Git {
            url,
            commit,
            subdir,
        } => load_git(url, commit, subdir.as_deref()),
        SkillSource::Connect { .. } => Err(SkillError::InvalidSource(
            "a connect version is applied by sync, not read as a local source".to_owned(),
        )),
    }
}

fn load_git(url: &str, commit: &str, subdir: Option<&str>) -> Result<LoadedSource, SkillError> {
    if commit.trim().is_empty() {
        return Err(SkillError::InvalidSource(
            "a git source must be pinned to a commit".to_owned(),
        ));
    }
    let directory = std::env::temp_dir().join(format!("h4rn-skill-git-{}", uuid::Uuid::now_v7()));
    let result = (|| {
        run_git(
            None,
            &[
                "clone",
                "--quiet",
                "--no-checkout",
                url,
                &directory.display().to_string(),
            ],
        )?;
        run_git(Some(&directory), &["checkout", "--quiet", commit])?;
        let head = run_git(Some(&directory), &["rev-parse", "HEAD"])?;
        let head = head.trim().to_owned();
        if !head.starts_with(commit) && !commit.starts_with(&head) {
            return Err(SkillError::Git(format!(
                "checked out {head}, expected {commit}"
            )));
        }
        let root = match subdir {
            Some(subdir) => {
                let subdir = crate::model::normalize_rel_path(subdir)?;
                directory.join(subdir.replace('/', std::path::MAIN_SEPARATOR_STR))
            }
            None => directory.clone(),
        };
        let bundle = read_bundle(&root)?;
        Ok(LoadedSource {
            bundle,
            source: SkillSource::Git {
                url: url.to_owned(),
                commit: head,
                subdir: subdir.map(str::to_owned),
            },
        })
    })();
    let _ = std::fs::remove_dir_all(&directory);
    result
}

fn run_git(cwd: Option<&Path>, args: &[&str]) -> Result<String, SkillError> {
    let mut command = Command::new("git");
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    command.args(args);
    command.env("GIT_TERMINAL_PROMPT", "0");
    let output = command
        .output()
        .map_err(|error| SkillError::Git(format!("could not run git: {error}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SkillError::Git(format!(
            "git {} failed: {}",
            args.first().copied().unwrap_or(""),
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn absolute(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

/// Reads a directory the user selected for import without normalising it away.
pub fn read_import(path: &Path) -> Result<SkillBundle, SkillError> {
    read_bundle(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SKILL_FILE;

    fn make_repo() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run_git(None, &["init", "--quiet", &root.display().to_string()]).unwrap();
        run_git(Some(root), &["config", "user.email", "test@example.test"]).unwrap();
        run_git(Some(root), &["config", "user.name", "Test"]).unwrap();
        std::fs::write(
            root.join(SKILL_FILE),
            "---\nname: demo\ndescription: Demo\n---\n\n# Demo\n",
        )
        .unwrap();
        run_git(Some(root), &["add", "."]).unwrap();
        run_git(Some(root), &["commit", "--quiet", "-m", "one"]).unwrap();
        let head = run_git(Some(root), &["rev-parse", "HEAD"]).unwrap();
        (dir, head.trim().to_owned())
    }

    #[test]
    fn reads_a_local_folder_and_records_its_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("demo");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join(SKILL_FILE),
            "---\nname: demo\ndescription: Demo\n---\n",
        )
        .unwrap();
        let loaded = load_source(&SkillSource::LocalFolder {
            path: root.display().to_string(),
        })
        .unwrap();
        assert_eq!(loaded.bundle.manifest.name, "demo");
        assert!(matches!(loaded.source, SkillSource::LocalFolder { .. }));
    }

    #[test]
    fn pins_a_git_source_to_the_resolved_commit() {
        let (repo, head) = make_repo();
        let loaded = load_source(&SkillSource::Git {
            url: repo.path().display().to_string(),
            commit: head[..12].to_owned(),
            subdir: None,
        })
        .unwrap();
        match loaded.source {
            SkillSource::Git { commit, .. } => assert_eq!(commit, head),
            other => panic!("expected a git source, got {other:?}"),
        }
    }

    #[test]
    fn rejects_an_unpinned_git_source() {
        let error = load_source(&SkillSource::Git {
            url: "https://example.test/s.git".into(),
            commit: " ".into(),
            subdir: None,
        })
        .unwrap_err();
        assert!(matches!(error, SkillError::InvalidSource(_)));
    }
}
