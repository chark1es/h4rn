//! Skill bundles, canonical manifests and content hashing.
//!
//! A bundle is every file under one skill directory except the `.h4rn-managed`
//! marker. The manifest includes each file's relative path, size, BLAKE3 digest
//! and executable bit; the version is the BLAKE3 digest of the canonical JSON
//! manifest. Nothing machine-specific (absolute path, mtime, owner) enters the
//! hash, so the same directory imported on two machines has one version id.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use h4rn_protocol::{SkillBundleFile, SkillDiffFile, SkillFile, SkillFileChange, SkillManifest};

use crate::error::SkillError;
use crate::frontmatter;

/// Entry point every harness reads.
pub const SKILL_FILE: &str = "SKILL.md";
/// Marker h4rn writes beside the files it owns.
pub const MARKER_FILE: &str = ".h4rn-managed";
/// Hard cap on one bundle.
pub const MAX_BUNDLE_BYTES: u64 = 16 * 1024 * 1024;
/// Hard cap on one file.
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// Hard cap on the number of files.
pub const MAX_FILES: usize = 512;
/// Canonical manifest format tag; changing it changes every version id.
const MANIFEST_FORMAT: &str = "h4rn-skill-manifest-v1";

/// A version's files and manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillBundle {
    /// Canonical manifest.
    pub manifest: SkillManifest,
    /// File bytes, keyed by relative path with forward slashes.
    pub files: BTreeMap<String, Vec<u8>>,
}

impl SkillBundle {
    /// The content-addressed version id.
    pub fn version(&self) -> String {
        version_for(&self.manifest)
    }

    /// The wire form of every file.
    pub fn wire_files(&self) -> Vec<SkillBundleFile> {
        self.files
            .iter()
            .map(|(path, bytes)| SkillBundleFile {
                path: path.clone(),
                content: STANDARD_NO_PAD.encode(bytes),
            })
            .collect()
    }

    /// Rebuilds a bundle from wire files and a manifest.
    pub fn from_wire(
        manifest: SkillManifest,
        files: Vec<SkillBundleFile>,
    ) -> Result<Self, SkillError> {
        let mut map = BTreeMap::new();
        for file in files {
            let path = normalize_rel_path(&file.path)?;
            let bytes = STANDARD_NO_PAD
                .decode(file.content.as_bytes())
                .map_err(|error| SkillError::InvalidSkill {
                    path: path.clone(),
                    reason: format!("invalid base64 file content: {error}"),
                })?;
            map.insert(path, bytes);
        }
        for entry in &manifest.files {
            let Some(bytes) = map.get(&entry.path) else {
                return Err(SkillError::InvalidSkill {
                    path: entry.path.clone(),
                    reason: "the manifest lists a file the payload omits".to_owned(),
                });
            };
            if bytes.len() as u64 != entry.size {
                return Err(SkillError::InvalidSkill {
                    path: entry.path.clone(),
                    reason: "the payload size does not match the manifest".to_owned(),
                });
            }
            let digest = blake3::hash(bytes).to_hex().to_string();
            if digest != entry.blake3 {
                return Err(SkillError::InvalidSkill {
                    path: entry.path.clone(),
                    reason: "the payload digest does not match the manifest".to_owned(),
                });
            }
        }
        if map.len() != manifest.files.len() {
            return Err(SkillError::InvalidSkill {
                path: ".".to_owned(),
                reason: "the payload has files the manifest does not list".to_owned(),
            });
        }
        Ok(Self {
            manifest,
            files: map,
        })
    }
}

