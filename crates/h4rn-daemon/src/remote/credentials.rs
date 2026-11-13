//! The daemon's connect session, persisted as an encrypted secret file.
//!
//! `h4rnd login` stores the refresh token and the non-secret routing ids in
//! `<data_dir>/connect-credentials.json`. The file is written with
//! [`h4rn_transport::keys::write_secret_file`], which encrypts it under a
//! sibling `0600` key file on headless Linux/WSL and is the same fallback the
//! transport identity uses. This is deliberate: the daemon usually runs without
//! a desktop session, so there is no keychain to use, and the encrypted file
//! keeps a stray backup or dotfile sync from becoming a credential leak.

use std::path::Path;

use anyhow::{Context, Result};
use chrono::Utc;
use h4rn_transport::keys::{read_secret_file, remove_secret_file, write_secret_file};
use serde::{Deserialize, Serialize};

/// File name inside the daemon data directory.
pub const CREDENTIALS_FILE: &str = "connect-credentials.json";

/// A persisted h4rn-connect session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    /// Public base URL of the connect deployment, without a trailing slash.
    pub connect_url: String,
    /// Signed-in user.
    pub user_id: String,
    /// This daemon's connect device id.
    pub device_id: String,
    /// The machine this daemon owns.
    pub machine_id: String,
    /// Short-lived bearer token.
    pub access_token: String,
    /// Rotating refresh token.
    pub refresh_token: String,
    /// Access-token expiry, Unix seconds.
    pub expires_at: i64,
    /// Token type, normally `Bearer`.
    pub token_type: String,
}

impl Credentials {
    /// Reads credentials from `data_dir`, returning `None` when signed out.
    pub fn load(data_dir: &Path) -> Result<Option<Self>> {
        let path = data_dir.join(CREDENTIALS_FILE);
        let Some(bytes) = read_secret_file(&path)
            .with_context(|| format!("read connect credentials {}", path.display()))?
        else {
            return Ok(None);
        };
        let credentials: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse connect credentials {}", path.display()))?;
        Ok(Some(credentials))
    }

    /// Writes credentials into `data_dir` with private permissions.
    pub fn save(&self, data_dir: &Path) -> Result<()> {
        let path = data_dir.join(CREDENTIALS_FILE);
        let body = serde_json::to_vec(self).context("serialise connect credentials")?;
        write_secret_file(&path, &body)
            .with_context(|| format!("write connect credentials {}", path.display()))?;
        Ok(())
    }

    /// Removes the credentials file, ignoring a missing file.
    pub fn remove(data_dir: &Path) -> Result<()> {
        let path = data_dir.join(CREDENTIALS_FILE);
        remove_secret_file(&path)
            .with_context(|| format!("remove connect credentials {}", path.display()))?;
        Ok(())
    }

    /// Whether the access token expires within `skew_secs`.
    pub fn expires_within(&self, skew_secs: i64) -> bool {
        self.expires_at <= Utc::now().timestamp() + skew_secs
    }

    /// The connect base URL with no trailing slash.
    pub fn base_url(&self) -> &str {
        self.connect_url.trim_end_matches('/')
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials() -> Credentials {
        Credentials {
            connect_url: "https://connect.example.com".into(),
            user_id: "user-1".into(),
            device_id: "device-1".into(),
            machine_id: "machine-1".into(),
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: Utc::now().timestamp() + 600,
            token_type: "Bearer".into(),
        }
    }

    #[test]
    fn credentials_round_trip_through_the_encrypted_file() {
        let dir = tempfile::tempdir().unwrap();
        let credentials = credentials();
        credentials.save(dir.path()).unwrap();
        let raw = std::fs::read_to_string(dir.path().join(CREDENTIALS_FILE)).unwrap();
        assert!(raw.starts_with("h4rn-encv1:"));
        assert!(!raw.contains("refresh"));
        assert_eq!(
            Credentials::load(dir.path()).unwrap().as_ref(),
            Some(&credentials)
        );
        Credentials::remove(dir.path()).unwrap();
        assert!(Credentials::load(dir.path()).unwrap().is_none());
    }

    #[test]
    fn expiry_is_evaluated_with_a_skew() {
        let mut credentials = credentials();
        credentials.expires_at = Utc::now().timestamp() + 10;
        assert!(credentials.expires_within(60));
        assert!(!credentials.expires_within(1));
    }
}
