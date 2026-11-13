//! Secret storage for upstream MCP servers (ADR-016).
//!
//! The SQLite registry stores only **references** — `mcp/<server id>/env/NAME`
//! — and never a value. Values live behind [`SecretStore`]:
//!
//! - [`KeychainSecretStore`] uses the OS keychain (Secret Service, Keychain,
//!   Credential Manager) and is the preferred backend;
//! - [`EncryptedFileSecretStore`] seals one JSON map with the transport's
//!   ChaCha20-Poly1305 secret-file helper under a sibling `0600` key. It is the
//!   headless Linux/WSL fallback, chosen once at construction so a value cannot
//!   be written to one backend and read from the other;
//! - [`MemorySecretStore`] exists for tests and never touches disk.
//!
//! The hub resolves references at upstream-start time and passes values in the
//! child environment or HTTP headers only. They are not returned by any
//! protocol response, not logged (the `Debug` impl lists names) and not sent to
//! another machine: a shared server executes on its owner, so the proxy on the
//! other machine has no secret to leak.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use h4rn_transport::keys::{read_secret_file, remove_secret_file, write_secret_file};

/// A failure to read or write a secret.
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    /// The backend rejected the operation.
    #[error("{0}")]
    Backend(String),
    /// The secret id is malformed.
    #[error("{0}")]
    Invalid(String),
    /// A value could not be serialised for the encrypted file backend.
    #[error("could not serialise the secret file: {0}")]
    Serialisation(#[from] serde_json::Error),
}

/// Storage for secret values addressed by a stable reference.
pub trait SecretStore: Send + Sync {
    /// Stores (or replaces) the value at `id`.
    fn put(&self, id: &str, value: &str) -> Result<(), SecretError>;

    /// Reads the value at `id`, or `None` when it is absent.
    fn get(&self, id: &str) -> Result<Option<String>, SecretError>;

    /// Deletes the value at `id`, ignoring a missing entry.
    fn delete(&self, id: &str) -> Result<(), SecretError>;

    /// Backend name for `doctor` and logs (`os-keychain`, `encrypted-file`,
    /// `memory`).
    fn backend(&self) -> &'static str;
}

/// In-process secret storage for tests.
#[derive(Default)]
pub struct MemorySecretStore {
    values: Mutex<BTreeMap<String, String>>,
}

impl MemorySecretStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl SecretStore for MemorySecretStore {
    fn put(&self, id: &str, value: &str) -> Result<(), SecretError> {
        self.values
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id.to_owned(), value.to_owned());
        Ok(())
    }

    fn get(&self, id: &str) -> Result<Option<String>, SecretError> {
        Ok(self
            .values
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .cloned())
    }

    fn delete(&self, id: &str) -> Result<(), SecretError> {
        self.values
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(id);
        Ok(())
    }

    fn backend(&self) -> &'static str {
        "memory"
    }
}

/// One encrypted JSON map of secret id -> value.
///
/// A single file (not one file per secret) means one key file and one atomic
/// rewrite, which keeps the fallback cheap enough to use on every update.
pub struct EncryptedFileSecretStore {
    path: PathBuf,
    values: Mutex<BTreeMap<String, String>>,
}

impl EncryptedFileSecretStore {
    /// Opens (or creates) the store at `path`.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, SecretError> {
        let path = path.into();
        let values = match read_secret_file(&path).map_err(backend)? {
            Some(bytes) => serde_json::from_slice(&bytes)?,
            None => BTreeMap::new(),
        };
        Ok(Self {
            path,
            values: Mutex::new(values),
        })
    }

    /// Path of the encrypted file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn persist(&self, values: &BTreeMap<String, String>) -> Result<(), SecretError> {
        if values.is_empty() {
            remove_secret_file(&self.path).map_err(backend)?;
            return Ok(());
        }
        let body = serde_json::to_vec(values)?;
        write_secret_file(&self.path, &body).map_err(backend)
    }
}

impl SecretStore for EncryptedFileSecretStore {
    fn put(&self, id: &str, value: &str) -> Result<(), SecretError> {
        let mut values = self
            .values
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values.insert(id.to_owned(), value.to_owned());
        self.persist(&values)
    }

    fn get(&self, id: &str) -> Result<Option<String>, SecretError> {
        Ok(self
            .values
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .cloned())
    }

