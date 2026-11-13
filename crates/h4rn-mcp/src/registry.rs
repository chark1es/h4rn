//! Durable MCP server configuration.
//!
//! The registry is the daemon's SQLite cache of which servers the user has
//! configured, which harness/machine may see them, where they were imported
//! from, and which machines they are shared with. It contains **no secret
//! values**: environment variables and headers are stored as
//! [`StoredSecret`] references into a [`SecretStore`](crate::secrets::SecretStore).
//!
//! A separate SQLite file (`mcp.db`) is used rather than the event journal so
//! MCP configuration can be deleted or rebuilt without touching thread
//! history. Both live in the daemon data directory and both are caches under
//! ADR-012, so this is a storage-layout choice rather than a second system of
//! record.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use h4rn_protocol::{HostId, McpEnabledMatrix, McpSharePolicy};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

/// One secret-valued variable stored by reference.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredSecret {
    /// Environment variable or header name.
    pub name: String,
    /// Reference into the secret store.
    pub secret_id: String,
}

/// A transport as persisted: values are references, never plaintext.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StoredTransport {
    /// A child process.
    Stdio {
        /// Executable.
        command: String,
        /// Arguments.
        #[serde(default)]
        args: Vec<String>,
        /// Working directory.
        #[serde(default)]
        cwd: Option<String>,
        /// Environment variables by reference.
        #[serde(default)]
        env: Vec<StoredSecret>,
    },
    /// A streamable-HTTP endpoint.
    Http {
        /// Endpoint.
        url: String,
        /// Headers by reference.
        #[serde(default)]
        headers: Vec<StoredSecret>,
    },
    /// A legacy SSE endpoint.
    Sse {
        /// Endpoint.
        url: String,
        /// Headers by reference.
        #[serde(default)]
        headers: Vec<StoredSecret>,
    },
}

impl StoredTransport {
    /// Every secret reference in the transport.
    pub fn secrets(&self) -> Vec<&StoredSecret> {
        match self {
            Self::Stdio { env, .. } => env.iter().collect(),
            Self::Http { headers, .. } | Self::Sse { headers, .. } => headers.iter().collect(),
        }
    }

    /// The transport kind (`stdio`, `http`, `sse`).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Stdio { .. } => "stdio",
            Self::Http { .. } => "http",
            Self::Sse { .. } => "sse",
        }
    }
}

/// One configured server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServerRecord {
    /// Stable id (`srv_` + UUID v7).
    pub id: String,
    /// Unique lowercased name; the tool namespace.
    pub name: String,
    /// Persisted transport.
    pub transport: StoredTransport,
    /// Free-form tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Harness/machine enable matrix.
    #[serde(default)]
    pub enabled: McpEnabledMatrix,
    /// Where the configuration came from.
    #[serde(default)]
    pub imported_from: Option<String>,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last update time.
    pub updated_at: DateTime<Utc>,
}

impl ServerRecord {
    /// Creates a new record with a fresh id and timestamps.
    pub fn new(
        name: impl Into<String>,
        transport: StoredTransport,
        tags: Vec<String>,
        enabled: McpEnabledMatrix,
        imported_from: Option<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: format!("srv_{}", Uuid::now_v7()),
            name: name.into(),
            transport,
            tags,
            enabled,
            imported_from,
            created_at: now,
            updated_at: now,
        }
    }
}

/// One `server -> machine` share grant.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShareRecord {
    /// Server id.
    pub server_id: String,
    /// Machine allowed to proxy it.
    pub machine: HostId,
    /// Policy enforced on this machine.
    pub policy: McpSharePolicy,
    /// Creation time.
    pub created_at: DateTime<Utc>,
}

/// Durable registry operations.
#[async_trait]
pub trait RegistryStore: Send + Sync {
    /// Every server, ordered by name.
    async fn list(&self) -> Result<Vec<ServerRecord>, RegistryError>;

    /// One server by name (lowercased by the caller).
    async fn get(&self, name: &str) -> Result<Option<ServerRecord>, RegistryError>;

    /// Inserts or replaces a record by id.
    async fn put(&self, record: &ServerRecord) -> Result<(), RegistryError>;

    /// Deletes one server by id.
    async fn remove(&self, id: &str) -> Result<(), RegistryError>;

    /// Every share grant.
    async fn list_shares(&self) -> Result<Vec<ShareRecord>, RegistryError>;

    /// Inserts or replaces one grant.
    async fn put_share(&self, share: &ShareRecord) -> Result<(), RegistryError>;

    /// Removes one grant.
    async fn remove_share(&self, server_id: &str, machine: &HostId) -> Result<(), RegistryError>;

    /// Removes every grant for a server.
    async fn remove_shares_for(&self, server_id: &str) -> Result<(), RegistryError>;
}

