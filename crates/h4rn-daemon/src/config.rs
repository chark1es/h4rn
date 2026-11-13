//! Daemon configuration: data directory, listeners and logging.
//!
//! Configuration comes from CLI flags (see [`crate::cli`]), environment
//! variables and platform defaults, in that order. There is deliberately no
//! config file yet: Phase 1b needs one unambiguous source of truth for the
//! data directory so `serve`, `token` and `doctor` cannot disagree.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::remote::TailscaleMode;

/// The loopback address a fresh daemon listens on.
pub const DEFAULT_BIND: &str = "127.0.0.1:47800";

/// The desktop application directory name.
pub const APP_DIR: &str = "h4rn";

/// Resolved daemon configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Directory holding the SQLite cache, access token and future blobs.
    pub data_dir: PathBuf,
    /// Primary listen address.
    pub bind: SocketAddr,
    /// Extra address to listen on (for example a Tailscale IP); loopback only
    /// by default because remote access arrives through the transport crate.
    pub extra_bind: Option<SocketAddr>,
    /// Whether to detect and bind/advertise Tailscale automatically.
    pub tailscale: TailscaleMode,
    /// `tracing` filter directive.
    pub log_level: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            bind: DEFAULT_BIND
                .parse()
                .expect("the default bind address is valid"),
            extra_bind: None,
            tailscale: TailscaleMode::Auto,
            log_level: "h4rn_daemon=info".to_owned(),
        }
    }
}

impl Config {
    /// Builds a configuration rooted at `data_dir` with the default listener.
    pub fn with_data_dir(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            ..Self::default()
        }
    }

    /// Path of the local access token.
    pub fn access_token_path(&self) -> PathBuf {
        self.data_dir.join("access_token")
    }

    /// Path of the SQLite event cache.
    pub fn event_db_path(&self) -> PathBuf {
        self.data_dir.join("events.db")
    }

    /// Path of the Noise transport identity.
    pub fn transport_identity_path(&self) -> PathBuf {
        self.data_dir.join("transport-identity")
    }

    /// Path of the paired-device allowlist written by the secure transport.
    pub fn allowlist_path(&self) -> PathBuf {
        self.data_dir.join("transport-allowlist")
    }

    /// Path of the persisted h4rn-connect session.
    pub fn connect_credentials_path(&self) -> PathBuf {
        self.data_dir
            .join(crate::remote::credentials::CREDENTIALS_FILE)
    }

    /// Path of the pending direct-pairing file.
    pub fn direct_pairing_path(&self) -> PathBuf {
        crate::remote::pairing::direct_pairing_path(&self.data_dir)
    }

    /// Path of the remote-action audit trail.
    pub fn remote_audit_path(&self) -> PathBuf {
        self.data_dir.join(crate::remote::audit::AUDIT_FILE)
    }

    /// Path of the public forge-account metadata (no tokens).
    pub fn forge_accounts_path(&self) -> PathBuf {
        self.data_dir.join(crate::forge::ACCOUNTS_FILE)
    }

    /// Path of custom forge host mappings.
    pub fn forge_hosts_path(&self) -> PathBuf {
        self.data_dir.join(crate::forge::HOSTS_FILE)
    }

    /// Path of the local forge write audit trail.
    pub fn forge_audit_path(&self) -> PathBuf {
        self.data_dir.join(crate::forge::AUDIT_FILE)
    }

    /// Directory holding content-addressed attachment blobs (ADR-008).
    pub fn blob_dir(&self) -> PathBuf {
        self.data_dir.join("blobs")
    }

    /// Directory holding CLIProxyAPI profile metadata (ADR-015).
    pub fn proxy_dir(&self) -> PathBuf {
        self.data_dir.join("proxy")
    }

    /// Directory holding encrypted proxy secrets when no keychain is available.
    pub fn proxy_secret_dir(&self) -> PathBuf {
        self.proxy_dir().join("secrets")
    }

    /// Directory holding per-thread materialised attachment copies.
    pub fn attachments_dir(&self) -> PathBuf {
        self.data_dir.join("attachments")
    }

    /// Largest attachment this daemon accepts, in bytes.
    ///
    /// Defaults to 50 MiB. `H4RN_MAX_BLOB_BYTES` overrides it for hosts that
    /// need a different ceiling; the UI reads the limit from the daemon's error
    /// rather than duplicating the number.
    pub fn max_blob_bytes(&self) -> u64 {
        std::env::var("H4RN_MAX_BLOB_BYTES")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(50 * 1024 * 1024)
    }

    /// How long an unreferenced blob or an abandoned upload survives.
    ///
    /// Defaults to 24 hours. The grace window is what keeps a just-committed
    /// upload from being swept before the message that references it lands.
    pub fn blob_gc_grace(&self) -> std::time::Duration {
        std::env::var("H4RN_BLOB_GRACE_SECS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(std::time::Duration::from_secs)
            .unwrap_or(std::time::Duration::from_secs(24 * 60 * 60))
    }

    /// How often the attachment GC runs.
    pub fn blob_gc_interval(&self) -> std::time::Duration {
        std::env::var("H4RN_BLOB_GC_SECS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|value| *value > 0)
            .map(std::time::Duration::from_secs)
            .unwrap_or(std::time::Duration::from_secs(60 * 60))
    }

    /// Creates the data directory if it does not exist.
    ///
    /// On Unix the directory is created with `0700`; it holds the access token
    /// and, later, harness credentials and blob paths.
    pub fn ensure_data_dir(&self) -> Result<()> {
        create_private_dir(&self.data_dir)
    }
}

/// The platform data directory for h4rn.
///
/// `dirs::data_dir` is XDG on Linux (`$XDG_DATA_HOME` or
/// `~/.local/share`), `~/Library/Application Support` on macOS and
/// `%APPDATA%` on Windows. `H4RN_DATA_DIR` wins when the CLI does not pass one.
pub fn default_data_dir() -> PathBuf {
    if let Some(value) = std::env::var_os("H4RN_DATA_DIR") {
        return PathBuf::from(value);
    }
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_DIR)
}

