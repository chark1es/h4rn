//! The daemon-owned network access listener and its direct pairing window.
//!
//! The desktop bundles and starts `h4rnd serve`, but by default the daemon
//! listens only on loopback: another device reaches it through the connect
//! relay, which needs internet and a signed-in session. Network access is the
//! offline alternative. It is a persisted setting owned by the daemon, so a
//! headless `h4rnd serve` and the desktop behave identically.
//!
//! Enabling it binds a **dedicated** wildcard port (default `47810`) rather
//! than the loopback port, which avoids wildcard-vs-`127.0.0.1` bind conflicts
//! and keeps the plaintext `/ws` endpoint off the LAN. Only `/healthz` and the
//! Noise routes `/secure` and `/secure-mcp` are served there; a device still
//! has to complete the encrypted handshake and hold a pairing.
//!
//! The setting is stored as `<data_dir>/network-access.json`
//! (`{ "enabled": bool }`, default `false`). A bind failure never crashes the
//! daemon: the setting stays on and reports the error so the UI can explain it.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::Router;
use h4rn_protocol::{PairScope, ProtoError, ProtoErrorCode};
use h4rn_transport::keys::KEY_LEN;
use h4rn_transport::server::SecureService;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::remote::DeviceRegistry;
use crate::remote::pairing::{
    DEFAULT_DIRECT_TTL_SECS, DirectPairingFile, direct_payload, read_direct_pairing,
    remove_direct_pairing, write_direct_pairing,
};

/// File holding the persisted setting.
pub const NETWORK_ACCESS_FILE: &str = "network-access.json";

/// Port the network listener uses unless configured otherwise.
pub const DEFAULT_NETWORK_PORT: u16 = 47810;

/// Longest the stop path waits for axum's graceful shutdown before it drops
/// the remaining sockets (a live WebSocket never finishes on its own).
const STOP_GRACE: Duration = Duration::from_secs(2);

/// How long a setting change may take to reach the supervisor.
const APPLY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct NetworkAccessFile {
    enabled: bool,
}

/// Path of the persisted setting.
pub fn network_access_path(data_dir: &Path) -> PathBuf {
    data_dir.join(NETWORK_ACCESS_FILE)
}

/// Reads the persisted setting; a missing or unreadable file means "off".
///
/// Failing closed matters: a corrupt file must not expose the daemon.
pub fn read_enabled(data_dir: &Path) -> bool {
    std::fs::read(network_access_path(data_dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<NetworkAccessFile>(&bytes).ok())
        .is_some_and(|file| file.enabled)
}

/// Persists the setting, replacing the file atomically.
pub fn write_enabled(data_dir: &Path, enabled: bool) -> anyhow::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let path = network_access_path(data_dir);
    let temp = path.with_extension("json.tmp");
    let body = serde_json::to_vec(&NetworkAccessFile { enabled })?;
    std::fs::write(&temp, body)?;
    if let Err(error) = std::fs::rename(&temp, &path) {
        // Windows does not replace an existing file on rename.
        std::fs::remove_file(&path).ok();
        std::fs::rename(&temp, &path).map_err(|_| error)?;
    }
    Ok(())
}

/// What a client sees for `GetNetworkAccess`/`SetNetworkAccess`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkAccessSnapshot {
    /// Whether the persisted setting is on.
    pub enabled: bool,
    /// Whether a socket is bound and serving right now.
    pub listening: bool,
    /// Bound port, or the configured one while off.
    pub port: u16,
    /// `host:port` candidates a phone would dial, most preferred first.
    pub addresses: Vec<String>,
    /// Why the listener could not bind, when it could not.
    pub error: Option<String>,
}

/// A direct pairing QR the daemon just armed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectPairingOffer {
    /// `h4rn://pair?...` payload.
    pub uri: String,
    /// Expiry, Unix seconds.
    pub expires_at: i64,
    /// Candidates the QR carries.
    pub addresses: Vec<String>,
    /// Scopes the paired device will hold.
    pub scopes: Vec<PairScope>,
}

#[derive(Clone, Debug, Default)]
struct Status {
    enabled: bool,
    listening: Option<SocketAddr>,
    error: Option<String>,
}

