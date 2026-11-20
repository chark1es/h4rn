//! Skills: one registry per user, content-addressed versions, per-harness
//! installers and cross-machine sync.
//!
//! The crate is deliberately split into small modules:
//! - [`frontmatter`] reads the flat YAML head of `SKILL.md`;
//! - [`model`] canonicalises a directory into a bundle and a version hash;
//! - [`store`] keeps metadata in SQLite and bytes under `skills/bundles`;
//! - [`install`] owns the four harness layouts and the `.h4rn-managed` marker;
//! - [`import`] previews and commits skills already on disk;
//! - [`secret`] refuses to upload credentials;
//! - [`sync`] speaks the connect push/pull protocol.
//!
//! [`SkillsService`] is the object the daemon holds. It is the only place that
//! mutates the registry and installed copies, so every change emits exactly one
//! `skills_changed` notification (ADR-034, ADR-035).

#![deny(missing_docs)]

pub mod error;
pub mod frontmatter;
pub mod import;
pub mod install;
pub mod model;
pub mod secret;
pub mod source;
pub mod store;
pub mod sync;

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use chrono::Utc;
use h4rn_protocol::{
    HarnessKind, HostId, SkillConflict, SkillConflictResolution, SkillDetail, SkillDiff,
    SkillImportPreview, SkillInstallState, SkillInventory, SkillSource, SkillSummary,
    SkillSyncState, SkillSyncStatus,
};
use tokio::sync::broadcast;

pub use error::SkillError;
pub use install::{ManagedMarker, SkillLayout, detect_layouts};
pub use model::{SkillBundle, read_bundle};
pub use sync::SkillSyncApi;

/// Crate name, used by skeleton tests and startup diagnostics.
pub const CRATE_NAME: &str = "h4rn-skills";

/// A change the daemon should forward to subscribed clients.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkillNotice {
    /// The registry or one installed copy changed.
    Changed,
    /// A sync finished or was blocked.
    Sync(SkillSyncStatus),
}

/// The skill service held in daemon `AppState`.
pub struct SkillsService {
    registry: store::Registry,
    layouts: Vec<SkillLayout>,
    machine_id: String,
    host: HostId,
    sync_api: Option<Arc<dyn SkillSyncApi>>,
    notify: broadcast::Sender<SkillNotice>,
    status: RwLock<SkillSyncStatus>,
}

impl std::fmt::Debug for SkillsService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SkillsService")
            .field("layouts", &self.layouts.len())
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}

impl SkillsService {
    /// Opens the registry under `data_dir` and detects the host layouts.
    pub async fn open(
        data_dir: &Path,
        host: HostId,
        sync_api: Option<Arc<dyn SkillSyncApi>>,
    ) -> Result<Self, SkillError> {
        let home = home_dir().ok_or_else(|| {
            SkillError::InvalidSource("could not resolve the user home directory".to_owned())
        })?;
        Self::with_layouts(data_dir, host, detect_layouts(&home, None), sync_api).await
    }

    /// Opens the registry with explicit layouts (tests and alternate homes).
    pub async fn with_layouts(
        data_dir: &Path,
        host: HostId,
        layouts: Vec<SkillLayout>,
        sync_api: Option<Arc<dyn SkillSyncApi>>,
    ) -> Result<Self, SkillError> {
        let registry = store::Registry::open(data_dir).await?;
        let machine_id = registry.machine_id().await?;
        let (notify, _) = broadcast::channel(64);
        Ok(Self {
            registry,
            layouts,
            machine_id,
            host,
            sync_api,
            notify,
            status: RwLock::new(SkillSyncStatus {
                state: SkillSyncState::Idle,
                last_sync: None,
                pushed: 0,
                pulled: 0,
                conflicts: 0,
                etag: None,
                message: None,
                hint: None,
            }),
        })
    }

    /// Subscribes to change notifications.
    pub fn subscribe(&self) -> broadcast::Receiver<SkillNotice> {
        self.notify.subscribe()
    }

    /// The host id carried by `skills_changed`.
    pub fn host(&self) -> &HostId {
        &self.host
    }

    /// The stable machine id used in version vectors.
    pub fn machine_id(&self) -> &str {
        &self.machine_id
    }