/// Creates a directory with private permissions on Unix.
pub fn create_private_dir(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(path)
        .with_context(|| format!("create data directory {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(path, permissions)
            .with_context(|| format!("tighten permissions on {}", path.display()))?;
    }
    Ok(())
}

/// Parses a listen address, accepting a bare port for convenience.
pub fn parse_addr(value: &str) -> Result<SocketAddr> {
    let value = value.trim();
    if let Ok(addr) = value.parse::<SocketAddr>() {
        return Ok(addr);
    }
    if let Ok(port) = value.parse::<u16>() {
        return Ok(SocketAddr::from(([127, 0, 0, 1], port)));
    }
    anyhow::bail!("invalid listen address {value:?}; expected HOST:PORT, e.g. 127.0.0.1:47800")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_and_bare_ports() {
        assert_eq!(
            parse_addr("127.0.0.1:47800").unwrap(),
            "127.0.0.1:47800".parse().unwrap()
        );
        assert_eq!(
            parse_addr("47801").unwrap(),
            "127.0.0.1:47801".parse().unwrap()
        );
        assert!(parse_addr("not-an-address").is_err());
    }

    #[test]
    fn token_and_database_live_in_the_data_dir() {
        let config = Config::with_data_dir("/tmp/h4rn-test");
        assert_eq!(
            config.access_token_path(),
            PathBuf::from("/tmp/h4rn-test/access_token")
        );
        assert_eq!(
            config.event_db_path(),
            PathBuf::from("/tmp/h4rn-test/events.db")
        );
        assert_eq!(config.blob_dir(), PathBuf::from("/tmp/h4rn-test/blobs"));
        assert_eq!(
            config.attachments_dir(),
            PathBuf::from("/tmp/h4rn-test/attachments")
        );
    }

    #[test]
    fn default_bind_is_loopback() {
        let config = Config::default();
        assert!(config.bind.ip().is_loopback());
        assert_eq!(config.bind.port(), 47800);
    }
}