pub(crate) enum Command {
    Set {
        enabled: bool,
        reply: oneshot::Sender<()>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

/// The daemon-wide network access setting and listener supervisor.
///
/// Clone-free: the daemon stores it in `Arc` and hands the same handle to the
/// protocol handlers, the presence publisher and the lifecycle code.
pub struct NetworkAccess {
    data_dir: PathBuf,
    port: u16,
    status: Arc<Mutex<Status>>,
    commands: mpsc::UnboundedSender<Command>,
    secure: Option<Arc<SecureService>>,
    devices: Arc<DeviceRegistry>,
}

impl NetworkAccess {
    /// Creates the handle, its command channel and the shared status.
    ///
    /// Nothing is bound until [`Self::start`] runs, because the supervisor
    /// needs the full router, which in turn holds this handle.
    pub(crate) fn new(
        data_dir: PathBuf,
        port: u16,
        secure: Option<Arc<SecureService>>,
        devices: Arc<DeviceRegistry>,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<Command>) {
        let (commands, receiver) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                data_dir,
                port,
                status: Arc::new(Mutex::new(Status::default())),
                commands,
                secure,
                devices,
            }),
            receiver,
        )
    }

    /// The configured port, even while the listener is off.
    pub fn configured_port(&self) -> u16 {
        self.port
    }

    /// Applies the startup setting and starts the supervisor task.
    ///
    /// The initial bind happens before this returns, so `bind()` hands the
    /// caller a daemon whose status is already accurate.
    pub(crate) async fn start(
        self: &Arc<Self>,
        app: Router,
        enabled: bool,
        commands: mpsc::UnboundedReceiver<Command>,
    ) {
        let mut supervisor = Supervisor {
            app,
            port: self.port,
            status: Arc::clone(&self.status),
            listeners: Vec::new(),
        };
        supervisor.apply(enabled).await;
        tokio::spawn(supervisor.run(commands));
    }

    /// Current state, including the addresses a device would dial.
    pub fn snapshot(&self) -> NetworkAccessSnapshot {
        let status = lock(&self.status).clone();
        let (listening, port) = match status.listening {
            Some(addr) => (true, addr.port()),
            None => (false, self.port),
        };
        let addresses = if listening {
            crate::remote::network::addresses_for_port(port)
        } else {
            Vec::new()
        };
        NetworkAccessSnapshot {
            enabled: status.enabled,
            listening,
            port,
            addresses,
            error: status.error,
        }
    }

    /// Persists and applies a new setting without restarting the daemon.
    pub async fn set_enabled(&self, enabled: bool) -> Result<NetworkAccessSnapshot, ProtoError> {
        write_enabled(&self.data_dir, enabled).map_err(|error| {
            ProtoError::new(
                ProtoErrorCode::Internal,
                format!("the network access setting could not be saved: {error}"),
                "Fix the daemon data directory, then try again",
            )
        })?;
        if !enabled {
            // A pairing window must not outlive the listener that serves it.
            self.close_direct_pairing();
        }
        let (reply, wait) = oneshot::channel();
        if self.commands.send(Command::Set { enabled, reply }).is_err() {
            return Err(ProtoError::new(
                ProtoErrorCode::Internal,
                "the network listener supervisor is not running",
                "Restart the daemon",
            ));
        }
        if tokio::time::timeout(APPLY_TIMEOUT, wait).await.is_err() {
            lock(&self.status).error = Some("the network listener did not apply the change".into());
        }
        Ok(self.snapshot())
    }

    /// Opens (and returns) a one-time direct pairing QR.
    ///
    /// A new offer replaces the previous one: the CLI and the UI must never
    /// arm two secrets at once, because only one can be live in the transport.
    pub fn create_direct_pairing(
        &self,
        requested: Vec<PairScope>,
        label: Option<String>,
    ) -> Result<DirectPairingOffer, ProtoError> {
        let snapshot = self.snapshot();
        if !snapshot.enabled {
            return Err(ProtoError::new(
                ProtoErrorCode::Conflict,
                "Turn on network access first",
                "Enable network access in Settings → Remote, or run `h4rnd serve --network-access on`",
            ));
        }
        if !snapshot.listening || snapshot.addresses.is_empty() {
            let hint = snapshot.error.map_or_else(
                || "Connect this machine to a LAN or tailnet, then try again".to_owned(),
                |error| format!("The network listener is not serving: {error}"),
            );
            return Err(ProtoError::new(
                ProtoErrorCode::Conflict,
                "No network address found",
                hint,
            ));
        }
        let Some(service) = &self.secure else {
            return Err(ProtoError::new(
                ProtoErrorCode::Unsupported,
                "the secure transport is not enabled on this daemon",
                "Enable network access from Settings → Remote, which also enables the noise transport",
            ));
        };

        let addresses = snapshot.addresses;
        let label = label.or_else(|| Some(crate::hosts::hostname()));
        let scopes = crate::direct_scopes(&requested);
        let psk: [u8; KEY_LEN] = rand::random();
        let expires_at = chrono::Utc::now().timestamp() + DEFAULT_DIRECT_TTL_SECS;
        let pairing = DirectPairingFile {
            psk: hex::encode(psk),
            expires_at,
            endpoints: addresses.clone(),
            label: label.clone(),
            scopes: scopes.clone(),
        };
        // Write first so a crash between the two steps leaves a file the
        // watcher can arm, never a live secret with no durable record.
        write_direct_pairing(&self.data_dir, &pairing).map_err(|error| {
            ProtoError::new(
                ProtoErrorCode::Internal,
                format!("the direct pairing could not be written: {error}"),
                "Fix the daemon data directory, then try again",
            )
        })?;
        // Arm through the same path the file watcher uses: the scopes are
        // recorded before the secret can admit a device.
        self.devices.arm_direct(scopes.clone());
        service.set_psk(psk);

        let payload = direct_payload(
            &addresses,
            service.public_identity().noise_public,
            psk,
            label,
        )
        .map_err(|error| {
            ProtoError::new(
                ProtoErrorCode::Internal,
                format!("the direct pairing payload could not be built: {error}"),
                "Disable and enable network access again",
            )
        })?;
        Ok(DirectPairingOffer {
            uri: payload.encode(),
            expires_at,
            addresses,
            scopes,
        })
    }

    /// Closes the open direct pairing, if any.
    pub fn cancel_direct_pairing(&self) {
        self.close_direct_pairing();
    }

    /// Deletes the pairing file and disarms only the secret it carried.
    ///
    /// An operator's `H4RN_PAIRING_PSK` is left armed: it was not this
    /// window's to close.
    fn close_direct_pairing(&self) {
        let pending = read_direct_pairing(&self.data_dir).ok().flatten();
        if let Some(pairing) = pending {
            let psk = pairing.psk().ok();
            if let Err(error) = remove_direct_pairing(&self.data_dir) {
                tracing::warn!(%error, "could not delete the direct pairing file");
            }
            if let (Some(service), Some(psk)) = (&self.secure, psk)
                && service.psk() == Some(psk)
            {
                service.clear_psk();
            }
        }
        self.devices.clear_direct();
    }

    /// Stops the listener before the daemon tears down its services.
    pub async fn shutdown(&self) {
        let (reply, wait) = oneshot::channel();
        if self.commands.send(Command::Shutdown { reply }).is_ok() {
            let _ = tokio::time::timeout(STOP_GRACE, wait).await;
        }
    }
}