    /// Current sync status.
    pub fn sync_status(&self) -> SkillSyncStatus {
        self.status
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The registry and the install matrix.
    pub async fn list(&self) -> Result<SkillInventory, SkillError> {
        let records = self.registry.list().await?;
        let installs = self.registry.installs(&self.machine_id).await?;
        let conflicts = self.registry.conflicts().await?;
        let mut skills = Vec::with_capacity(records.len());
        for record in &records {
            let skill_conflicts = conflicts
                .iter()
                .filter(|conflict| conflict.conflict.skill_id == record.id)
                .count() as u32;
            let mut states = Vec::with_capacity(self.layouts.len());
            for layout in &self.layouts {
                states.push(self.install_state(layout, record, &installs));
            }
            skills.push(record.summary(states, skill_conflicts));
        }
        Ok(SkillInventory {
            skills,
            harnesses: self
                .layouts
                .iter()
                .map(|layout| h4rn_protocol::SkillHarnessAvailability {
                    harness: layout.harness,
                    detected: layout.detected,
                    user_root: layout
                        .user_root
                        .as_ref()
                        .map(|path| path.display().to_string()),
                    project_root: layout
                        .project_root
                        .as_ref()
                        .map(|path| path.display().to_string()),
                })
                .collect(),
        })
    }

    fn install_state(
        &self,
        layout: &SkillLayout,
        record: &store::SkillRecord,
        installs: &[store::InstallRow],
    ) -> SkillInstallState {
        let row = installs
            .iter()
            .find(|row| row.skill_id == record.id && row.harness == layout.harness);
        let candidate = row
            .filter(|row| !row.path.is_empty())
            .map(|row| PathBuf::from(&row.path))
            .or_else(|| {
                let name = install::skill_dir_name(&record.name).ok()?;
                layout.user_root.as_ref().map(|root| root.join(name))
            });
        let marker = candidate.as_deref().and_then(install::read_marker);
        let managed = marker
            .as_ref()
            .is_some_and(|marker| marker.skill_id == record.id);
        let installed = candidate.as_deref().is_some_and(|path| path.is_dir())
            && (managed
                || layout.user_root.as_ref().is_some_and(|root| {
                    candidate
                        .as_deref()
                        .is_some_and(|path| path.starts_with(root))
                }));
        SkillInstallState {
            harness: layout.harness,
            installed,
            managed,
            enabled: row.is_some_and(|row| row.enabled),
            version: marker.as_ref().map(|marker| marker.version.clone()),
            path: installed.then(|| {
                candidate
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default()
            }),
        }
    }

    /// Adds a skill and installs it into the selected harnesses.
    pub async fn add(
        &self,
        source: &SkillSource,
        harnesses: &[HarnessKind],
        name_override: Option<&str>,
    ) -> Result<SkillSummary, SkillError> {
        let mut loaded = source::load_source_async(source).await?;
        if let Some(name) = name_override.filter(|name| !name.trim().is_empty()) {
            model::rename_bundle(&mut loaded.bundle, name)?;
        }
        if loaded.bundle.manifest.name.trim().is_empty() {
            return Err(SkillError::InvalidSkill {
                path: "SKILL.md".to_owned(),
                reason: "the skill has no name".to_owned(),
            });
        }
        for harness in harnesses {
            if self.layout(*harness).is_none() {
                return Err(SkillError::InvalidSource(format!(
                    "{harness:?} has no skill directory on this host"
                )));
            }
        }
        let now = Utc::now();
        let version = loaded.bundle.version();
        let record = match self
            .registry
            .find_by_name(&loaded.bundle.manifest.name)
            .await?
        {
            Some(existing) if existing.version == version => existing,
            Some(existing) => {
                self.registry
                    .record_conflict(
                        &existing.id,
                        &existing.name,
                        Some(&existing.version),
                        &loaded.bundle,
                        &loaded.source,
                        "a different version already uses this name",
                        now,
                    )
                    .await?;
                self.emit_changed();
                return Err(SkillError::Conflict(format!(
                    "{} already exists at version {}",
                    existing.name, existing.version
                )));
            }
            None => {
                self.registry
                    .insert(&loaded.bundle, &loaded.source, now)
                    .await?
            }
        };

        for harness in harnesses {
            self.install_into(*harness, &record, &loaded.bundle).await?;
        }
        self.emit_changed();
        let installs = self.registry.installs(&self.machine_id).await?;
        let conflicts = self.registry.conflicts().await?;
        self.summary(&record, &installs, &conflicts)
    }

    /// Replaces the current version and refreshes managed copies.
    pub async fn update(
        &self,
        skill_id: &str,
        source: &SkillSource,
    ) -> Result<SkillSummary, SkillError> {
        let existing = self.registry.get(skill_id).await?;
        let loaded = source::load_source_async(source).await?;
        if loaded.bundle.version() == existing.version {
            return self.summary_now(&existing).await;
        }
        let record = self
            .registry
            .update(skill_id, &loaded.bundle, &loaded.source, Utc::now())
            .await?;
        self.reinstall_enabled(&record, &loaded.bundle).await?;
        self.emit_changed();
        self.summary_now(&record).await
    }

    /// Removes a skill and only its managed copies.
    pub async fn remove(&self, skill_id: &str) -> Result<(), SkillError> {
        let record = self.registry.get(skill_id).await?;
        self.uninstall_managed(&record.id).await?;
        self.registry.remove(&record.id, Utc::now()).await?;
        self.emit_changed();
        Ok(())
    }

    /// Removes every managed copy of one skill on this machine.
    async fn uninstall_managed(&self, skill_id: &str) -> Result<(), SkillError> {
        let installs = self.registry.installs(&self.machine_id).await?;
        for row in installs.iter().filter(|row| row.skill_id == skill_id) {
            if let Some(layout) = self.layout(row.harness)
                && let Some(root) = &layout.user_root
            {
                match install::uninstall_bundle(root, row.harness, skill_id) {
                    Ok(_) => {}
                    Err(error) => tracing::warn!(
                        skill = %skill_id,
                        harness = ?row.harness,
                        %error,
                        "could not remove an installed copy"
                    ),
                }
            }
            self.registry
                .clear_install(skill_id, row.harness, &self.machine_id)
                .await?;
        }
        Ok(())
    }

    /// Enables or disables one harness for one skill.
    pub async fn set_enabled(
        &self,
        skill_id: &str,
        harness: HarnessKind,
        enabled: bool,
    ) -> Result<SkillSummary, SkillError> {
        let record = self.registry.get(skill_id).await?;
        let layout = self
            .layout(harness)
            .cloned()
            .ok_or_else(|| SkillError::InvalidSource(format!("{harness:?} is not installed")))?;
        let root = layout.user_root.as_ref().ok_or_else(|| {
            SkillError::InvalidSource(format!("{harness:?} has no user skill directory"))
        })?;
        let now = Utc::now();
        if enabled {
            let bundle = self.registry.bundle(&record.version).await?;
            let copy = install::install_bundle(root, harness, &record.id, &bundle)?;
            self.registry
                .set_install(
                    &record.id,
                    harness,
                    &self.machine_id,
                    &copy.path.display().to_string(),
                    true,
                    now,
                )
                .await?;
        } else {
            let outcome = install::uninstall_bundle(root, harness, &record.id)?;
            let path = outcome
                .path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default();
            self.registry
                .set_install(&record.id, harness, &self.machine_id, &path, false, now)
                .await?;
        }
        self.emit_changed();
        self.summary_now(&record).await
    }

    /// Scans harness directories for importable skills.
    pub async fn import_preview(&self) -> Result<SkillImportPreview, SkillError> {
        Ok(import::scan(&self.layouts, &self.registry).await?.preview)
    }

    /// Imports the selected candidates. With `manage`, each imported or
    /// already-present folder whose files match the registry version is
    /// adopted: h4rn writes its marker there and records the install, so sync
    /// keeps the copy current. Returns how many candidates were imported plus
    /// adopted.
    pub async fn import_commit(
        &self,
        candidate_ids: &[String],
        manage: bool,
    ) -> Result<u32, SkillError> {
        let now = Utc::now();
        let outcomes = import::commit(&self.layouts, &self.registry, candidate_ids, now).await?;
        let mut count = 0u32;
        for outcome in &outcomes {
            let (skill_id, target, added) = match outcome {
                import::CommitOutcome::Added { skill_id, target } => (skill_id, target, true),
                import::CommitOutcome::Duplicate { skill_id, target } => (skill_id, target, false),
                import::CommitOutcome::Conflict(_) => continue,
            };
            let adopted = manage && self.adopt(skill_id, target, now).await?;
            if added || adopted {
                count += 1;
            }
        }
        self.emit_changed();
        Ok(count)
    }

    /// Adopts one imported folder; returns whether h4rn now manages it.
    async fn adopt(
        &self,
        skill_id: &str,
        target: &import::ImportTarget,
        now: chrono::DateTime<Utc>,
    ) -> Result<bool, SkillError> {
        let Some(root) = self
            .layout(target.harness)
            .and_then(|layout| layout.user_root.clone())
        else {
            return Ok(false);
        };
        let record = self.registry.get(skill_id).await?;
        let outcome = install::adopt_folder(
            &root,
            &target.path,
            target.harness,
            &record.id,
            &record.name,
            &record.version,
        )?;
        if outcome != install::AdoptOutcome::Adopted {
            tracing::info!(
                skill = %record.name,
                path = %target.path.display(),
                ?outcome,
                "left a harness skill folder unmanaged"
            );
            return Ok(false);
        }
        // Every harness that reads this root shares the copy (Codex and
        // OpenCode can point at the same folder), so record each of them.
        for layout in &self.layouts {
            if layout.user_root.as_deref() != Some(root.as_path()) {
                continue;
            }
            if layout.harness != target.harness {
                install::adopt_folder(
                    &root,
                    &target.path,
                    layout.harness,
                    &record.id,
                    &record.name,
                    &record.version,
                )?;
            }
            self.registry
                .set_install(
                    &record.id,
                    layout.harness,
                    &self.machine_id,
                    &target.path.display().to_string(),
                    true,
                    now,
                )
                .await?;
        }
        Ok(true)
    }

    /// Conflicts the UI can render.
    pub async fn conflicts(&self) -> Result<Vec<SkillConflict>, SkillError> {
        Ok(self
            .registry
            .conflicts()
            .await?
            .into_iter()
            .map(|row| row.conflict)
            .collect())
    }

    /// Applies one conflict resolution.
    pub async fn resolve_conflict(
        &self,
        conflict_id: &str,
        resolution: SkillConflictResolution,
    ) -> Result<(), SkillError> {
        let row = self.registry.conflict(conflict_id).await?;
        if row.resolved {
            return Ok(());
        }
        let candidate = self.registry.bundle(&row.candidate_version).await?;
        let now = Utc::now();
        match resolution {
            SkillConflictResolution::KeepLocal => {}
            SkillConflictResolution::KeepRemote => {
                match self.registry.get(&row.conflict.skill_id).await {
                    Ok(existing) => {
                        let record = self
                            .registry
                            .update(&existing.id, &candidate, &row.candidate_source, now)
                            .await?;
                        self.reinstall_enabled(&record, &candidate).await?;
                    }
                    Err(SkillError::NotFound(_)) => {
                        self.registry
                            .insert(&candidate, &row.candidate_source, now)
                            .await?;
                    }
                    Err(error) => return Err(error),
                }
            }
            SkillConflictResolution::KeepBoth => {
                let suffix: String = row.candidate_version.chars().take(6).collect();
                let mut renamed = candidate.clone();
                model::rename_bundle(
                    &mut renamed,
                    &format!("{}-{suffix}", candidate.manifest.name),
                )?;
                self.registry
                    .insert(&renamed, &row.candidate_source, now)
                    .await?;
            }
        }
        self.registry.resolve_conflict(conflict_id, now).await?;
        self.emit_changed();
        Ok(())
    }

    /// Diffs two stored versions.
    pub async fn diff(
        &self,
        skill_id: &str,
        from_version: Option<&str>,
        to_version: Option<&str>,
    ) -> Result<SkillDiff, SkillError> {
        let record = self.registry.get(skill_id).await?;
        let to_version = to_version.unwrap_or(&record.version);
        let to = self.registry.bundle(to_version).await?;
        let from = match from_version {
            Some(version) => Some(self.registry.bundle(version).await?),
            None => None,
        };
        Ok(model::diff_manifests(
            skill_id,
            from.as_ref().map(|bundle| &bundle.manifest),
            &to.manifest,
            to_version,
        ))
    }

    /// The full manifest for one skill.
    pub async fn detail(&self, skill_id: &str) -> Result<SkillDetail, SkillError> {
        let record = self.registry.get(skill_id).await?;
        let manifest = self.registry.bundle(&record.version).await?.manifest;
        let summary = self.summary_now(&record).await?;
        Ok(SkillDetail { summary, manifest })
    }

    /// Pushes and pulls through connect.
    pub async fn sync_now(&self) -> Result<SkillSyncStatus, SkillError> {
        let Some(api) = self.sync_api.clone() else {
            let status = SkillSyncStatus {
                state: SkillSyncState::Offline,
                last_sync: self.sync_status().last_sync,
                pushed: 0,
                pulled: 0,
                conflicts: self.registry.conflicts().await?.len() as u32,
                etag: self.sync_status().etag,
                message: Some("the daemon is not signed in to connect".to_owned()),
                hint: Some("Run `h4rnd login --connect` on this machine".to_owned()),
            };
            self.set_status(status.clone());
            self.emit_sync(status.clone());
            return Ok(status);
        };

        self.set_status(SkillSyncStatus {
            state: SkillSyncState::Syncing,
            ..self.sync_status()
        });
        let cursor = self.registry.state("sync_etag").await?;
        let machine_id = self.machine_id.clone();
        let report = sync::sync_now(&self.registry, api.as_ref(), &machine_id, cursor).await;
        let status = match report {
            Ok(report) => {
                for skill_id in &report.applied {
                    match self.registry.get(skill_id).await {
                        Ok(record) => {
                            if let Ok(bundle) = self.registry.bundle(&record.version).await {
                                let _ = self.reinstall_enabled(&record, &bundle).await;
                            }
                        }
                        // The entry was a tombstone: the registry row is gone,
                        // so only the managed copies remain to clean up.
                        Err(SkillError::NotFound(_)) => {
                            let _ = self.uninstall_managed(skill_id).await;
                        }
                        Err(error) => tracing::warn!(
                            skill = %skill_id,
                            %error,
                            "could not read a synced skill"
                        ),
                    }
                }
                report.status
            }
            Err(error) => {
                let proto = error.into_proto();
                SkillSyncStatus {
                    state: SkillSyncState::Offline,
                    last_sync: self.sync_status().last_sync,
                    pushed: 0,
                    pulled: 0,
                    conflicts: self.registry.conflicts().await?.len() as u32,
                    etag: self.sync_status().etag,
                    message: Some(proto.message),
                    hint: proto.hint,
                }
            }
        };
        self.set_status(status.clone());
        self.emit_sync(status.clone());
        Ok(status)
    }

    async fn summary_now(&self, record: &store::SkillRecord) -> Result<SkillSummary, SkillError> {
        let installs = self.registry.installs(&self.machine_id).await?;
        let conflicts = self.registry.conflicts().await?;
        self.summary(record, &installs, &conflicts)
    }

    fn summary(
        &self,
        record: &store::SkillRecord,
        installs: &[store::InstallRow],
        conflicts: &[store::ConflictRow],
    ) -> Result<SkillSummary, SkillError> {
        let count = conflicts
            .iter()
            .filter(|conflict| conflict.conflict.skill_id == record.id)
            .count() as u32;
        let states = self
            .layouts
            .iter()
            .map(|layout| self.install_state(layout, record, installs))
            .collect();
        Ok(record.summary(states, count))
    }

    async fn install_into(
        &self,
        harness: HarnessKind,
        record: &store::SkillRecord,
        bundle: &SkillBundle,
    ) -> Result<bool, SkillError> {
        let layout = self.layout(harness).ok_or_else(|| {
            SkillError::InvalidSource(format!("{harness:?} has no skill directory"))
        })?;
        let root = layout.user_root.as_ref().ok_or_else(|| {
            SkillError::InvalidSource(format!("{harness:?} has no user skill directory"))
        })?;
        let copy = install::install_bundle(root, harness, &record.id, bundle)?;
        self.registry
            .set_install(
                &record.id,
                harness,
                &self.machine_id,
                &copy.path.display().to_string(),
                true,
                Utc::now(),
            )
            .await?;
        Ok(true)
    }

    async fn reinstall_enabled(
        &self,
        record: &store::SkillRecord,
        bundle: &SkillBundle,
    ) -> Result<(), SkillError> {
        let installs = self.registry.installs(&self.machine_id).await?;
        for row in installs
            .iter()
            .filter(|row| row.skill_id == record.id && row.enabled)
        {
            let Some(layout) = self.layout(row.harness) else {
                continue;
            };
            let Some(root) = &layout.user_root else {
                continue;
            };
            match install::install_bundle(root, row.harness, &record.id, bundle) {
                Ok(copy) => {
                    self.registry
                        .set_install(
                            &record.id,
                            row.harness,
                            &self.machine_id,
                            &copy.path.display().to_string(),
                            true,
                            Utc::now(),
                        )
                        .await?;
                }
                Err(SkillError::UserOwned { path }) => {
                    tracing::warn!(%path, "refusing to replace a user-owned skill copy");
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn layout(&self, harness: HarnessKind) -> Option<&SkillLayout> {
        self.layouts.iter().find(|layout| layout.harness == harness)
    }

    fn set_status(&self, status: SkillSyncStatus) {
        *self
            .status
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = status;
    }

    fn emit_changed(&self) {
        let _ = self.notify.send(SkillNotice::Changed);
    }

    fn emit_sync(&self, status: SkillSyncStatus) {
        let _ = self.notify.send(SkillNotice::Sync(status));
    }
}

/// Creates a directory with private permissions on Unix.
pub(crate) fn prepare_private_dir(path: &Path) -> Result<(), SkillError> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::SkillLayout;

    fn layout(harness: HarnessKind, root: PathBuf) -> SkillLayout {
        SkillLayout {
            harness,
            user_root: Some(root),
            project_root: None,
            shared_project_root: false,
            detected: true,
        }
    }

    async fn service() -> (tempfile::TempDir, SkillsService) {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let layouts = vec![
            layout(
                HarnessKind::ClaudeCode,
                home.path().join("claude").join("skills"),
            ),
            layout(HarnessKind::Codex, home.path().join("codex").join("skills")),
        ];
        // The temp dirs must outlive the service; leak them in the test process.
        std::mem::forget(home);
        let service =
            SkillsService::with_layouts(dir.path(), HostId::new("host_test"), layouts, None)
                .await
                .unwrap();
        (dir, service)
    }

    fn make_skill(root: &Path, name: &str, note: &str) -> PathBuf {
        let dir = root.join(format!("{name}-{note}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: Demo {note}\n---\n\n{note}\n"),
        )
        .unwrap();
        dir
    }

    fn claude_root(service: &SkillsService) -> PathBuf {
        service
            .layout(HarnessKind::ClaudeCode)
            .and_then(|layout| layout.user_root.clone())
            .unwrap()
    }

    fn write_skill(dir: &Path, name: &str, note: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: Demo {note}\n---\n\n{note}\n"),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn import_with_manage_adopts_the_harness_folder() {
        let (_dir, service) = service().await;
        let root = claude_root(&service);
        let folder = root.join("lint");
        write_skill(&folder, "lint", "one");

        let preview = service.import_preview().await.unwrap();
        let candidate = &preview.candidates[0];
        assert!(!candidate.managed);
        let count = service
            .import_commit(std::slice::from_ref(&candidate.id), true)
            .await
            .unwrap();
        assert_eq!(count, 1);

        let marker = install::read_marker(&folder).expect("adopted folder carries the marker");
        let inventory = service.list().await.unwrap();
        let skill = &inventory.skills[0];
        assert_eq!(marker.skill_id, skill.id);
        let claude = skill
            .installs
            .iter()
            .find(|state| state.harness == HarnessKind::ClaudeCode)
            .unwrap();
        assert!(claude.managed && claude.enabled && claude.installed);
        // The skill's own files are untouched.
        assert_eq!(
            std::fs::read_to_string(folder.join("SKILL.md")).unwrap(),
            "---\nname: lint\ndescription: Demo one\n---\n\none\n"
        );
        // A rescan reports it as managed.
        let rescan = service.import_preview().await.unwrap();
        assert!(rescan.candidates[0].managed && rescan.candidates[0].duplicate);
    }

    #[tokio::test]
    async fn import_without_manage_leaves_the_folder_user_owned() {
        let (_dir, service) = service().await;
        let folder = claude_root(&service).join("lint");
        write_skill(&folder, "lint", "one");
        let preview = service.import_preview().await.unwrap();
        service
            .import_commit(std::slice::from_ref(&preview.candidates[0].id), false)
            .await
            .unwrap();
        assert!(install::read_marker(&folder).is_none());
    }

    #[tokio::test]
    async fn a_duplicate_can_be_adopted_later_but_a_renamed_folder_cannot() {
        let (_dir, service) = service().await;
        let root = claude_root(&service);
        write_skill(&root.join("lint"), "lint", "one");
        write_skill(&root.join("my-review"), "review", "two");
        let preview = service.import_preview().await.unwrap();
        let ids: Vec<String> = preview.candidates.iter().map(|c| c.id.clone()).collect();
        service.import_commit(&ids, false).await.unwrap();

        let again = service.import_preview().await.unwrap();
        assert!(again.candidates.iter().all(|c| c.duplicate && !c.managed));
        let ids: Vec<String> = again.candidates.iter().map(|c| c.id.clone()).collect();
        let adopted = service.import_commit(&ids, true).await.unwrap();
        // "lint" sits where h4rn installs it; "my-review" does not match its
        // skill name, so a later install would not replace it.
        assert_eq!(adopted, 1);
        assert!(install::read_marker(&root.join("lint")).is_some());
        assert!(install::read_marker(&root.join("my-review")).is_none());
    }

    #[tokio::test]
    async fn adds_lists_toggles_and_removes_a_skill() {
        let (_dir, service) = service().await;
        let source = tempfile::tempdir().unwrap();
        let root = make_skill(source.path(), "demo", "one");
        let summary = service
            .add(
                &SkillSource::LocalFolder {
                    path: root.display().to_string(),
                },
                &[HarnessKind::ClaudeCode],
                None,
            )
            .await
            .unwrap();
        assert_eq!(summary.name, "demo");
        assert!(summary.installs[0].installed);
        assert!(summary.installs[0].managed);
        assert!(summary.installs[0].enabled);
        assert!(!summary.installs[1].installed);

        let inventory = service.list().await.unwrap();
        assert_eq!(inventory.skills.len(), 1);
        assert_eq!(inventory.harnesses.len(), 2);

        let disabled = service
            .set_enabled(&summary.id, HarnessKind::ClaudeCode, false)
            .await
            .unwrap();
        assert!(!disabled.installs[0].installed);
        assert!(!disabled.installs[0].enabled);

        service.remove(&summary.id).await.unwrap();
        assert!(service.list().await.unwrap().skills.is_empty());
    }

    #[tokio::test]
    async fn updates_reinstall_managed_copies() {
        let (_dir, service) = service().await;
        let source = tempfile::tempdir().unwrap();
        let root = make_skill(source.path(), "demo", "one");
        let summary = service
            .add(
                &SkillSource::LocalFolder {
                    path: root.display().to_string(),
                },
                &[HarnessKind::Codex],
                None,
            )
            .await
            .unwrap();
        let new_root = make_skill(source.path(), "demo", "two");
        let updated = service
            .update(
                &summary.id,
                &SkillSource::LocalFolder {
                    path: new_root.display().to_string(),
                },
            )
            .await
            .unwrap();
        assert_ne!(updated.version, summary.version);
        assert!(updated.installs[1].installed);
    }

    #[tokio::test]
    async fn a_conflicting_add_is_surfaced() {
        let (_dir, service) = service().await;
        let source = tempfile::tempdir().unwrap();
        let first = make_skill(source.path(), "demo", "one");
        let second = make_skill(source.path(), "demo", "two");
        service
            .add(
                &SkillSource::LocalFolder {
                    path: first.display().to_string(),
                },
                &[],
                None,
            )
            .await
            .unwrap();
        let error = service
            .add(
                &SkillSource::LocalFolder {
                    path: second.display().to_string(),
                },
                &[],
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, SkillError::Conflict(_)));
        assert_eq!(service.conflicts().await.unwrap().len(), 1);
    }
}
