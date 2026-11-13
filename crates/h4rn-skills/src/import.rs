//! Importing skills that already exist in a harness directory.
//!
//! The preview is a pure scan: it reads every `SKILL.md`, hashes the bundle and
//! compares the result with the registry. Commit re-scans by candidate id, so
//! a preview cannot import a directory that changed after the user looked at
//! it. Content-hash dedupe means importing a skill h4rn already installed is a
//! no-op, and a different version of the same name becomes a conflict the user
//! resolves rather than a silent overwrite.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use h4rn_protocol::{HarnessKind, SkillImportCandidate, SkillImportPreview, SkillSource};

use crate::error::SkillError;
use crate::install::{SkillLayout, scan_root};
use crate::model::SkillBundle;
use crate::store::Registry;

/// One directory the scan found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportTarget {
    /// Absolute skill directory.
    pub path: PathBuf,
    /// Harness that reads it.
    pub harness: HarnessKind,
}

/// A preview plus the private map needed to commit it.
#[derive(Clone, Debug)]
pub struct ImportScan {
    /// Wire-facing preview.
    pub preview: SkillImportPreview,
    /// Candidate id to directory.
    pub targets: BTreeMap<String, ImportTarget>,
}

/// Scans every harness root and compares content with the registry.
pub async fn scan(layouts: &[SkillLayout], registry: &Registry) -> Result<ImportScan, SkillError> {
    let mut seen = BTreeSet::new();
    let mut candidates = Vec::new();
    let mut targets = BTreeMap::new();
    let mut conflicts = 0u32;
    let mut duplicates = 0u32;

    for layout in layouts {
        for root in [&layout.user_root, &layout.project_root]
            .into_iter()
            .flatten()
        {
            if !root.is_dir() {
                continue;
            }
            let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
            if !seen.insert(canonical) {
                continue;
            }
            for installed in scan_root(root, layout.harness)? {
                let bundle = match crate::model::read_bundle(&installed.path) {
                    Ok(bundle) => bundle,
                    Err(error) => {
                        tracing::debug!(path = %installed.path.display(), %error, "skipping unreadable skill");
                        continue;
                    }
                };
                let version = bundle.version();
                let id = candidate_id(&installed.path, &version);
                let (conflict, duplicate) = classify(registry, &bundle, &version).await?;
                if conflict.is_some() {
                    conflicts += 1;
                }
                if duplicate {
                    duplicates += 1;
                }
                targets.insert(
                    id.clone(),
                    ImportTarget {
                        path: installed.path.clone(),
                        harness: layout.harness,
                    },
                );
                candidates.push(SkillImportCandidate {
                    id,
                    name: bundle.manifest.name.clone(),
                    description: bundle.manifest.description.clone(),
                    version,
                    source_path: installed.path.display().to_string(),
                    harness: layout.harness,
                    duplicate,
                    conflict,
                });
            }
        }
    }

    candidates.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.source_path.cmp(&right.source_path))
    });
    Ok(ImportScan {
        preview: SkillImportPreview {
            candidates,
            conflicts,
            duplicates,
        },
        targets,
    })
}

async fn classify(
    registry: &Registry,
    bundle: &SkillBundle,
    version: &str,
) -> Result<(Option<String>, bool), SkillError> {
    match registry.find_by_name(&bundle.manifest.name).await? {
        None => Ok((None, false)),
        Some(existing) if existing.version == version => Ok((None, true)),
        Some(existing) => Ok((
            Some(format!(
                "the registry already has {} at version {}",
                existing.name, existing.version
            )),
            false,
        )),
    }
}

fn candidate_id(path: &Path, version: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(path.to_string_lossy().as_bytes());
    hasher.update(b"\0");
    hasher.update(version.as_bytes());
    format!("im_{}", &hasher.finalize().to_hex().to_string()[..16])
}

/// The result of committing one candidate.
#[derive(Clone, Debug, PartialEq)]
pub enum CommitOutcome {
    /// A new registry row.
    Added(String),
    /// The identical version was already present.
    Duplicate(String),
    /// A conflict row was written.
    Conflict(String),
}

