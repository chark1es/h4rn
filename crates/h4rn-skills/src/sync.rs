//! Pushing and pulling the registry through `h4rn-connect`.
//!
//! The daemon owns the local registry; connect stores an envelope-encrypted
//! copy for the same user. A push includes each skill's full bundle and version
//! vector, so the server can compare causality instead of guessing from a
//! timestamp alone. Concurrent edits are applied last-writer-wins and leave a
//! conflict row on the relay; a daemon whose push loses also records a local
//! conflict pointing at its rejected bundle, so the user can restore it. A
//! stale entry that has already been dominated is rejected and also surfaced as
//! a conflict (ADR-035).
//!
//! Secrets are scanned before the first byte leaves the machine. A block is a
//! successful, visible status, not a network error: the user sees exactly which
//! file and line to fix.

use async_trait::async_trait;
use chrono::Utc;
use h4rn_protocol::{
    SkillSyncAck, SkillSyncConflict, SkillSyncEntry, SkillSyncPull, SkillSyncPullResponse,
    SkillSyncPush, SkillSyncPushResponse, SkillSyncState, SkillSyncStatus, SkillVersionVector,
};

use crate::error::SkillError;
use crate::model::SkillBundle;
use crate::secret::{SecretFinding, scan_bundle};
use crate::store::{Registry, SkillRecord};

/// The small API the daemon implements on top of its connect client.
#[async_trait]
pub trait SkillSyncApi: Send + Sync {
    /// Publishes local entries and returns the server's decisions.
    async fn push(&self, request: SkillSyncPush) -> Result<SkillSyncPushResponse, SkillError>;

    /// Reads entries changed since a cursor.
    async fn pull(&self, request: SkillSyncPull) -> Result<SkillSyncPullResponse, SkillError>;
}

/// What one sync did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncReport {
    /// Status for the UI and the transient event.
    pub status: SkillSyncStatus,
    /// Skill ids whose local copy was replaced by a remote version.
    pub applied: Vec<String>,
}

/// Builds the push payload, refusing when any bundle contains a secret.
pub async fn build_entries(
    registry: &Registry,
    machine_id: &str,
    include_deleted: bool,
) -> Result<Vec<SkillSyncEntry>, SkillError> {
    let mut entries = Vec::new();
    let mut findings: Vec<SecretFinding> = Vec::new();
    for record in registry.list_for_sync().await? {
        if record.deleted && !include_deleted {
            continue;
        }
        let (manifest, files) = if record.deleted {
            (record.manifest.clone(), Vec::new())
        } else {
            let bundle = registry.bundle(&record.version).await?;
            findings.extend(scan_bundle(&bundle));
            (bundle.manifest.clone(), bundle.wire_files())
        };
        let mut vector = SkillVersionVector::default();
        vector.clocks.insert(machine_id.to_owned(), record.revision);
        entries.push(SkillSyncEntry {
            skill_id: record.id.clone(),
            name: record.name.clone(),
            description: record.description.clone(),
            version: record.version.clone(),
            manifest,
            files,
            vector,
            updated_at: record.updated_at,
            deleted: record.deleted,
            expected_etag: None,
        });
    }
    if !findings.is_empty() {
        return Err(SkillError::Secrets(findings));
    }
    Ok(entries)
}

