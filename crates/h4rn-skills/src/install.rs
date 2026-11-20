//! Per-harness installers.
//!
//! Every harness reads a directory containing `SKILL.md`, but the parent
//! directory differs. h4rn never guesses a path it has not verified (ADR-034):
//! the table below mirrors the layouts recorded in `docs/research/` and
//! `docs/skills.md`. Installs are copies, not symlinks, because the
//! `.h4rn-managed` marker must travel with the copy and because Codex and
//! Antigravity can share the project `.agents/skills` directory.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use h4rn_protocol::HarnessKind;
use serde::{Deserialize, Serialize};

use crate::error::SkillError;
use crate::model::{MARKER_FILE, SKILL_FILE, SkillBundle, write_bundle};

/// Ownership marker written beside every managed skill.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedMarker {
    /// Always [`MANAGED_BY`].
    pub managed_by: String,
    /// Registry id that owns the copy.
    pub skill_id: String,
    /// Frontmatter name at install time.
    pub skill_name: String,
    /// Installed content version.
    pub version: String,
    /// Harnesses that share this directory and this copy.
    pub harnesses: Vec<HarnessKind>,
    /// When h4rn installed it.
    pub installed_at: DateTime<Utc>,
}

/// Value of [`ManagedMarker::managed_by`].
pub const MANAGED_BY: &str = "h4rn";

/// One harness's user- and project-scope directories.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillLayout {
    /// Harness this layout belongs to.
    pub harness: HarnessKind,
    /// User-scope root that contains one directory per skill.
    pub user_root: Option<PathBuf>,
    /// Project-scope root, when the harness reads skills from the workspace.
    pub project_root: Option<PathBuf>,
    /// Whether the project root is shared with another harness.
    pub shared_project_root: bool,
    /// Whether the harness executable was found on this host.
    pub detected: bool,
}

/// A skill discovered in a harness directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstalledSkill {
    /// Directory that holds `SKILL.md`.
    pub path: PathBuf,
    /// Harness that owns the layout.
    pub harness: HarnessKind,
    /// Parsed marker, when h4rn installed it.
    pub marker: Option<ManagedMarker>,
    /// Content version when it can be read.
    pub version: Option<String>,
}

impl InstalledSkill {
    /// Whether h4rn owns this copy and may replace or remove it.
    pub fn managed(&self) -> bool {
        self.marker.is_some()
    }
}

/// Result of an install.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstalledCopy {
    /// Absolute directory the harness reads.
    pub path: PathBuf,
    /// Version installed.
    pub version: String,
}

/// Result of an uninstall.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UninstalledCopy {
    /// Whether a matching managed copy was found.
    pub found: bool,
    /// Whether the directory was removed entirely.
    pub removed_directory: bool,
    /// Directory involved, when found.
    pub path: Option<PathBuf>,
}

/// Builds the four layouts for one home directory and optional workspace.
pub fn detect_layouts(home: &Path, workspace: Option<&Path>) -> Vec<SkillLayout> {
    let claude_user = env_path("CLAUDE_CONFIG_DIR")
        .unwrap_or_else(|| home.join(".claude"))
        .join("skills");
    let codex_user = env_path("CODEX_HOME")
        .unwrap_or_else(|| home.join(".codex"))
        .join("skills");
    let opencode_user = env_path("OPENCODE_CONFIG_DIR")
        .or_else(|| {
            std::env::var_os("XDG_CONFIG_HOME").map(|dir| PathBuf::from(dir).join("opencode"))
        })
        .unwrap_or_else(|| home.join(".config").join("opencode"))
        .join("skills");
    let antigravity_user = home.join(".gemini").join("config").join("skills");

    let project = |relative: &str| workspace.map(|root| root.join(relative));
    vec![
        SkillLayout {
            harness: HarnessKind::ClaudeCode,
            user_root: Some(claude_user),
            project_root: project(".claude/skills"),
            shared_project_root: false,
            detected: command_exists("claude"),
        },
        SkillLayout {
            harness: HarnessKind::Codex,
            user_root: Some(codex_user),
            project_root: project(".agents/skills"),
            shared_project_root: true,
            detected: command_exists("codex"),
        },
        SkillLayout {
            harness: HarnessKind::Opencode,
            user_root: Some(opencode_user),
            project_root: project(".opencode/skills"),
            shared_project_root: false,
            detected: command_exists("opencode"),
        },
        SkillLayout {
            harness: HarnessKind::Antigravity,
            user_root: Some(antigravity_user),
            // `.agents/skills` is the verified first-wins project path shared
            // with Codex; the marker records both owners.
            project_root: project(".agents/skills"),
            shared_project_root: true,
            detected: command_exists("agy"),
        },
    ]
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

fn command_exists(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let candidates: Vec<String> = if cfg!(windows) {
        vec![
            format!("{name}.exe"),
            format!("{name}.cmd"),
            format!("{name}.bat"),
            name.to_owned(),
        ]
    } else {
        vec![name.to_owned()]
    };
    std::env::split_paths(&path).any(|directory| {
        candidates
            .iter()
            .any(|candidate| directory.join(candidate).is_file())
    })
}

/// Turns a skill name into a safe directory name.
pub fn skill_dir_name(name: &str) -> Result<String, SkillError> {
    let mut safe = String::new();
    for character in name.trim().chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
            safe.push(character);
        } else if character.is_whitespace() || character == '/' || character == '\\' {
            safe.push('-');
        }
    }
    let safe = safe.trim_matches(['-', '.']).to_owned();
    if safe.is_empty() || safe == ".." {
        return Err(SkillError::InvalidSkill {
            path: name.to_owned(),
            reason: "the skill name cannot be used as a directory name".to_owned(),
        });
    }
    Ok(safe)
}