/// Commits the selected candidates by re-reading their directories.
pub async fn commit(
    layouts: &[SkillLayout],
    registry: &Registry,
    candidate_ids: &[String],
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<CommitOutcome>, SkillError> {
    let scan = scan(layouts, registry).await?;
    let wanted: BTreeSet<&str> = candidate_ids.iter().map(String::as_str).collect();
    let mut outcomes = Vec::new();
    for candidate in scan.preview.candidates {
        if !wanted.contains(candidate.id.as_str()) {
            continue;
        }
        let target = scan
            .targets
            .get(&candidate.id)
            .cloned()
            .ok_or_else(|| SkillError::NotFound(candidate.id.clone()))?;
        let bundle = crate::model::read_bundle(&target.path)?;
        let source = SkillSource::HarnessDir {
            harness: target.harness,
            path: target.path.display().to_string(),
        };
        if candidate.duplicate {
            outcomes.push(CommitOutcome::Duplicate(candidate.name));
            continue;
        }
        if candidate.conflict.is_some() {
            let existing = registry
                .find_by_name(&bundle.manifest.name)
                .await?
                .ok_or_else(|| SkillError::NotFound(candidate.name.clone()))?;
            let conflict = registry
                .record_conflict(
                    &existing.id,
                    &existing.name,
                    Some(&existing.version),
                    &bundle,
                    &source,
                    &candidate.conflict.unwrap_or_default(),
                    now,
                )
                .await?;
            outcomes.push(CommitOutcome::Conflict(conflict.id));
            continue;
        }
        let record = registry.insert(&bundle, &source, now).await?;
        outcomes.push(CommitOutcome::Added(record.id));
    }
    Ok(outcomes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::{SkillLayout, install_bundle};
    use crate::model::{SKILL_FILE, read_bundle};

    async fn registry() -> (tempfile::TempDir, Registry) {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::open(dir.path()).await.unwrap();
        (dir, registry)
    }

    fn make_skill(root: &Path, name: &str, note: &str) -> SkillBundle {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(SKILL_FILE),
            format!("---\nname: {name}\ndescription: Demo {note}\n---\n\n{note}\n"),
        )
        .unwrap();
        read_bundle(&dir).unwrap()
    }

    fn layout(root: PathBuf) -> SkillLayout {
        SkillLayout {
            harness: HarnessKind::ClaudeCode,
            user_root: Some(root),
            project_root: None,
            shared_project_root: false,
            detected: true,
        }
    }

    #[tokio::test]
    async fn previews_and_commits_a_new_skill() {
        let (_dir, registry) = registry().await;
        let root = tempfile::tempdir().unwrap();
        let bundle = make_skill(root.path(), "demo", "one");
        let layouts = vec![layout(root.path().to_path_buf())];

        let preview = scan(&layouts, &registry).await.unwrap();
        assert_eq!(preview.preview.candidates.len(), 1);
        let id = preview.preview.candidates[0].id.clone();
        let outcomes = commit(&layouts, &registry, &[id], chrono::Utc::now())
            .await
            .unwrap();
        assert!(matches!(outcomes[0], CommitOutcome::Added(_)));
        assert_eq!(registry.list().await.unwrap()[0].version, bundle.version());

        let again = scan(&layouts, &registry).await.unwrap();
        assert_eq!(again.preview.duplicates, 1);
        assert!(again.preview.candidates[0].duplicate);
    }

    #[tokio::test]
    async fn a_different_version_becomes_a_conflict() {
        let (_dir, registry) = registry().await;
        let root = tempfile::tempdir().unwrap();
        let first = make_skill(root.path(), "demo", "one");
        registry
            .insert(
                &first,
                &SkillSource::LocalFolder { path: "/x".into() },
                chrono::Utc::now(),
            )
            .await
            .unwrap();
        // Rewrite the directory with different content under the same name.
        std::fs::write(
            root.path().join("demo").join(SKILL_FILE),
            "---\nname: demo\ndescription: Demo two\n---\n\ntwo\n",
        )
        .unwrap();

        let layouts = vec![layout(root.path().to_path_buf())];
        let scan = scan(&layouts, &registry).await.unwrap();
        assert_eq!(scan.preview.conflicts, 1);
        let id = scan.preview.candidates[0].id.clone();
        let outcomes = commit(&layouts, &registry, &[id], chrono::Utc::now())
            .await
            .unwrap();
        assert!(matches!(outcomes[0], CommitOutcome::Conflict(_)));
        assert_eq!(registry.conflicts().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn scans_an_installed_copy_as_a_duplicate() {
        let (_dir, registry) = registry().await;
        let root = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        let bundle = make_skill(source.path(), "demo", "one");
        let harness_root = root.path().join("skills");
        install_bundle(&harness_root, HarnessKind::ClaudeCode, "sk_1", &bundle).unwrap();
        registry
            .insert(
                &bundle,
                &SkillSource::LocalFolder { path: "/x".into() },
                chrono::Utc::now(),
            )
            .await
            .unwrap();

        let layouts = vec![layout(harness_root)];
        let scan = scan(&layouts, &registry).await.unwrap();
        assert!(scan.preview.candidates[0].duplicate);
    }
}