/// Reads one skill directory into a bundle.
pub fn read_bundle(root: &Path) -> Result<SkillBundle, SkillError> {
    let skill_md = root.join(SKILL_FILE);
    if !skill_md.is_file() {
        return Err(SkillError::InvalidSkill {
            path: root.display().to_string(),
            reason: format!("{SKILL_FILE} is missing"),
        });
    }
    let source = std::fs::read_to_string(&skill_md)?;
    let parsed = frontmatter::parse(&source).map_err(|error| SkillError::InvalidSkill {
        path: skill_md.display().to_string(),
        reason: error.to_string(),
    })?;
    let fallback_name = root
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("skill")
        .to_owned();
    let name = parsed
        .frontmatter
        .name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(fallback_name);
    let description = parsed.frontmatter.description.unwrap_or_default();

    let mut files = BTreeMap::new();
    let mut manifest_files = Vec::new();
    let mut total_bytes = 0u64;
    walk(
        root,
        root,
        &mut files,
        &mut manifest_files,
        &mut total_bytes,
    )?;

    let Some(skill_bytes) = files.get(SKILL_FILE) else {
        return Err(SkillError::InvalidSkill {
            path: root.display().to_string(),
            reason: format!("{SKILL_FILE} could not be read"),
        });
    };
    if skill_bytes.is_empty() {
        return Err(SkillError::InvalidSkill {
            path: SKILL_FILE.to_owned(),
            reason: "SKILL.md is empty".to_owned(),
        });
    }

    manifest_files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(SkillBundle {
        manifest: SkillManifest {
            name,
            description,
            total_bytes,
            files: manifest_files,
        },
        files,
    })
}

fn walk(
    root: &Path,
    directory: &Path,
    files: &mut BTreeMap<String, Vec<u8>>,
    manifest: &mut Vec<SkillFile>,
    total_bytes: &mut u64,
) -> Result<(), SkillError> {
    let mut entries: Vec<_> = std::fs::read_dir(directory)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == MARKER_FILE || name == ".git" || name == ".DS_Store" {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            // A symlink can point outside the skill directory. Refusing it is
            // safer than silently copying a file the user did not select.
            tracing::warn!(path = %path.display(), "skipping symlink in skill bundle");
            continue;
        }
        if file_type.is_dir() {
            walk(root, &path, files, manifest, total_bytes)?;
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| SkillError::InvalidSkill {
                path: path.display().to_string(),
                reason: "path escaped the skill directory".to_owned(),
            })?;
        let relative = normalize_rel_path(&relative.to_string_lossy())?;
        let size = entry.metadata()?.len();
        if size > MAX_FILE_BYTES {
            return Err(SkillError::TooLarge(format!(
                "{relative} is {size} bytes; the per-file limit is {MAX_FILE_BYTES}"
            )));
        }
        *total_bytes = total_bytes.saturating_add(size);
        if *total_bytes > MAX_BUNDLE_BYTES {
            return Err(SkillError::TooLarge(format!(
                "the bundle is over the {MAX_BUNDLE_BYTES}-byte limit"
            )));
        }
        if manifest.len() >= MAX_FILES {
            return Err(SkillError::TooLarge(format!(
                "the bundle has more than {MAX_FILES} files"
            )));
        }
        let mut bytes = Vec::with_capacity(size as usize);
        std::fs::File::open(&path)?.read_to_end(&mut bytes)?;
        let executable = is_executable(&entry.metadata()?);
        manifest.push(SkillFile {
            path: relative.clone(),
            size: bytes.len() as u64,
            blake3: blake3::hash(&bytes).to_hex().to_string(),
            executable,
        });
        files.insert(relative, bytes);
    }
    Ok(())
}

#[cfg(unix)]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &std::fs::Metadata) -> bool {
    false
}

/// Writes a bundle's files under `root`, creating directories as needed.
///
/// The marker is deliberately not written here; the installer owns it so the
/// marker can list every harness that shares one directory.
pub fn write_bundle(root: &Path, bundle: &SkillBundle) -> Result<(), SkillError> {
    std::fs::create_dir_all(root)?;
    for (path, bytes) in &bundle.files {
        let relative = normalize_rel_path(path)?;
        let destination = root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&destination, bytes)?;
        if let Some(file) = bundle.manifest.files.iter().find(|file| file.path == *path)
            && file.executable
        {
            set_executable(&destination)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), SkillError> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    std::fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), SkillError> {
    Ok(())
}

/// The canonical version id for one manifest.
pub fn version_for(manifest: &SkillManifest) -> String {
    #[derive(serde::Serialize)]
    struct Canonical<'a> {
        format: &'static str,
        name: &'a str,
        description: &'a str,
        files: &'a [SkillFile],
    }
    let canonical = Canonical {
        format: MANIFEST_FORMAT,
        name: &manifest.name,
        description: &manifest.description,
        files: &manifest.files,
    };
    let bytes = serde_json::to_vec(&canonical).expect("a manifest is always serialisable");
    blake3::hash(&bytes).to_hex().to_string()
}