    fn delete(&self, id: &str) -> Result<(), SecretError> {
        let mut values = self
            .values
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values.remove(id);
        self.persist(&values)
    }

    fn backend(&self) -> &'static str {
        "encrypted-file"
    }
}

/// OS keychain storage.
pub struct KeychainSecretStore {
    service: String,
}

impl KeychainSecretStore {
    /// Creates a store in the platform credential store under `service`.
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    fn entry(&self, id: &str) -> Result<keyring::Entry, SecretError> {
        keyring::Entry::new(&self.service, id)
            .map_err(|error| SecretError::Backend(error.to_string()))
    }
}

impl SecretStore for KeychainSecretStore {
    fn put(&self, id: &str, value: &str) -> Result<(), SecretError> {
        self.entry(id)?
            .set_password(value)
            .map_err(|error| SecretError::Backend(error.to_string()))
    }

    fn get(&self, id: &str) -> Result<Option<String>, SecretError> {
        match self.entry(id)?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(SecretError::Backend(error.to_string())),
        }
    }

    fn delete(&self, id: &str) -> Result<(), SecretError> {
        match self.entry(id)?.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(SecretError::Backend(error.to_string())),
        }
    }

    fn backend(&self) -> &'static str {
        "os-keychain"
    }
}

/// The backend selected for a daemon data directory.
pub enum OpenSecretStore {
    /// The OS keychain was available.
    Keychain(KeychainSecretStore),
    /// The encrypted file fallback was selected.
    File(EncryptedFileSecretStore),
}

impl OpenSecretStore {
    /// Opens the preferred backend, probing the OS credential store.
    ///
    /// `path` is used only when no keychain is available.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, SecretError> {
        let path = path.into();
        if keyring::Entry::store_status().is_ok() {
            Ok(Self::Keychain(KeychainSecretStore::new("h4rn-mcp")))
        } else {
            Ok(Self::File(EncryptedFileSecretStore::open(path)?))
        }
    }

    /// The backend as a trait object.
    pub fn into_store(self) -> Box<dyn SecretStore> {
        match self {
            Self::Keychain(store) => Box::new(store),
            Self::File(store) => Box::new(store),
        }
    }
}

/// Builds the stable id for a secret belonging to `server_id`.
pub fn secret_id(server_id: &str, kind: &str, name: &str) -> String {
    format!("mcp/{server_id}/{kind}/{name}")
}

/// Validates a secret id before it reaches a backend.
pub fn validate_secret_id(id: &str) -> Result<(), SecretError> {
    if id.is_empty() || id.len() > 512 {
        return Err(SecretError::Invalid("invalid secret reference".into()));
    }
    if id.contains("..") || id.contains('\0') {
        return Err(SecretError::Invalid("invalid secret reference".into()));
    }
    Ok(())
}

fn backend(error: impl std::fmt::Display) -> SecretError {
    SecretError::Backend(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_round_trips() {
        let store = MemorySecretStore::new();
        store.put("a", "1").unwrap();
        assert_eq!(store.get("a").unwrap().as_deref(), Some("1"));
        store.delete("a").unwrap();
        assert_eq!(store.get("a").unwrap(), None);
        assert_eq!(store.backend(), "memory");
    }

    #[test]
    fn encrypted_file_store_never_writes_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-secrets.json");
        let store = EncryptedFileSecretStore::open(&path).unwrap();
        store.put("mcp/srv/env/TOKEN", "ghp_very_secret").unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("ghp_very_secret"));
        assert!(raw.starts_with("h4rn-encv1:"));
        // Re-opening reads the same value.
        let reopened = EncryptedFileSecretStore::open(&path).unwrap();
        assert_eq!(
            reopened.get("mcp/srv/env/TOKEN").unwrap().as_deref(),
            Some("ghp_very_secret")
        );
        reopened.delete("mcp/srv/env/TOKEN").unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn secret_ids_are_stable_and_validated() {
        assert_eq!(
            secret_id("srv_1", "env", "GITHUB_TOKEN"),
            "mcp/srv_1/env/GITHUB_TOKEN"
        );
        assert!(validate_secret_id("mcp/srv_1/env/X").is_ok());
        assert!(validate_secret_id("../etc/passwd").is_err());
        assert!(validate_secret_id("").is_err());
    }
}
