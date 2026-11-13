//! SQLite metadata and the content-addressed bundle directory.
//!
//! There is one registry per user in `<data_dir>/skills`. SQLite holds names,
//! versions, install state, conflicts and sync cursors; bundle bytes live under
//! `<data_dir>/skills/bundles/<version>` so an identical version is stored once
//! no matter how many harnesses, imports or machines reference it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use h4rn_protocol::{HarnessKind, SkillConflict, SkillManifest, SkillSource};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{FromRow, Row, SqlitePool};

use crate::error::SkillError;
use crate::model::{SkillBundle, bundle_root, read_bundle, write_bundle};

/// One registry row.
#[derive(Clone, Debug, PartialEq)]
pub struct SkillRecord {
    /// Registry id.
    pub id: String,
    /// Frontmatter name.
    pub name: String,
    /// Frontmatter description.
    pub description: String,
    /// Current version hash.
    pub version: String,
    /// Source of the current version.
    pub source: SkillSource,
    /// Canonical manifest of the current version.
    pub manifest: SkillManifest,
    /// Monotonic local revision, used as the version-vector clock.
    pub revision: i64,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last change.
    pub updated_at: DateTime<Utc>,
    /// Tombstone written by a sync.
    pub deleted: bool,
}

/// The installed copy of one skill for one harness.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallRow {
    /// Registry id.
    pub skill_id: String,
    /// Harness.
    pub harness: HarnessKind,
    /// Machine that owns the copy.
    pub machine_id: String,
    /// Absolute install directory.
    pub path: String,
    /// Enabled flag.
    pub enabled: bool,
    /// Last change.
    pub updated_at: DateTime<Utc>,
}

/// A conflict plus the candidate data needed to resolve it.
#[derive(Clone, Debug, PartialEq)]
pub struct ConflictRow {
    /// Wire-facing conflict.
    pub conflict: SkillConflict,
    /// Candidate bundle version, already stored content-addressed.
    pub candidate_version: String,
    /// Candidate source, applied when the user keeps the remote version.
    pub candidate_source: SkillSource,
    /// Whether the conflict has been resolved.
    pub resolved: bool,
}

#[derive(Debug, FromRow)]
struct SkillRow {
    id: String,
    name: String,
    description: String,
    latest_version: String,
    source_json: String,
    manifest_json: String,
    revision: i64,
    created_at: String,
    updated_at: String,
    deleted: i64,
}

#[derive(Debug, FromRow)]
struct InstallDbRow {
    skill_id: String,
    harness: String,
    machine_id: String,
    path: String,
    enabled: i64,
    updated_at: String,
}

#[derive(Debug, FromRow)]
struct ConflictDbRow {
    id: String,
    skill_id: String,
    name: String,
    local_version: Option<String>,
    remote_version: String,
    candidate_version: String,
    candidate_source_json: String,
    reason: String,
    detected_at: String,
    resolved_at: Option<String>,
}

/// The metadata registry.
#[derive(Clone, Debug)]
pub struct Registry {
    pool: SqlitePool,
    bundles: PathBuf,
}

impl Registry {
    /// Opens or creates the registry under `data_dir`.
    pub async fn open(data_dir: &Path) -> Result<Self, SkillError> {
        let skills_dir = data_dir.join("skills");
        crate::prepare_private_dir(&skills_dir)?;
        let db_path = skills_dir.join("registry.db");
        let options = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        Self::from_pool(pool, bundle_root(data_dir)).await
    }