/// Reads the marker beside a skill, if present.
pub fn read_marker(directory: &Path) -> Option<ManagedMarker> {
    let path = directory.join(MARKER_FILE);
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Installs a bundle for one harness under `root`.
///
/// A matching managed copy is updated in place and its marker gains the
/// harness. A directory without a marker is user-owned and is never touched.
pub fn install_bundle(
    root: &Path,
    harness: HarnessKind,
    skill_id: &str,
    bundle: &SkillBundle,
) -> Result<InstalledCopy, SkillError> {
    std::fs::create_dir_all(root)?;
    let version = bundle.version();
    let name = skill_dir_name(&bundle.manifest.name)?;
    let destination = root.join(&name);

    if destination.exists() {
        let Some(marker) = read_marker(&destination) else {
            return Err(SkillError::UserOwned {
                path: destination.display().to_string(),
            });
        };
        if marker.skill_id != skill_id {
            return Err(SkillError::UserOwned {
                path: destination.display().to_string(),
            });
        }
        if marker.version == version {
            let mut marker = marker;
            if !marker.harnesses.contains(&harness) {
                marker.harnesses.push(harness);
                write_marker(&destination, &marker)?;
            }
            return Ok(InstalledCopy {
                path: destination,
                version,
            });
        }
    }

    let staging = root.join(format!(".h4rn-tmp-{}", uuid::Uuid::now_v7()));
    let result = (|| {
        write_bundle(&staging, bundle)?;
        let mut harnesses = if destination.exists() {
            read_marker(&destination)
                .map(|marker| marker.harnesses)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        if !harnesses.contains(&harness) {
            harnesses.push(harness);
        }
        write_marker(
            &staging,
            &ManagedMarker {
                managed_by: MANAGED_BY.to_owned(),
                skill_id: skill_id.to_owned(),
                skill_name: bundle.manifest.name.clone(),
                version: version.clone(),
                harnesses,
                installed_at: Utc::now(),
            },
        )?;
        if destination.exists() {
            std::fs::remove_dir_all(&destination)?;
        }
        std::fs::rename(&staging, &destination)?;
        Ok(InstalledCopy {
            path: destination.clone(),
            version: version.clone(),
        })
    })();
    if result.is_err() && staging.exists() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result
}

fn write_marker(directory: &Path, marker: &ManagedMarker) -> Result<(), SkillError> {
    let bytes = serde_json::to_vec_pretty(marker)?;
    std::fs::write(directory.join(MARKER_FILE), bytes)?;
    Ok(())
}

/// Why [`adopt_folder`] left a folder alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdoptOutcome {
    /// The marker was written (or the harness added to an existing one).
    Adopted,
    /// Another registry skill already owns the folder.
    OwnedByOther,
    /// The folder's files differ from the registry version.
    ContentDiffers,
    /// The folder is not where h4rn installs this skill for the harness
    /// (a project-scope folder, or a directory name that does not match the
    /// skill name), so later installs would not replace it.
    NotInstallPath,
}

/// Takes ownership of a user-installed skill folder without touching its
/// files: when its content matches `version` and it sits where
/// [`install_bundle`] would put the skill, h4rn writes its marker so the copy
/// is updated by later versions and removed with the skill.
pub fn adopt_folder(
    root: &Path,
    directory: &Path,
    harness: HarnessKind,
    skill_id: &str,
    skill_name: &str,
    version: &str,
) -> Result<AdoptOutcome, SkillError> {
    let expected = root.join(skill_dir_name(skill_name)?);
    let same = match (
        std::fs::canonicalize(&expected),
        std::fs::canonicalize(directory),
    ) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    };
    if !same {
        return Ok(AdoptOutcome::NotInstallPath);
    }
    if let Some(mut marker) = read_marker(directory) {
        if marker.skill_id != skill_id {
            return Ok(AdoptOutcome::OwnedByOther);
        }
        if !marker.harnesses.contains(&harness) {
            marker.harnesses.push(harness);
            write_marker(directory, &marker)?;
        }
        return Ok(AdoptOutcome::Adopted);
    }
    let bundle = crate::model::read_bundle(directory)?;
    if bundle.version() != version {
        return Ok(AdoptOutcome::ContentDiffers);
    }
    write_marker(
        directory,
        &ManagedMarker {
            managed_by: MANAGED_BY.to_owned(),
            skill_id: skill_id.to_owned(),
            skill_name: bundle.manifest.name.clone(),
            version: version.to_owned(),
            harnesses: vec![harness],
            installed_at: Utc::now(),
        },
    )?;
    Ok(AdoptOutcome::Adopted)
}