/// Renames a bundle by rewriting the `name:` line in `SKILL.md` and updating
/// the manifest entry, so the directory name, frontmatter and manifest agree.
pub fn rename_bundle(bundle: &mut SkillBundle, name: &str) -> Result<(), SkillError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(SkillError::InvalidSkill {
            path: SKILL_FILE.to_owned(),
            reason: "the new name is empty".to_owned(),
        });
    }
    let Some(bytes) = bundle.files.get(SKILL_FILE).cloned() else {
        return Err(SkillError::InvalidSkill {
            path: SKILL_FILE.to_owned(),
            reason: "SKILL.md is missing".to_owned(),
        });
    };
    let text = String::from_utf8(bytes).map_err(|_| SkillError::InvalidSkill {
        path: SKILL_FILE.to_owned(),
        reason: "SKILL.md is not UTF-8".to_owned(),
    })?;
    let rewritten = rewrite_name(&text, name);
    let bytes = rewritten.into_bytes();
    bundle.files.insert(SKILL_FILE.to_owned(), bytes.clone());
    bundle.manifest.name = name.to_owned();
    if let Some(entry) = bundle
        .manifest
        .files
        .iter_mut()
        .find(|entry| entry.path == SKILL_FILE)
    {
        entry.size = bytes.len() as u64;
        entry.blake3 = blake3::hash(&bytes).to_hex().to_string();
    }
    bundle.manifest.total_bytes = bundle.manifest.files.iter().map(|file| file.size).sum();
    Ok(())
}

fn rewrite_name(text: &str, name: &str) -> String {
    let line = format!("name: {name}");
    let normalized = text.strip_prefix('\u{feff}').unwrap_or(text);
    if normalized.starts_with("---") {
        let mut lines: Vec<String> = normalized.lines().map(str::to_owned).collect();
        let mut replaced = false;
        for candidate in lines.iter_mut().skip(1) {
            if candidate.trim() == "---" {
                break;
            }
            if candidate.trim_start().starts_with("name:") {
                *candidate = line.clone();
                replaced = true;
                break;
            }
        }
        if !replaced {
            lines.insert(1, line);
        }
        lines.join("\n")
    } else {
        format!("---\n{line}\n---\n\n{normalized}")
    }
}

/// Normalises and validates a bundle-relative path.
pub fn normalize_rel_path(path: &str) -> Result<String, SkillError> {
    let replaced = path.replace('\\', "/");
    let candidate = Path::new(&replaced);
    if candidate.is_absolute() {
        return Err(SkillError::InvalidSkill {
            path: path.to_owned(),
            reason: "absolute paths are not allowed in a bundle".to_owned(),
        });
    }
    let mut parts = Vec::new();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(SkillError::InvalidSkill {
                    path: path.to_owned(),
                    reason: "the path traverses outside the bundle".to_owned(),
                });
            }
        }
    }
    if parts.is_empty() {
        return Err(SkillError::InvalidSkill {
            path: path.to_owned(),
            reason: "the path is empty".to_owned(),
        });
    }
    Ok(parts.join("/"))
}

/// Diffs two manifests by path.
pub fn diff_manifests(
    skill_id: &str,
    from: Option<&SkillManifest>,
    to: &SkillManifest,
    to_version: &str,
) -> h4rn_protocol::SkillDiff {
    let old: BTreeMap<&str, &SkillFile> = from
        .map(|manifest| {
            manifest
                .files
                .iter()
                .map(|file| (file.path.as_str(), file))
                .collect()
        })
        .unwrap_or_default();
    let new: BTreeMap<&str, &SkillFile> = to
        .files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect();
    let mut paths: Vec<&str> = old.keys().chain(new.keys()).copied().collect();
    paths.sort_unstable();
    paths.dedup();

    let files = paths
        .into_iter()
        .filter_map(|path| {
            let before = old.get(path).copied();
            let after = new.get(path).copied();
            let change = match (before, after) {
                (None, Some(_)) => SkillFileChange::Added,
                (Some(_), None) => SkillFileChange::Removed,
                (Some(before), Some(after)) if before.blake3 != after.blake3 => {
                    SkillFileChange::Modified
                }
                _ => return None,
            };
            Some(SkillDiffFile {
                path: path.to_owned(),
                change,
                old_blake3: before.map(|file| file.blake3.clone()),
                new_blake3: after.map(|file| file.blake3.clone()),
                old_size: before.map(|file| file.size),
                new_size: after.map(|file| file.size),
            })
        })
        .collect();

    h4rn_protocol::SkillDiff {
        skill_id: skill_id.to_owned(),
        from_version: from.map(version_for),
        to_version: to_version.to_owned(),
        files,
    }
}