    /// Opens an in-memory registry for tests.
    pub async fn in_memory(bundles: PathBuf) -> Result<Self, SkillError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        Self::from_pool(pool, bundles).await
    }

    async fn from_pool(pool: SqlitePool, bundles: PathBuf) -> Result<Self, SkillError> {
        crate::prepare_private_dir(&bundles)?;
        let registry = Self { pool, bundles };
        registry.ensure_schema().await?;
        Ok(registry)
    }

    async fn ensure_schema(&self) -> Result<(), SkillError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS skill_skills (
                id              TEXT PRIMARY KEY,
                name            TEXT NOT NULL,
                description     TEXT NOT NULL,
                latest_version  TEXT NOT NULL,
                source_json     TEXT NOT NULL,
                manifest_json   TEXT NOT NULL,
                revision        INTEGER NOT NULL DEFAULT 1,
                created_at      TEXT NOT NULL,
                updated_at      TEXT NOT NULL,
                deleted         INTEGER NOT NULL DEFAULT 0
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_skill_skills_name
             ON skill_skills (name) WHERE deleted = 0",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS skill_versions (
                skill_id       TEXT NOT NULL,
                version        TEXT NOT NULL,
                manifest_json  TEXT NOT NULL,
                created_at     TEXT NOT NULL,
                PRIMARY KEY (skill_id, version)
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS skill_installs (
                skill_id   TEXT NOT NULL,
                harness    TEXT NOT NULL,
                machine_id TEXT NOT NULL,
                path       TEXT NOT NULL,
                enabled    INTEGER NOT NULL DEFAULT 1,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (skill_id, harness, machine_id)
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS skill_conflicts (
                id                    TEXT PRIMARY KEY,
                skill_id              TEXT NOT NULL,
                name                  TEXT NOT NULL,
                local_version         TEXT,
                remote_version        TEXT NOT NULL,
                candidate_version     TEXT NOT NULL,
                candidate_source_json TEXT NOT NULL,
                reason                TEXT NOT NULL,
                detected_at           TEXT NOT NULL,
                resolved_at           TEXT
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS skill_state (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Every live registry row, sorted by name.
    pub async fn list(&self) -> Result<Vec<SkillRecord>, SkillError> {
        let rows: Vec<SkillRow> =
            sqlx::query_as("SELECT * FROM skill_skills WHERE deleted = 0 ORDER BY lower(name), id")
                .fetch_all(&self.pool)
                .await?;
        rows.into_iter().map(Self::record_from_row).collect()
    }

    /// Every row including tombstones, for sync.
    pub async fn list_for_sync(&self) -> Result<Vec<SkillRecord>, SkillError> {
        let rows: Vec<SkillRow> =
            sqlx::query_as("SELECT * FROM skill_skills ORDER BY lower(name), id")
                .fetch_all(&self.pool)
                .await?;
        rows.into_iter().map(Self::record_from_row).collect()
    }

    /// One live row by id.
    pub async fn get(&self, id: &str) -> Result<SkillRecord, SkillError> {
        let row: Option<SkillRow> =
            sqlx::query_as("SELECT * FROM skill_skills WHERE id = ? AND deleted = 0")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        row.map(Self::record_from_row)
            .transpose()?
            .ok_or_else(|| SkillError::NotFound(id.to_owned()))
    }

    /// One live row by name.
    pub async fn find_by_name(&self, name: &str) -> Result<Option<SkillRecord>, SkillError> {
        let row: Option<SkillRow> =
            sqlx::query_as("SELECT * FROM skill_skills WHERE name = ? AND deleted = 0")
                .bind(name)
                .fetch_optional(&self.pool)
                .await?;
        row.map(Self::record_from_row).transpose()
    }

    /// Inserts a new skill row and stores its bundle.
    pub async fn insert(
        &self,
        bundle: &SkillBundle,
        source: &SkillSource,
        now: DateTime<Utc>,
    ) -> Result<SkillRecord, SkillError> {
        let version = self.store_version(bundle, now).await?;
        let id = format!("sk_{}", &version[..16]);
        let manifest_json = serde_json::to_string(&bundle.manifest)?;
        let source_json = serde_json::to_string(source)?;
        sqlx::query(
            "INSERT INTO skill_skills
                (id, name, description, latest_version, source_json, manifest_json, revision, created_at, updated_at, deleted)
             VALUES (?, ?, ?, ?, ?, ?, 1, ?, ?, 0)",
        )
        .bind(&id)
        .bind(&bundle.manifest.name)
        .bind(&bundle.manifest.description)
        .bind(&version)
        .bind(source_json)
        .bind(manifest_json)
        .bind(now.to_rfc3339())
        .bind(now.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(SkillRecord {
            id,
            name: bundle.manifest.name.clone(),
            description: bundle.manifest.description.clone(),
            version,
            source: source.clone(),
            manifest: bundle.manifest.clone(),
            revision: 1,
            created_at: now,
            updated_at: now,
            deleted: false,
        })
    }

    /// Replaces the current version of an existing row.
    pub async fn update(
        &self,
        id: &str,
        bundle: &SkillBundle,
        source: &SkillSource,
        now: DateTime<Utc>,
    ) -> Result<SkillRecord, SkillError> {
        let existing = self.get(id).await?;
        let version = self.store_version(bundle, now).await?;
        let manifest_json = serde_json::to_string(&bundle.manifest)?;
        let source_json = serde_json::to_string(source)?;
        let revision = existing.revision + 1;
        sqlx::query(
            "UPDATE skill_skills
                SET name = ?, description = ?, latest_version = ?, source_json = ?,
                    manifest_json = ?, revision = ?, updated_at = ?
              WHERE id = ?",
        )
        .bind(&bundle.manifest.name)
        .bind(&bundle.manifest.description)
        .bind(&version)
        .bind(source_json)
        .bind(manifest_json)
        .bind(revision)
        .bind(now.to_rfc3339())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(SkillRecord {
            id: id.to_owned(),
            name: bundle.manifest.name.clone(),
            description: bundle.manifest.description.clone(),
            version,
            source: source.clone(),
            manifest: bundle.manifest.clone(),
            revision,
            created_at: existing.created_at,
            updated_at: now,
            deleted: false,
        })
    }

    /// Applies a remote record read from a pull.
    pub async fn upsert_remote(
        &self,
        bundle: Option<&SkillBundle>,
        record: &SkillRecord,
    ) -> Result<(), SkillError> {
        if let Some(bundle) = bundle {
            self.store_version(bundle, record.updated_at).await?;
        }
        let manifest_json = serde_json::to_string(&record.manifest)?;
        let source_json = serde_json::to_string(&record.source)?;
        sqlx::query(
            "INSERT INTO skill_skills
                (id, name, description, latest_version, source_json, manifest_json, revision, created_at, updated_at, deleted)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                description = excluded.description,
                latest_version = excluded.latest_version,
                source_json = excluded.source_json,
                manifest_json = excluded.manifest_json,
                revision = MAX(skill_skills.revision, excluded.revision),
                updated_at = excluded.updated_at,
                deleted = excluded.deleted",
        )
        .bind(&record.id)
        .bind(&record.name)
        .bind(&record.description)
        .bind(&record.version)
        .bind(source_json)
        .bind(manifest_json)
        .bind(record.revision)
        .bind(record.created_at.to_rfc3339())
        .bind(record.updated_at.to_rfc3339())
        .bind(if record.deleted { 1i64 } else { 0i64 })
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Marks a row deleted without dropping its versions.
    pub async fn remove(&self, id: &str, now: DateTime<Utc>) -> Result<SkillRecord, SkillError> {
        let existing = self.get(id).await?;
        sqlx::query(
            "UPDATE skill_skills SET deleted = 1, revision = revision + 1, updated_at = ? WHERE id = ?",
        )
        .bind(now.to_rfc3339())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(SkillRecord {
            revision: existing.revision + 1,
            updated_at: now,
            deleted: true,
            ..existing
        })
    }

    /// Reads one stored bundle by version.
    pub async fn bundle(&self, version: &str) -> Result<SkillBundle, SkillError> {
        let directory = self.bundles.join(version);
        if !directory.is_dir() {
            return Err(SkillError::NotFound(format!("version {version}")));
        }
        let bundle = read_bundle(&directory)?;
        if bundle.version() != version {
            return Err(SkillError::Conflict(format!(
                "stored version {version} no longer matches its manifest"
            )));
        }
        Ok(bundle)
    }

    /// Whether a version's bytes are already on disk.
    pub fn has_version(&self, version: &str) -> bool {
        self.bundles.join(version).is_dir()
    }

    /// Stores a bundle content-addressed. Returns its version.
    pub async fn store_version(
        &self,
        bundle: &SkillBundle,
        _now: DateTime<Utc>,
    ) -> Result<String, SkillError> {
        let version = bundle.version();
        let destination = self.bundles.join(&version);
        if !destination.is_dir() {
            let staging = self.bundles.join(format!(".tmp-{}", uuid::Uuid::now_v7()));
            let result: Result<(), SkillError> = (|| {
                write_bundle(&staging, bundle)?;
                std::fs::rename(&staging, &destination)?;
                Ok(())
            })();
            if result.is_err() && staging.exists() {
                let _ = std::fs::remove_dir_all(&staging);
            }
            result?;
        }
        Ok(version)
    }

    /// All install rows for one machine.
    pub async fn installs(&self, machine_id: &str) -> Result<Vec<InstallRow>, SkillError> {
        let rows: Vec<InstallDbRow> = sqlx::query_as(
            "SELECT * FROM skill_installs WHERE machine_id = ? ORDER BY skill_id, harness",
        )
        .bind(machine_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(install_from_row).collect()
    }

    /// Inserts or updates one install row.
    pub async fn set_install(
        &self,
        skill_id: &str,
        harness: HarnessKind,
        machine_id: &str,
        path: &str,
        enabled: bool,
        now: DateTime<Utc>,
    ) -> Result<(), SkillError> {
        sqlx::query(
            "INSERT INTO skill_installs (skill_id, harness, machine_id, path, enabled, updated_at)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(skill_id, harness, machine_id) DO UPDATE SET
                path = excluded.path,
                enabled = excluded.enabled,
                updated_at = excluded.updated_at",
        )
        .bind(skill_id)
        .bind(harness_slug(harness))
        .bind(machine_id)
        .bind(path)
        .bind(if enabled { 1i64 } else { 0i64 })
        .bind(now.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Drops one install row.
    pub async fn clear_install(
        &self,
        skill_id: &str,
        harness: HarnessKind,
        machine_id: &str,
    ) -> Result<(), SkillError> {
        sqlx::query(
            "DELETE FROM skill_installs WHERE skill_id = ? AND harness = ? AND machine_id = ?",
        )
        .bind(skill_id)
        .bind(harness_slug(harness))
        .bind(machine_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Records a content conflict, replacing an older unresolved one for the
    /// same skill and candidate version.
    // Every argument is part of the conflict row; a builder would obscure the
    // two callers (import and sync) without making the record safer.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_conflict(
        &self,
        skill_id: &str,
        name: &str,
        local_version: Option<&str>,
        candidate: &SkillBundle,
        candidate_source: &SkillSource,
        reason: &str,
        now: DateTime<Utc>,
    ) -> Result<SkillConflict, SkillError> {
        let version = self.store_version(candidate, now).await?;
        // Repeating a rejected sync must not accumulate identical rows.
        let existing: Option<ConflictDbRow> = sqlx::query_as(
            "SELECT * FROM skill_conflicts
              WHERE skill_id = ? AND remote_version = ? AND resolved_at IS NULL
              LIMIT 1",
        )
        .bind(skill_id)
        .bind(&version)
        .fetch_optional(&self.pool)
        .await?;
        if let Some(row) = existing {
            return Ok(conflict_from_row(row)?.conflict);
        }
        let id = format!("cf_{}", uuid::Uuid::now_v7().simple());
        sqlx::query(
            "INSERT INTO skill_conflicts
                (id, skill_id, name, local_version, remote_version, candidate_version, candidate_source_json, reason, detected_at, resolved_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL)",
        )
        .bind(&id)
        .bind(skill_id)
        .bind(name)
        .bind(local_version)
        .bind(&version)
        .bind(&version)
        .bind(serde_json::to_string(candidate_source)?)
        .bind(reason)
        .bind(now.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(SkillConflict {
            id,
            skill_id: skill_id.to_owned(),
            name: name.to_owned(),
            local_version: local_version.map(str::to_owned),
            remote_version: version,
            reason: reason.to_owned(),
            detected_at: now,
        })
    }

    /// Unresolved conflicts.
    pub async fn conflicts(&self) -> Result<Vec<ConflictRow>, SkillError> {
        let rows: Vec<ConflictDbRow> = sqlx::query_as(
            "SELECT * FROM skill_conflicts WHERE resolved_at IS NULL ORDER BY detected_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(conflict_from_row).collect()
    }

    /// One conflict by id, resolved or not.
    pub async fn conflict(&self, id: &str) -> Result<ConflictRow, SkillError> {
        let row: Option<ConflictDbRow> =
            sqlx::query_as("SELECT * FROM skill_conflicts WHERE id = ?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        row.map(conflict_from_row)
            .transpose()?
            .ok_or_else(|| SkillError::NotFound(format!("conflict {id}")))
    }

    /// Marks a conflict resolved.
    pub async fn resolve_conflict(&self, id: &str, now: DateTime<Utc>) -> Result<(), SkillError> {
        sqlx::query("UPDATE skill_conflicts SET resolved_at = ? WHERE id = ?")
            .bind(now.to_rfc3339())
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Reads a stored state value.
    pub async fn state(&self, key: &str) -> Result<Option<String>, SkillError> {
        let row = sqlx::query("SELECT value FROM skill_state WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.and_then(|row| row.try_get::<String, _>("value").ok()))
    }

    /// Writes a state value.
    pub async fn set_state(&self, key: &str, value: &str) -> Result<(), SkillError> {
        sqlx::query(
            "INSERT INTO skill_state (key, value) VALUES (?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The machine id recorded for this registry.
    pub async fn machine_id(&self) -> Result<String, SkillError> {
        if let Some(value) = self.state("machine_id").await? {
            return Ok(value);
        }
        let value = format!("machine-{}", uuid::Uuid::now_v7().simple());
        self.set_state("machine_id", &value).await?;
        Ok(value)
    }

    /// Local revision for every live row, used to build version vectors.
    pub async fn revisions(&self) -> Result<BTreeMap<String, i64>, SkillError> {
        let rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT id, revision FROM skill_skills WHERE deleted = 0")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().collect())
    }

    fn record_from_row(row: SkillRow) -> Result<SkillRecord, SkillError> {
        Ok(SkillRecord {
            id: row.id,
            name: row.name,
            description: row.description,
            version: row.latest_version,
            source: serde_json::from_str(&row.source_json)?,
            manifest: serde_json::from_str(&row.manifest_json)?,
            revision: row.revision,
            created_at: parse_time(&row.created_at)?,
            updated_at: parse_time(&row.updated_at)?,
            deleted: row.deleted != 0,
        })
    }
}

fn install_from_row(row: InstallDbRow) -> Result<InstallRow, SkillError> {
    Ok(InstallRow {
        skill_id: row.skill_id,
        harness: harness_from_slug(&row.harness).ok_or_else(|| SkillError::InvalidSkill {
            path: row.harness.clone(),
            reason: "unknown harness in the install table".to_owned(),
        })?,
        machine_id: row.machine_id,
        path: row.path,
        enabled: row.enabled != 0,
        updated_at: parse_time(&row.updated_at)?,
    })
}

fn conflict_from_row(row: ConflictDbRow) -> Result<ConflictRow, SkillError> {
    Ok(ConflictRow {
        conflict: SkillConflict {
            id: row.id,
            skill_id: row.skill_id,
            name: row.name,
            local_version: row.local_version,
            remote_version: row.remote_version,
            reason: row.reason,
            detected_at: parse_time(&row.detected_at)?,
        },
        candidate_version: row.candidate_version,
        candidate_source: serde_json::from_str(&row.candidate_source_json)?,
        resolved: row.resolved_at.is_some(),
    })
}

fn parse_time(value: &str) -> Result<DateTime<Utc>, SkillError> {
    DateTime::parse_from_rfc3339(value)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|error| SkillError::InvalidSkill {
            path: value.to_owned(),
            reason: format!("invalid timestamp: {error}"),
        })
}

fn harness_slug(harness: HarnessKind) -> &'static str {
    harness.slug()
}

fn harness_from_slug(value: &str) -> Option<HarnessKind> {
    [
        HarnessKind::ClaudeCode,
        HarnessKind::Codex,
        HarnessKind::Opencode,
        HarnessKind::Antigravity,
    ]
    .into_iter()
    .find(|harness| harness.slug() == value)
}

impl SkillRecord {
    /// Converts a stored row to the wire summary without install state.
    pub fn summary(
        &self,
        installs: Vec<h4rn_protocol::SkillInstallState>,
        conflicts: u32,
    ) -> h4rn_protocol::SkillSummary {
        h4rn_protocol::SkillSummary {
            id: self.id.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            version: self.version.clone(),
            source: self.source.clone(),
            total_bytes: self.manifest.total_bytes,
            updated_at: self.updated_at,
            installs,
            conflicts,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{SKILL_FILE, read_bundle};

    async fn registry() -> (tempfile::TempDir, Registry) {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::open(dir.path()).await.unwrap();
        (dir, registry)
    }

    fn bundle(dir: &Path, name: &str, note: &str) -> SkillBundle {
        let root = dir.join(format!("src-{note}"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join(SKILL_FILE),
            format!("---\nname: {name}\ndescription: Demo\n---\n\n{note}\n"),
        )
        .unwrap();
        read_bundle(&root).unwrap()
    }

    #[tokio::test]
    async fn stores_versions_once_and_reads_them_back() {
        let (_dir, registry) = registry().await;
        let source = tempfile::tempdir().unwrap();
        let one = bundle(source.path(), "demo", "one");
        let record = registry
            .insert(
                &one,
                &SkillSource::LocalFolder {
                    path: "/tmp/x".into(),
                },
                Utc::now(),
            )
            .await
            .unwrap();
        assert_eq!(record.version, one.version());
        let read = registry.bundle(&one.version()).await.unwrap();
        assert_eq!(read.version(), one.version());
        assert_eq!(registry.list().await.unwrap().len(), 1);

        let two = bundle(source.path(), "demo", "two");
        let updated = registry
            .update(&record.id, &two, &record.source, Utc::now())
            .await
            .unwrap();
        assert_eq!(updated.revision, 2);
        assert_eq!(registry.list().await.unwrap()[0].version, two.version());
    }

    #[tokio::test]
    async fn install_rows_round_trip_per_machine_and_harness() {
        let (_dir, registry) = registry().await;
        let machine = registry.machine_id().await.unwrap();
        registry
            .set_install(
                "sk_1",
                HarnessKind::Codex,
                &machine,
                "/tmp/codex/demo",
                true,
                Utc::now(),
            )
            .await
            .unwrap();
        registry
            .set_install(
                "sk_1",
                HarnessKind::ClaudeCode,
                &machine,
                "/tmp/claude/demo",
                false,
                Utc::now(),
            )
            .await
            .unwrap();
        let rows = registry.installs(&machine).await.unwrap();
        assert_eq!(rows.len(), 2);
        registry
            .clear_install("sk_1", HarnessKind::Codex, &machine)
            .await
            .unwrap();
        assert_eq!(registry.installs(&machine).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn conflicts_keep_the_candidate_bundle() {
        let (_dir, registry) = registry().await;
        let source = tempfile::tempdir().unwrap();
        let candidate = bundle(source.path(), "demo", "candidate");
        let conflict = registry
            .record_conflict(
                "sk_1",
                "demo",
                Some("old"),
                &candidate,
                &SkillSource::LocalFolder {
                    path: "/tmp/x".into(),
                },
                "different content",
                Utc::now(),
            )
            .await
            .unwrap();
        assert_eq!(registry.conflicts().await.unwrap().len(), 1);
        let stored = registry.conflict(&conflict.id).await.unwrap();
        assert_eq!(stored.candidate_version, candidate.version());
        registry
            .resolve_conflict(&conflict.id, Utc::now())
            .await
            .unwrap();
        assert!(registry.conflicts().await.unwrap().is_empty());
    }
}