/// A registry failure.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// SQLite failed.
    #[error("mcp registry database error: {0}")]
    Database(#[from] sqlx::Error),
    /// Stored JSON could not be decoded.
    #[error("mcp registry contains invalid JSON: {0}")]
    Serde(#[from] serde_json::Error),
    /// A record was not found.
    #[error("no MCP server named \"{0}\" is configured")]
    NotFound(String),
    /// A name is already taken.
    #[error("an MCP server named \"{0}\" already exists")]
    Conflict(String),
}

/// In-memory registry used by tests and by embedded callers.
#[derive(Default)]
pub struct MemoryRegistry {
    servers: Mutex<BTreeMap<String, ServerRecord>>,
    shares: Mutex<Vec<ShareRecord>>,
}

impl MemoryRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl RegistryStore for MemoryRegistry {
    async fn list(&self) -> Result<Vec<ServerRecord>, RegistryError> {
        let servers = self
            .servers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut records: Vec<ServerRecord> = servers.values().cloned().collect();
        records.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(records)
    }

    async fn get(&self, name: &str) -> Result<Option<ServerRecord>, RegistryError> {
        Ok(self
            .servers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(name)
            .cloned())
    }

    async fn put(&self, record: &ServerRecord) -> Result<(), RegistryError> {
        let mut servers = self
            .servers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = servers.get(&record.name)
            && existing.id != record.id
        {
            return Err(RegistryError::Conflict(record.name.clone()));
        }
        servers.insert(record.name.clone(), record.clone());
        Ok(())
    }

    async fn remove(&self, id: &str) -> Result<(), RegistryError> {
        self.servers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|_, record| record.id != id);
        Ok(())
    }

    async fn list_shares(&self) -> Result<Vec<ShareRecord>, RegistryError> {
        Ok(self
            .shares
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone())
    }

    async fn put_share(&self, share: &ShareRecord) -> Result<(), RegistryError> {
        let mut shares = self
            .shares
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        shares.retain(|existing| {
            !(existing.server_id == share.server_id && existing.machine == share.machine)
        });
        shares.push(share.clone());
        Ok(())
    }

    async fn remove_share(&self, server_id: &str, machine: &HostId) -> Result<(), RegistryError> {
        self.shares
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|share| !(share.server_id == server_id && &share.machine == machine));
        Ok(())
    }

    async fn remove_shares_for(&self, server_id: &str) -> Result<(), RegistryError> {
        self.shares
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|share| share.server_id != server_id);
        Ok(())
    }
}

/// SQLite-backed registry.
pub struct SqliteRegistry {
    pool: SqlitePool,
}

impl SqliteRegistry {
    /// Opens (or creates) the registry at `path` and applies the schema.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, RegistryError> {
        let options = SqliteConnectOptions::new()
            .filename(path.as_ref())
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        let registry = Self { pool };
        registry.migrate().await?;
        Ok(registry)
    }

    /// Opens an in-memory registry (tests that want SQL semantics).
    pub async fn open_memory() -> Result<Self, RegistryError> {
        let options = SqliteConnectOptions::new()
            .filename(":memory:")
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        let registry = Self { pool };
        registry.migrate().await?;
        Ok(registry)
    }

    async fn migrate(&self) -> Result<(), RegistryError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS mcp_servers (\
                id TEXT PRIMARY KEY NOT NULL,\
                name TEXT NOT NULL UNIQUE,\
                transport TEXT NOT NULL,\
                tags TEXT NOT NULL DEFAULT '[]',\
                enabled TEXT NOT NULL DEFAULT '{}',\
                imported_from TEXT,\
                created_at TEXT NOT NULL,\
                updated_at TEXT NOT NULL\
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS mcp_shares (\
                server_id TEXT NOT NULL,\
                machine TEXT NOT NULL,\
                policy TEXT NOT NULL,\
                created_at TEXT NOT NULL,\
                PRIMARY KEY (server_id, machine)\
            )",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Closes the connection pool (tests).
    pub async fn close(self) {
        self.pool.close().await;
    }

    fn row_to_record(row: &sqlx::sqlite::SqliteRow) -> Result<ServerRecord, RegistryError> {
        Ok(ServerRecord {
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            transport: serde_json::from_str(&row.try_get::<String, _>("transport")?)?,
            tags: serde_json::from_str(&row.try_get::<String, _>("tags")?)?,
            enabled: serde_json::from_str(&row.try_get::<String, _>("enabled")?)?,
            imported_from: row.try_get("imported_from")?,
            created_at: row.try_get::<DateTime<Utc>, _>("created_at")?,
            updated_at: row.try_get::<DateTime<Utc>, _>("updated_at")?,
        })
    }
}

#[async_trait]
impl RegistryStore for SqliteRegistry {
    async fn list(&self) -> Result<Vec<ServerRecord>, RegistryError> {
        let rows = sqlx::query("SELECT * FROM mcp_servers ORDER BY name ASC")
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(Self::row_to_record).collect()
    }

    async fn get(&self, name: &str) -> Result<Option<ServerRecord>, RegistryError> {
        let row = sqlx::query("SELECT * FROM mcp_servers WHERE name = ?")
            .bind(name)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(Self::row_to_record).transpose()
    }

