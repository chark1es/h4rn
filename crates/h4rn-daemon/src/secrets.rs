//! The daemon's secret store for CLIProxyAPI keys (ADR-016).
//!
//! The OS keychain is used when a credential store is available; headless hosts
//! fall back to an encrypted `0600` file under the proxy data directory. The
//! profile metadata file never contains a key, and this module never logs one:
//! diagnostics name the backend, not the value.

use std::path::{Path, PathBuf};

use h4rn_proxy::{ProxyError, ProxyErrorKind, SecretStore, secret_backend_error};

/// Service name used for OS-keychain entries.
const KEYRING_SERVICE: &str = "dev.h4rn.proxy";

/// Keychain-first secret storage with an encrypted-file fallback.
#[derive(Clone)]
pub struct DaemonSecretStore {
    dir: PathBuf,
    keyring: bool,
}

impl std::fmt::Debug for DaemonSecretStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DaemonSecretStore")
            .field("backend", &self.backend())
            .finish_non_exhaustive()
    }
}

impl DaemonSecretStore {
    /// Opens the store under `dir`, selecting a backend once.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, ProxyError> {
        let dir = dir.as_ref().to_path_buf();
        crate::config::create_private_dir(&dir).map_err(|_| {
            ProxyError::new(
                ProxyErrorKind::Secret,
                "could not create the proxy secret directory",
            )
        })?;
        Ok(Self::with_keyring(dir, keyring_available()))
    }

    /// Builds a store with an explicit backend, for diagnostics and tests.
    pub fn with_keyring(dir: impl Into<PathBuf>, keyring: bool) -> Self {
        Self {
            dir: dir.into(),
            keyring,
        }
    }

    /// Which backend is in use: `os-keychain` or `encrypted-file`.
    pub fn backend(&self) -> &'static str {
        if self.keyring {
            "os-keychain"
        } else {
            "encrypted-file"
        }
    }

    fn entry(&self, key: &str) -> Result<keyring::Entry, ProxyError> {
        keyring::Entry::new(KEYRING_SERVICE, key).map_err(|_| secret_backend_error("open"))
    }

    fn path(&self, key: &str) -> PathBuf {
        let digest = blake3::hash(key.as_bytes()).to_hex();
        self.dir.join(format!("{digest}.secret"))
    }
}

impl SecretStore for DaemonSecretStore {
    fn get(&self, key: &str) -> Result<Option<String>, ProxyError> {
        if self.keyring
            && let Ok(entry) = self.entry(key)
            && let Ok(value) = entry.get_password()
        {
            return Ok(Some(value));
        }
        let bytes = h4rn_transport::read_secret_file(&self.path(key))
            .map_err(|_| secret_backend_error("read"))?;
        let Some(bytes) = bytes else {
            return Ok(None);
        };
        String::from_utf8(bytes).map(Some).map_err(|_| {
            ProxyError::new(
                ProxyErrorKind::Secret,
                "a stored proxy secret is not valid UTF-8",
            )
            .with_hint("Replace the management or API key on the affected proxy profile")
        })
    }

    fn set(&self, key: &str, value: &str) -> Result<(), ProxyError> {
        if self.keyring
            && let Ok(entry) = self.entry(key)
            && entry.set_password(value).is_ok()
        {
            return Ok(());
        }
        h4rn_transport::write_secret_file(&self.path(key), value.as_bytes())
            .map_err(|_| secret_backend_error("write"))
    }

    fn delete(&self, key: &str) -> Result<(), ProxyError> {
        if self.keyring
            && let Ok(entry) = self.entry(key)
        {
            let _ = entry.delete_credential();
        }
        h4rn_transport::remove_secret_file(&self.path(key))
            .map_err(|_| secret_backend_error("remove"))
    }
}

fn keyring_available() -> bool {
    keyring::Entry::store_status().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, DaemonSecretStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = DaemonSecretStore::with_keyring(dir.path().join("secrets"), false);
        std::fs::create_dir_all(dir.path().join("secrets")).expect("secrets dir");
        (dir, store)
    }

    #[test]
    fn secrets_round_trip_and_delete_through_the_file_backend() {
        let (_dir, store) = store();
        assert_eq!(store.backend(), "encrypted-file");
        assert!(store.get("proxy/proxy_0/api-key").expect("get").is_none());
        store
            .set("proxy/proxy_0/api-key", "sk-secret")
            .expect("set");
        assert_eq!(
            store.get("proxy/proxy_0/api-key").expect("get").as_deref(),
            Some("sk-secret")
        );
        store.delete("proxy/proxy_0/api-key").expect("delete");
        assert!(store.get("proxy/proxy_0/api-key").expect("get").is_none());
        store.delete("proxy/proxy_0/api-key").expect("idempotent");
    }

    #[test]
    fn the_debug_representation_never_contains_a_value() {
        let (_dir, store) = store();
        store.set("k", "sk-must-not-appear").expect("set");
        let rendered = format!("{store:?}");
        assert!(!rendered.contains("sk-must-not-appear"));
        assert!(rendered.contains("encrypted-file"));
    }
}