struct Running {
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

struct Supervisor {
    app: Router,
    port: u16,
    status: Arc<Mutex<Status>>,
    listeners: Vec<Running>,
}

impl Supervisor {
    async fn run(mut self, mut commands: mpsc::UnboundedReceiver<Command>) {
        while let Some(command) = commands.recv().await {
            match command {
                Command::Set { enabled, reply } => {
                    self.apply(enabled).await;
                    let _ = reply.send(());
                }
                Command::Shutdown { reply } => {
                    self.apply(false).await;
                    let _ = reply.send(());
                    break;
                }
            }
        }
        self.apply(false).await;
    }

    async fn apply(&mut self, enabled: bool) {
        lock(&self.status).enabled = enabled;
        if enabled {
            if self.listeners.is_empty() {
                self.start_listeners().await;
            }
        } else {
            self.stop_listeners().await;
        }
    }

    /// Binds the wildcard listener(s) and spawns one axum server per socket.
    ///
    /// A failure is reported in the status, never propagated: the relay and the
    /// loopback listener keep working.
    async fn start_listeners(&mut self) {
        let primary =
            match TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, self.port))).await {
                Ok(listener) => listener,
                Err(error) => {
                    self.fail(format!(
                        "could not listen on 0.0.0.0:{}: {error}",
                        self.port
                    ));
                    return;
                }
            };
        let addr = match primary.local_addr() {
            Ok(addr) => addr,
            Err(error) => {
                self.fail(format!(
                    "could not read the network listener address: {error}"
                ));
                return;
            }
        };
        let port = addr.port();
        self.listeners
            .push(spawn_listener(primary, self.app.clone()));

        // A second wildcard for IPv6. Best effort: a host with IPv6 disabled,
        // or a dual-stack socket that already covers v6, must not fail the
        // whole listener.
        match TcpListener::bind(SocketAddr::from((Ipv6Addr::UNSPECIFIED, port))).await {
            Ok(listener) => self
                .listeners
                .push(spawn_listener(listener, self.app.clone())),
            Err(error) => tracing::debug!(%error, "no IPv6 network listener"),
        }

        let mut status = lock(&self.status);
        status.listening = Some(addr);
        status.error = None;
        tracing::info!(%addr, "network access listener started");
    }

    async fn stop_listeners(&mut self) {
        let listeners = std::mem::take(&mut self.listeners);
        for listener in &listeners {
            let _ = listener.shutdown.send(true);
        }
        for listener in listeners {
            let mut task = listener.task;
            if tokio::time::timeout(STOP_GRACE, &mut task).await.is_err() {
                tracing::warn!("network listener did not stop gracefully; dropping its sessions");
                task.abort();
                let _ = task.await;
            }
        }
        let mut status = lock(&self.status);
        if status.listening.take().is_some() {
            tracing::info!("network access listener stopped");
        }
        status.error = None;
    }

    fn fail(&mut self, error: String) {
        tracing::warn!(%error, "network access listener failed to bind");
        let mut status = lock(&self.status);
        status.listening = None;
        status.error = Some(error);
    }
}