/// Removes one harness's ownership of a managed copy.
pub fn uninstall_bundle(
    root: &Path,
    harness: HarnessKind,
    skill_id: &str,
) -> Result<UninstalledCopy, SkillError> {
    let installed = scan_root(root, harness)?;
    let Some(mut entry) = installed.into_iter().find(|entry| {
        entry
            .marker
            .as_ref()
            .is_some_and(|marker| marker.skill_id == skill_id)
    }) else {
        return Ok(UninstalledCopy {
            found: false,
            removed_directory: false,
            path: None,
        });
    };
    let marker = entry.marker.take().expect("filtered to managed copies");
    let mut harnesses = marker.harnesses;
    harnesses.retain(|owner| *owner != harness);
    if harnesses.is_empty() {
        std::fs::remove_dir_all(&entry.path)?;
        Ok(UninstalledCopy {
            found: true,
            removed_directory: true,
            path: Some(entry.path),
        })
    } else {
        let marker = ManagedMarker {
            harnesses,
            ..marker
        };
        write_marker(&entry.path, &marker)?;
        Ok(UninstalledCopy {
            found: true,
            removed_directory: false,
            path: Some(entry.path),
        })
    }
}

/// Lists every skill directory under one harness root.
pub fn scan_root(root: &Path, harness: HarnessKind) -> Result<Vec<InstalledSkill>, SkillError> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(root)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        // `Path::is_dir` follows symlinks, unlike `DirEntry::file_type`: tools
        // such as `npx skills` link each skill folder in from `~/.agents/skills`,
        // and the harness reads the link like a folder. A broken link is skipped.
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(".h4rn-tmp-") || !path.join(SKILL_FILE).is_file() {
            continue;
        }
        let marker = read_marker(&path);
        let version = marker
            .as_ref()
            .map(|marker| marker.version.clone())
            .or_else(|| {
                crate::model::read_bundle(&path)
                    .ok()
                    .map(|bundle| bundle.version())
            });
        found.push(InstalledSkill {
            path,
            harness,
            marker,
            version,
        });
    }
    Ok(found)
}