/// Runs one push/pull cycle.
pub async fn sync_now(
    registry: &Registry,
    api: &dyn SkillSyncApi,
    machine_id: &str,
    cursor: Option<String>,
) -> Result<SyncReport, SkillError> {
    let entries = match build_entries(registry, machine_id, true).await {
        Ok(entries) => entries,
        Err(SkillError::Secrets(findings)) => {
            let proto = SkillError::Secrets(findings).into_proto();
            return Ok(SyncReport {
                status: SkillSyncStatus {
                    state: SkillSyncState::Blocked,
                    last_sync: None,
                    pushed: 0,
                    pulled: 0,
                    conflicts: registry.conflicts().await?.len() as u32,
                    etag: cursor,
                    message: Some(proto.message),
                    hint: proto.hint,
                },
                applied: Vec::new(),
            });
        }
        Err(error) => return Err(error),
    };

    let push = api
        .push(SkillSyncPush {
            entries: entries.clone(),
        })
        .await?;
    let accepted = push.accepted.len() as u32;
    let rejected = push.rejected.len() as u32;

    // A rejected local version is still on disk, so record it as the
    // resolvable candidate. The server's conflict row names the version that
    // won; the user can then keep the winner or restore this copy.
    for ack in &push.rejected {
        let Some(conflict) = push
            .conflicts
            .iter()
            .find(|conflict| conflict.skill_id == ack.skill_id)
        else {
            continue;
        };
        let Some(entry) = entries
            .iter()
            .find(|entry| entry.skill_id == ack.skill_id && !entry.deleted)
        else {
            continue;
        };
        let bundle = bundle_for(entry)?;
        registry
            .record_conflict(
                &entry.skill_id,
                &entry.name,
                conflict.local_version.as_deref(),
                &bundle,
                &h4rn_protocol::SkillSource::Connect {
                    skill_id: entry.skill_id.clone(),
                    version: entry.version.clone(),
                },
                &conflict.reason,
                Utc::now(),
            )
            .await?;
    }

    let pull = api
        .pull(SkillSyncPull {
            since: cursor.clone(),
        })
        .await?;
    let mut applied = Vec::new();
    for entry in &pull.entries {
        if apply_entry(registry, entry, machine_id).await? {
            applied.push(entry.skill_id.clone());
        }
    }

    let conflicts = registry.conflicts().await?.len() as u32;
    let etag = pull.etag.or(push.etag).or(cursor);
    if let Some(etag) = &etag {
        registry.set_state("sync_etag", etag).await?;
    }
    registry
        .set_state("sync_last", &Utc::now().to_rfc3339())
        .await?;

    let message = if rejected > 0 {
        Some(format!(
            "{accepted} pushed, {rejected} rejected as stale or conflicting"
        ))
    } else {
        None
    };
    Ok(SyncReport {
        status: SkillSyncStatus {
            state: SkillSyncState::Synced,
            last_sync: Some(Utc::now()),
            pushed: accepted,
            pulled: applied.len() as u32,
            conflicts,
            etag,
            message,
            hint: None,
        },
        applied,
    })
}

/// Applies one pulled entry. Returns whether the local copy changed.
pub async fn apply_entry(
    registry: &Registry,
    entry: &SkillSyncEntry,
    machine_id: &str,
) -> Result<bool, SkillError> {
    let local = registry.get(&entry.skill_id).await.ok();
    let local_vector = local
        .as_ref()
        .map(|record| {
            let mut vector = SkillVersionVector::default();
            vector.clocks.insert(machine_id.to_owned(), record.revision);
            vector
        })
        .unwrap_or_default();

    if let Some(existing) = &local
        && existing.version == entry.version
        && existing.deleted == entry.deleted
    {
        return Ok(false);
    }

    let remote_newer = local
        .as_ref()
        .is_none_or(|existing| entry.updated_at >= existing.updated_at);
    let remote_wins = entry.vector.dominates(&local_vector)
        || (entry.vector.concurrent(&local_vector) && remote_newer);
    if !remote_wins {
        if let Some(existing) = &local {
            let bundle = if entry.deleted {
                None
            } else {
                Some(bundle_for(entry)?)
            };
            if let Some(bundle) = &bundle {
                registry
                    .record_conflict(
                        &existing.id,
                        &existing.name,
                        Some(&existing.version),
                        bundle,
                        &h4rn_protocol::SkillSource::Connect {
                            skill_id: entry.skill_id.clone(),
                            version: entry.version.clone(),
                        },
                        "the remote version is older or concurrent",
                        Utc::now(),
                    )
                    .await?;
            }
        }
        return Ok(false);
    }

    let revision = local.as_ref().map_or(1, |record| record.revision + 1);
    let record = SkillRecord {
        id: entry.skill_id.clone(),
        name: entry.name.clone(),
        description: entry.description.clone(),
        version: entry.version.clone(),
        source: h4rn_protocol::SkillSource::Connect {
            skill_id: entry.skill_id.clone(),
            version: entry.version.clone(),
        },
        manifest: entry.manifest.clone(),
        revision,
        created_at: local
            .as_ref()
            .map_or(entry.updated_at, |record| record.created_at),
        updated_at: entry.updated_at,
        deleted: entry.deleted,
    };
    let bundle = if entry.deleted {
        None
    } else {
        Some(bundle_for(entry)?)
    };
    registry.upsert_remote(bundle.as_ref(), &record).await?;
    // A tombstone is a change too: the caller must remove managed copies even
    // though there is no bundle to reinstall.
    Ok(true)
}