fn spawn_listener(listener: TcpListener, app: Router) -> Running {
    let (shutdown, mut receiver) = watch::channel(false);
    let task = tokio::spawn(async move {
        let _ = crate::server::serve_listener(listener, app, async move {
            let _ = receiver.changed().await;
        })
        .await;
    });
    Running { shutdown, task }
}

fn lock(status: &Mutex<Status>) -> MutexGuard<'_, Status> {
    status
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Formats one dialable candidate.
///
/// Kept here so tests can build an expected value without re-deriving the
/// bracket rules for IPv6.
pub fn format_address(ip: IpAddr, port: u16) -> String {
    SocketAddr::new(ip, port).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_setting_is_off_and_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!read_enabled(dir.path()));
        std::fs::write(network_access_path(dir.path()), b"not json").unwrap();
        assert!(!read_enabled(dir.path()), "a corrupt file must mean off");
    }

    #[test]
    fn the_setting_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        write_enabled(dir.path(), true).unwrap();
        assert!(read_enabled(dir.path()));
        write_enabled(dir.path(), false).unwrap();
        assert!(!read_enabled(dir.path()));
    }

    #[test]
    fn ipv6_candidates_are_bracketed() {
        assert_eq!(
            format_address("2001:db8::1".parse().unwrap(), 47810),
            "[2001:db8::1]:47810"
        );
        assert_eq!(
            format_address("192.168.1.20".parse().unwrap(), 47810),
            "192.168.1.20:47810"
        );
    }

    #[tokio::test]
    async fn a_pairing_offer_needs_network_access() {
        let dir = tempfile::tempdir().unwrap();
        let (network, _commands) = NetworkAccess::new(
            dir.path().to_path_buf(),
            0,
            None,
            Arc::new(DeviceRegistry::in_memory()),
        );
        let error = network.create_direct_pairing(Vec::new(), None).unwrap_err();
        assert_eq!(error.code, ProtoErrorCode::Conflict);
        assert!(error.message.contains("Turn on network access first"));
    }

    #[tokio::test]
    async fn disabling_closes_the_pairing_file() {
        let dir = tempfile::tempdir().unwrap();
        let pairing = DirectPairingFile {
            psk: "11".repeat(KEY_LEN),
            expires_at: chrono::Utc::now().timestamp() + 60,
            endpoints: vec!["192.168.1.20:47810".into()],
            label: None,
            scopes: PairScope::MINIMAL.to_vec(),
        };
        write_direct_pairing(dir.path(), &pairing).unwrap();
        let (network, _commands) = NetworkAccess::new(
            dir.path().to_path_buf(),
            0,
            None,
            Arc::new(DeviceRegistry::in_memory()),
        );
        network.cancel_direct_pairing();
        assert!(!crate::remote::pairing::direct_pairing_path(dir.path()).exists());
    }
}