/// Where the registry stores content-addressed bundles.
pub fn bundle_root(data_dir: &Path) -> PathBuf {
    data_dir.join("skills").join("bundles")
}

/// Where pending conflict candidates live.
pub fn pending_root(data_dir: &Path) -> PathBuf {
    data_dir.join("skills").join("pending")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, name: &str, description: &str, extra: &[(&str, &str)]) {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(
            root.join(SKILL_FILE),
            format!("---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n"),
        )
        .unwrap();
        for (path, content) in extra {
            let path = root.join(path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, content).unwrap();
        }
    }

    #[test]
    fn the_same_content_hashes_the_same_on_two_directories() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        write_skill(
            &first.path().join("one"),
            "demo",
            "Demo skill",
            &[("reference/a.md", "hello"), ("scripts/run.sh", "echo hi\n")],
        );
        write_skill(
            &second.path().join("two"),
            "demo",
            "Demo skill",
            &[("scripts/run.sh", "echo hi\n"), ("reference/a.md", "hello")],
        );
        let a = read_bundle(&first.path().join("one")).unwrap();
        let b = read_bundle(&second.path().join("two")).unwrap();
        assert_eq!(a.version(), b.version());
    }

    #[test]
    fn changing_one_byte_changes_the_version() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("demo");
        write_skill(&root, "demo", "Demo", &[("a.md", "one")]);
        let before = read_bundle(&root).unwrap().version();
        std::fs::write(root.join("a.md"), "two").unwrap();
        let after = read_bundle(&root).unwrap().version();
        assert_ne!(before, after);
    }

    #[test]
    fn the_marker_is_not_part_of_the_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("demo");
        write_skill(&root, "demo", "Demo", &[]);
        let version = read_bundle(&root).unwrap().version();
        std::fs::write(root.join(MARKER_FILE), "{}").unwrap();
        assert_eq!(version, read_bundle(&root).unwrap().version());
    }

    #[test]
    fn rejects_parent_traversal() {
        assert!(normalize_rel_path("../secret").is_err());
        assert!(normalize_rel_path("/etc/passwd").is_err());
        assert_eq!(normalize_rel_path("a\\b").unwrap(), "a/b");
    }

    #[test]
    fn diff_lists_added_modified_and_removed() {
        let old = SkillManifest {
            name: "demo".into(),
            description: "Demo".into(),
            total_bytes: 2,
            files: vec![
                SkillFile {
                    path: "a.md".into(),
                    size: 1,
                    blake3: "a".into(),
                    executable: false,
                },
                SkillFile {
                    path: "b.md".into(),
                    size: 1,
                    blake3: "b".into(),
                    executable: false,
                },
            ],
        };
        let new = SkillManifest {
            name: "demo".into(),
            description: "Demo".into(),
            total_bytes: 2,
            files: vec![
                SkillFile {
                    path: "a.md".into(),
                    size: 2,
                    blake3: "c".into(),
                    executable: false,
                },
                SkillFile {
                    path: "c.md".into(),
                    size: 1,
                    blake3: "d".into(),
                    executable: false,
                },
            ],
        };
        let diff = diff_manifests("sk_1", Some(&old), &new, "v2");
        assert_eq!(diff.files.len(), 3);
        assert_eq!(diff.files[0].change, SkillFileChange::Modified);
        assert_eq!(diff.files[1].change, SkillFileChange::Removed);
        assert_eq!(diff.files[2].change, SkillFileChange::Added);
    }
}