/// All managed skill ids under one root, regardless of harness.
pub fn managed_ids(root: &Path) -> BTreeSet<String> {
    scan_root(root, HarnessKind::ClaudeCode)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| entry.marker.map(|marker| marker.skill_id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{SKILL_FILE, read_bundle};

    fn make_bundle(dir: &Path, name: &str, version_note: &str) -> SkillBundle {
        let root = dir.join(format!("source-{version_note}"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join(SKILL_FILE),
            format!("---\nname: {name}\ndescription: Demo skill\n---\n\n{version_note}\n"),
        )
        .unwrap();
        read_bundle(&root).unwrap()
    }

    #[test]
    fn installs_copy_into_all_four_harness_roots() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let layouts = detect_layouts(home.path(), Some(workspace.path()));
        assert_eq!(layouts.len(), 4);
        let source = tempfile::tempdir().unwrap();
        let bundle = make_bundle(source.path(), "demo", "one");
        for layout in &layouts {
            let root = layout.user_root.as_ref().unwrap();
            let installed = install_bundle(root, layout.harness, "sk_1", &bundle).unwrap();
            assert!(installed.path.join(SKILL_FILE).is_file());
            let marker = read_marker(&installed.path).unwrap();
            assert_eq!(marker.skill_id, "sk_1");
            assert_eq!(marker.harnesses, vec![layout.harness]);
        }
    }

    #[test]
    fn never_overwrites_a_user_owned_directory() {
        let home = tempfile::tempdir().unwrap();
        let layout = &detect_layouts(home.path(), None)[0];
        let root = layout.user_root.as_ref().unwrap();
        let user = root.join("demo");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(user.join(SKILL_FILE), "# mine\n").unwrap();

        let source = tempfile::tempdir().unwrap();
        let bundle = make_bundle(source.path(), "demo", "one");
        let error = install_bundle(root, layout.harness, "sk_1", &bundle).unwrap_err();
        assert!(matches!(error, SkillError::UserOwned { .. }));
        assert_eq!(
            std::fs::read_to_string(user.join(SKILL_FILE)).unwrap(),
            "# mine\n"
        );
    }

    #[test]
    fn a_shared_root_records_both_harnesses_and_uninstalls_one() {
        let root = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        let bundle = make_bundle(source.path(), "demo", "one");
        let first = install_bundle(root.path(), HarnessKind::Codex, "sk_1", &bundle).unwrap();
        let second =
            install_bundle(root.path(), HarnessKind::Antigravity, "sk_1", &bundle).unwrap();
        assert_eq!(first.path, second.path);
        let marker = read_marker(&first.path).unwrap();
        assert_eq!(
            marker.harnesses,
            vec![HarnessKind::Codex, HarnessKind::Antigravity]
        );

        let outcome = uninstall_bundle(root.path(), HarnessKind::Codex, "sk_1").unwrap();
        assert!(outcome.found);
        assert!(!outcome.removed_directory);
        let marker = read_marker(&first.path).unwrap();
        assert_eq!(marker.harnesses, vec![HarnessKind::Antigravity]);

        let outcome = uninstall_bundle(root.path(), HarnessKind::Antigravity, "sk_1").unwrap();
        assert!(outcome.removed_directory);
        assert!(!first.path.exists());
    }

    #[test]
    fn uninstall_refuses_a_user_owned_copy() {
        let root = tempfile::tempdir().unwrap();
        let user = root.path().join("demo");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(user.join(SKILL_FILE), "# mine\n").unwrap();
        let outcome = uninstall_bundle(root.path(), HarnessKind::ClaudeCode, "sk_1").unwrap();
        assert!(!outcome.found);
        assert!(user.exists());
    }

    #[test]
    fn upgrading_keeps_the_marker_harness_list() {
        let root = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        let one = make_bundle(source.path(), "demo", "one");
        let two = make_bundle(source.path(), "demo", "two");
        install_bundle(root.path(), HarnessKind::Codex, "sk_1", &one).unwrap();
        install_bundle(root.path(), HarnessKind::Antigravity, "sk_1", &one).unwrap();
        let installed = install_bundle(root.path(), HarnessKind::Codex, "sk_1", &two).unwrap();
        let marker = read_marker(&installed.path).unwrap();
        assert_eq!(marker.version, two.version());
        assert_eq!(
            marker.harnesses,
            vec![HarnessKind::Codex, HarnessKind::Antigravity]
        );
    }

    #[cfg(unix)]
    #[test]
    fn scan_finds_symlinked_skill_folders_and_skips_broken_links() {
        let shared = tempfile::tempdir().unwrap();
        let linked = shared.path().join("linked");
        std::fs::create_dir_all(&linked).unwrap();
        std::fs::write(linked.join(SKILL_FILE), "---\nname: linked\n---\n").unwrap();
        let root = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(&linked, root.path().join("linked")).unwrap();
        std::os::unix::fs::symlink(shared.path().join("gone"), root.path().join("gone")).unwrap();

        let found = scan_root(root.path(), HarnessKind::ClaudeCode).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, root.path().join("linked"));
        assert!(found[0].version.is_some());
    }
}