fn bundle_for(entry: &SkillSyncEntry) -> Result<SkillBundle, SkillError> {
    let bundle = SkillBundle::from_wire(entry.manifest.clone(), entry.files.clone())?;
    if bundle.version() != entry.version {
        return Err(SkillError::Conflict(format!(
            "synced bundle {} does not hash to {}",
            entry.skill_id, entry.version
        )));
    }
    Ok(bundle)
}

/// Folds sync acks into a user-facing message.
pub fn summarise(
    push: &SkillSyncPushResponse,
    pulled: usize,
    conflicts: &[SkillSyncConflict],
) -> String {
    format!(
        "{} accepted, {} rejected, {} applied, {} conflicts",
        push.accepted.len(),
        push.rejected.len(),
        pulled,
        conflicts.len()
    )
}

/// Convenience for tests and diagnostics: the last ack for one skill.
pub fn ack_for<'a>(acks: &'a [SkillSyncAck], skill_id: &str) -> Option<&'a SkillSyncAck> {
    acks.iter().find(|ack| ack.skill_id == skill_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{SKILL_FILE, read_bundle};
    use h4rn_protocol::{SkillManifest, SkillSyncPullResponse, SkillSyncPushResponse};
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    struct FakeApi {
        pushed: Mutex<Vec<SkillSyncEntry>>,
        pull: Mutex<SkillSyncPullResponse>,
    }

    #[async_trait]
    impl SkillSyncApi for FakeApi {
        async fn push(&self, request: SkillSyncPush) -> Result<SkillSyncPushResponse, SkillError> {
            let mut pushed = self.pushed.lock().unwrap();
            let accepted = request
                .entries
                .iter()
                .map(|entry| SkillSyncAck {
                    skill_id: entry.skill_id.clone(),
                    version: entry.version.clone(),
                    etag: Some("etag-1".into()),
                    accepted: true,
                    reason: None,
                })
                .collect();
            pushed.extend(request.entries);
            Ok(SkillSyncPushResponse {
                accepted,
                rejected: Vec::new(),
                conflicts: Vec::new(),
                etag: Some("etag-1".into()),
                server_time: Utc::now(),
            })
        }

        async fn pull(&self, _request: SkillSyncPull) -> Result<SkillSyncPullResponse, SkillError> {
            Ok(self.pull.lock().unwrap().clone())
        }
    }

    async fn registry() -> (tempfile::TempDir, Registry) {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::open(dir.path()).await.unwrap();
        (dir, registry)
    }

    fn skill_bundle(dir: &std::path::Path, name: &str, note: &str) -> SkillBundle {
        let root = dir.join(format!("source-{note}"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join(SKILL_FILE),
            format!("---\nname: {name}\ndescription: Demo\n---\n\n{note}\n"),
        )
        .unwrap();
        read_bundle(&root).unwrap()
    }

    fn empty_pull() -> SkillSyncPullResponse {
        SkillSyncPullResponse {
            entries: Vec::new(),
            conflicts: Vec::new(),
            etag: Some("etag-1".into()),
            server_time: Utc::now(),
        }
    }

    #[tokio::test]
    async fn pushes_local_versions() {
        let (_dir, registry) = registry().await;
        let source = tempfile::tempdir().unwrap();
        let bundle = skill_bundle(source.path(), "demo", "one");
        registry
            .insert(
                &bundle,
                &h4rn_protocol::SkillSource::LocalFolder { path: "/x".into() },
                Utc::now(),
            )
            .await
            .unwrap();
        let api = FakeApi {
            pushed: Mutex::new(Vec::new()),
            pull: Mutex::new(empty_pull()),
        };
        let report = sync_now(&registry, &api, "machine-a", None).await.unwrap();
        assert_eq!(report.status.state, SkillSyncState::Synced);
        assert_eq!(report.status.pushed, 1);
        assert_eq!(api.pushed.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_secret_blocks_the_push() {
        let (_dir, registry) = registry().await;
        let source = tempfile::tempdir().unwrap();
        let root = source.path().join("leaky");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join(SKILL_FILE),
            "---\nname: leaky\ndescription: Demo\n---\n\nghp_abcdefghijklmnopqrstuvwxyz0123456789\n",
        )
        .unwrap();
        let bundle = read_bundle(&root).unwrap();
        registry
            .insert(
                &bundle,
                &h4rn_protocol::SkillSource::LocalFolder { path: "/x".into() },
                Utc::now(),
            )
            .await
            .unwrap();
        let api = FakeApi {
            pushed: Mutex::new(Vec::new()),
            pull: Mutex::new(empty_pull()),
        };
        let report = sync_now(&registry, &api, "machine-a", None).await.unwrap();
        assert_eq!(report.status.state, SkillSyncState::Blocked);
        assert!(report.status.hint.unwrap().contains("SKILL.md"));
        assert!(api.pushed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn applies_a_remote_entry_and_rejects_a_stale_one() {
        let (_dir, registry) = registry().await;
        let source = tempfile::tempdir().unwrap();
        let bundle = skill_bundle(source.path(), "demo", "remote");
        let mut vector = SkillVersionVector::default();
        vector.clocks.insert("machine-b".into(), 4);
        let entry = SkillSyncEntry {
            skill_id: "sk_remote".into(),
            name: "demo".into(),
            description: "Demo".into(),
            version: bundle.version(),
            manifest: bundle.manifest.clone(),
            files: bundle.wire_files(),
            vector: vector.clone(),
            updated_at: Utc::now(),
            deleted: false,
            expected_etag: None,
        };
        assert!(apply_entry(&registry, &entry, "machine-a").await.unwrap());
        assert_eq!(registry.list().await.unwrap()[0].id, "sk_remote");

        let mut stale = entry.clone();
        let stale_bundle = skill_bundle(source.path(), "demo", "stale");
        stale.version = stale_bundle.version();
        stale.manifest = stale_bundle.manifest.clone();
        stale.files = stale_bundle.wire_files();
        stale.updated_at = Utc::now() - chrono::Duration::hours(1);
        let mut stale_vector = SkillVersionVector::default();
        stale_vector.clocks.insert("machine-b".into(), 3);
        stale.vector = stale_vector;
        assert!(!apply_entry(&registry, &stale, "machine-a").await.unwrap());
        assert_eq!(registry.conflicts().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn applying_a_tombstone_hides_the_skill() {
        let (_dir, registry) = registry().await;
        let source = tempfile::tempdir().unwrap();
        let bundle = skill_bundle(source.path(), "demo", "one");
        let record = registry
            .insert(
                &bundle,
                &h4rn_protocol::SkillSource::LocalFolder { path: "/x".into() },
                Utc::now(),
            )
            .await
            .unwrap();
        let mut vector = SkillVersionVector::default();
        vector.clocks.insert("machine-b".into(), 2);
        let entry = SkillSyncEntry {
            skill_id: record.id.clone(),
            name: record.name.clone(),
            description: record.description.clone(),
            version: record.version.clone(),
            manifest: record.manifest.clone(),
            files: Vec::new(),
            vector,
            updated_at: Utc::now() + chrono::Duration::seconds(1),
            deleted: true,
            expected_etag: None,
        };
        assert!(apply_entry(&registry, &entry, "machine-a").await.unwrap());
        assert!(matches!(
            registry.get(&record.id).await,
            Err(SkillError::NotFound(_))
        ));
    }

    #[test]
    fn manifest_round_trips_through_wire_files() {
        let manifest = SkillManifest {
            name: "demo".into(),
            description: "Demo".into(),
            total_bytes: 1,
            files: vec![h4rn_protocol::SkillFile {
                path: "SKILL.md".into(),
                size: 1,
                blake3: blake3::hash(b"x").to_hex().to_string(),
                executable: false,
            }],
        };
        let files = vec![h4rn_protocol::SkillBundleFile {
            path: "SKILL.md".into(),
            content: base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD_NO_PAD,
                b"x",
            ),
        }];
        let bundle = SkillBundle::from_wire(manifest, files).unwrap();
        assert_eq!(bundle.files.len(), 1);
        assert_eq!(bundle.wire_files().len(), 1);
    }

    #[test]
    fn an_empty_pull_has_a_cursor() {
        let pull = empty_pull();
        assert_eq!(pull.etag.as_deref(), Some("etag-1"));
        assert!(pull.entries.is_empty());
        let _: BTreeMap<String, i64> = BTreeMap::new();
    }
}