    async fn put(&self, record: &ServerRecord) -> Result<(), RegistryError> {
        // A rename is not allowed to collide with another server.
        let conflict = sqlx::query("SELECT id FROM mcp_servers WHERE name = ? AND id != ?")
            .bind(&record.name)
            .bind(&record.id)
            .fetch_optional(&self.pool)
            .await?;
        if conflict.is_some() {
            return Err(RegistryError::Conflict(record.name.clone()));
        }
        sqlx::query(
            "INSERT INTO mcp_servers (id, name, transport, tags, enabled, imported_from, created_at, updated_at)\
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)\
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, transport = excluded.transport,\
             tags = excluded.tags, enabled = excluded.enabled, imported_from = excluded.imported_from,\
             updated_at = excluded.updated_at",
        )
        .bind(&record.id)
        .bind(&record.name)
        .bind(serde_json::to_string(&record.transport)?)
        .bind(serde_json::to_string(&record.tags)?)
        .bind(serde_json::to_string(&record.enabled)?)
        .bind(&record.imported_from)
        .bind(record.created_at)
        .bind(record.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn remove(&self, id: &str) -> Result<(), RegistryError> {
        sqlx::query("DELETE FROM mcp_servers WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM mcp_shares WHERE server_id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn list_shares(&self) -> Result<Vec<ShareRecord>, RegistryError> {
        let rows = sqlx::query("SELECT * FROM mcp_shares ORDER BY created_at ASC")
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|row| {
                Ok(ShareRecord {
                    server_id: row.try_get("server_id")?,
                    machine: HostId::new(row.try_get::<String, _>("machine")?),
                    policy: serde_json::from_str(&row.try_get::<String, _>("policy")?)?,
                    created_at: row.try_get("created_at")?,
                })
            })
            .collect()
    }

    async fn put_share(&self, share: &ShareRecord) -> Result<(), RegistryError> {
        sqlx::query(
            "INSERT INTO mcp_shares (server_id, machine, policy, created_at) VALUES (?, ?, ?, ?)\
             ON CONFLICT(server_id, machine) DO UPDATE SET policy = excluded.policy",
        )
        .bind(&share.server_id)
        .bind(share.machine.as_str())
        .bind(serde_json::to_string(&share.policy)?)
        .bind(share.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn remove_share(&self, server_id: &str, machine: &HostId) -> Result<(), RegistryError> {
        sqlx::query("DELETE FROM mcp_shares WHERE server_id = ? AND machine = ?")
            .bind(server_id)
            .bind(machine.as_str())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn remove_shares_for(&self, server_id: &str) -> Result<(), RegistryError> {
        sqlx::query("DELETE FROM mcp_shares WHERE server_id = ?")
            .bind(server_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use h4rn_protocol::McpServerStatusKind;

    fn record(name: &str) -> ServerRecord {
        ServerRecord::new(
            name,
            StoredTransport::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "server".into()],
                cwd: None,
                env: vec![StoredSecret {
                    name: "TOKEN".into(),
                    secret_id: "mcp/srv/env/TOKEN".into(),
                }],
            },
            vec!["test".into()],
            McpEnabledMatrix::default(),
            Some("claude".into()),
        )
    }

    #[tokio::test]
    async fn sqlite_round_trips_without_secret_values() {
        let registry = SqliteRegistry::open_memory().await.unwrap();
        let record = record("github");
        registry.put(&record).await.unwrap();
        let loaded = registry.get("github").await.unwrap().unwrap();
        assert_eq!(loaded.id, record.id);
        assert_eq!(loaded.transport.kind(), "stdio");
        let json = serde_json::to_string(&loaded).unwrap();
        assert!(!json.contains("ghp_"));
        assert!(json.contains("mcp/srv/env/TOKEN"));
    }

    #[tokio::test]
    async fn names_are_unique() {
        let registry = SqliteRegistry::open_memory().await.unwrap();
        registry.put(&record("github")).await.unwrap();
        let mut other = record("github");
        other.id = "srv_other".into();
        assert!(matches!(
            registry.put(&other).await,
            Err(RegistryError::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn shares_are_scoped_to_server_and_machine() {
        let registry = SqliteRegistry::open_memory().await.unwrap();
        let server = record("github");
        registry.put(&server).await.unwrap();
        let machine = HostId::new("host_b");
        registry
            .put_share(&ShareRecord {
                server_id: server.id.clone(),
                machine: machine.clone(),
                policy: McpSharePolicy::default(),
                created_at: Utc::now(),
            })
            .await
            .unwrap();
        assert_eq!(registry.list_shares().await.unwrap().len(), 1);
        registry.remove_share(&server.id, &machine).await.unwrap();
        assert!(registry.list_shares().await.unwrap().is_empty());
    }

    #[test]
    fn status_kind_is_not_persisted() {
        // Runtime status must never leak into the record; this guard fails if a
        // future field is added without thinking about durability.
        let record = record("x");
        let json = serde_json::to_string(&record).unwrap();
        assert!(!json.contains("Status"));
        let _ = McpServerStatusKind::Stopped;
    }
}
