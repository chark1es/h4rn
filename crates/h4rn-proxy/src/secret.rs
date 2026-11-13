//! Secret storage behind a narrow interface (ADR-016).
//!
//! The daemon supplies an OS-keychain-backed store with an encrypted-file
//! fallback; tests use [`MemorySecretStore`]. Profile metadata is deliberately
//! separate from secrets so a metadata file can never leak a key.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use crate::error::{ProxyError, ProxyErrorKind};

/// Stores opaque secret values by name.
pub trait SecretStore: Send + Sync + std::fmt::Debug {
    /// Returns the secret, or `None` when it has never been written.
    fn get(&self, key: &str) -> Result<Option<String>, ProxyError>;

    /// Writes (or replaces) a secret.
    fn set(&self, key: &str, value: &str) -> Result<(), ProxyError>;

    /// Removes a secret. Removing a missing secret succeeds.
    fn delete(&self, key: &str) -> Result<(), ProxyError>;
}

/// An in-process store for tests.
#[derive(Debug, Default)]
pub struct MemorySecretStore {
    values: Mutex<BTreeMap<String, String>>,
}

impl MemorySecretStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    fn guard(&self) -> MutexGuard<'_, BTreeMap<String, String>> {
        self.values
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl SecretStore for MemorySecretStore {
    fn get(&self, key: &str) -> Result<Option<String>, ProxyError> {
        Ok(self.guard().get(key).cloned())
    }

    fn set(&self, key: &str, value: &str) -> Result<(), ProxyError> {
        self.guard().insert(key.to_owned(), value.to_owned());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), ProxyError> {
        self.guard().remove(key);
        Ok(())
    }
}

/// Wraps a secret so `Debug` output never reveals it.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretValue(String);

impl SecretValue {
    /// Wraps a plaintext value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrows the plaintext.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretValue(***)")
    }
}

impl Drop for SecretValue {
    fn drop(&mut self) {
        // Overwrite the allocation we own; the OS may still have copied it, but
        // this keeps the plaintext out of a later heap reuse.
        unsafe {
            for byte in self.0.as_bytes_mut() {
                *byte = 0;
            }
        }
    }
}

/// Maps a secret-store backend failure into a proxy error without echoing the
/// value. The backend error is reduced to a fixed description.
pub fn secret_backend_error(operation: &str) -> ProxyError {
    ProxyError::new(
        ProxyErrorKind::Secret,
        format!("could not {operation} the proxy secret in the daemon secret store"),
    )
    .with_hint("Unlock the OS keychain, or check that the daemon data directory is writable")
}
