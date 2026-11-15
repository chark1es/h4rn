//! Cross-machine MCP sharing, the target-machine half (ADR-037).
//!
//! The owner (machine A) already stores shares, enforces policy and serves
//! `/secure-mcp`. This module is everything machine B needs to *use* a share:
//!
//! 1. [`PeerDirectory`] — the sibling daemons B may talk to: their pinned
//!    Noise key and the endpoints to reach them. Production fills it from
//!    h4rn-connect (`remote::peers`); embedders and tests can add peers
//!    directly with [`PeerDirectory::replace_source`].
//! 2. Grant discovery — B dials each online peer's `/secure-mcp` with a
//!    `grants` handshake. The peer authenticates B by its daemon key, derives
//!    B's machine from that key (never from anything B says) and **pushes** the
//!    full grant set: once on connect and again whenever a share is added,
//!    changed or removed, or B stops being a peer. Nothing polls.
//! 3. Registration — for every granted server B builds a
//!    [`TransportMcpTunnel`] to the owner and calls
//!    [`McpHub::register_remote`], so B's harnesses see the tools. A dropped
//!    grant, a dropped connection or a removed peer unregisters them again.
//!
//! Wire shape on the owner's `/secure-mcp` route, after the Noise handshake:
//!
//! ```text
//! B -> A   {"h4rn_mcp_grants": "h4rn-mcp-grants-1"}
//! A -> B   {"grants": [{"server": "github", "policy": {..}}]}      (a full snapshot, repeated on every change)
//! ```

use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use h4rn_mcp::hub::McpHub;
use h4rn_mcp::sharing::McpTunnel;
use h4rn_protocol::{HostId, McpSharePolicy, TransportPathKind};
use h4rn_transport::AppFrame;
use h4rn_transport::client::{Client, ClientConfig, ClientEvent, ClientPath};
use h4rn_transport::engine::EngineConfig;
use h4rn_transport::keys::{DeviceIdentity, KEY_LEN};
use h4rn_transport::server::SecureService;
use h4rn_transport::wire::SESSION_ID_LEN;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;

use crate::mcp::{TransportMcpTunnel, TunnelConnection};
use crate::remote::{DeviceRegistry, PeerRelayConnector, RelayDialer, Role};

/// Marker for the grant-discovery handshake on `/secure-mcp`.
pub(crate) const GRANTS_HANDSHAKE: &str = "h4rn-mcp-grants-1";

/// How long B waits for the owner while establishing a grants session.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// First and last retry delay after a grants session drops.
const MIN_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// One sibling daemon this machine may exchange MCP shares with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    /// The peer's host id, which is what a `Share { to_machine }` names.
    pub host: HostId,
    /// Display name.
    pub name: String,
    /// The peer daemon's pinned X25519 static key.
    pub noise_public: [u8; KEY_LEN],
    /// `ip:port` candidates for the peer's listener, preferred first. LAN and
    /// tailnet addresses; the relay is carried separately because it is opened
    /// as an authenticated session on this daemon's existing relay socket.
    pub endpoints: Vec<String>,
    /// The peer's h4rn-connect machine id, needed to open a relay session.
    pub machine_id: Option<String>,
    /// Advertised relay base URL (`relay:<url>` without the prefix), when the
    /// peer published one.
    pub relay: Option<String>,
    /// The peer daemon's connect device id. This is the value
    /// `McpSharePolicy::allowed_devices` compares; it is registered as the
    /// `Host` entry's device id.
    pub device_id: Option<String>,
    /// Whether the peer is believed reachable. An offline peer is not dialled.
    pub online: bool,
}

/// The sibling daemons of this account, by source.
///
/// The directory owns the peer daemons' entries in the device registry and
/// the secure-transport allowlist. A peer that appears is registered as a
/// `Host` (provenance connect, no client scopes) and allowed to complete a
/// handshake; a peer that disappears (machine or device revoked) is removed
/// from the registry, revoked and its live sessions are closed. A registered
/// host is only ever accepted on `/secure-mcp`.
pub struct PeerDirectory {
    sources: RwLock<BTreeMap<String, Vec<Peer>>>,
    secure: RwLock<Option<Arc<SecureService>>>,
    devices: RwLock<Option<Arc<DeviceRegistry>>>,
    version: watch::Sender<u64>,
}

impl Default for PeerDirectory {
    fn default() -> Self {
        Self::new()
    }
}

impl PeerDirectory {
    /// An empty directory.
    pub fn new() -> Self {
        Self {
            sources: RwLock::new(BTreeMap::new()),
            secure: RwLock::new(None),
            devices: RwLock::new(None),
            version: watch::channel(0).0,
        }
    }

    /// Points the directory at the transport whose allowlist it maintains.
    pub fn attach_secure(&self, service: Arc<SecureService>) {
        *self
            .secure
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(service);
    }

    /// Points the directory at the registry that records peer daemons as hosts
    /// (and registers the peers it already knows).
    pub fn attach_devices(&self, devices: Arc<DeviceRegistry>) {
        *self
            .devices
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(devices);
        self.apply(&[], &self.peers());
    }

    /// Replaces every peer a named source reported. Sources are independent
    /// (`connect`, `static`, ...); the directory serves their union.
    pub fn replace_source(&self, source: &str, peers: Vec<Peer>) {
        let before = self.peers();
        {
            let mut sources = self
                .sources
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if peers.is_empty() {
                sources.remove(source);
            } else {
                sources.insert(source.to_owned(), peers);
            }
        }
        let after = self.peers();
        if before == after {
            return;
        }
        self.apply(&before, &after);
        self.version.send_modify(|version| *version += 1);
    }

    /// Mirrors a change of the peer set into the device registry and the
    /// transport allowlist.
    fn apply(&self, before: &[Peer], after: &[Peer]) {
        let devices = self
            .devices
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let service = self
            .secure
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        for peer in after {
            let known = before.iter().any(|old| old == peer);
            if known {
                continue;
            }
            if let Some(devices) = &devices {
                // A key that already holds client scopes is never silently
                // re-roled into a peer daemon.
                if devices
                    .authorization(&peer.noise_public)
                    .is_some_and(|auth| auth.role == Role::Client)
                {
                    tracing::warn!(
                        peer = %peer.host,
                        "a peer daemon key is already a paired client; not registering it as a host"
                    );
                    continue;
                }
                devices.register_host(
                    peer.host.clone(),
                    peer.noise_public,
                    peer.device_id.clone(),
                );
            }
            if let Some(service) = &service {
                service.allow(peer.noise_public);
            }
        }
        for old in before {
            if after
                .iter()
                .any(|peer| peer.noise_public == old.noise_public)
            {
                continue;
            }
            if let Some(devices) = &devices
                && devices
                    .authorization(&old.noise_public)
                    .is_some_and(|auth| auth.role == Role::Host)
            {
                devices.remove_key(&old.noise_public);
            }
            if let Some(service) = &service {
                service.revoke(old.noise_public);
            }
        }
    }

    /// Every known peer, sorted by host id; the first source wins a host id.
    pub fn peers(&self) -> Vec<Peer> {
        let sources = self
            .sources
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut merged: BTreeMap<HostId, Peer> = BTreeMap::new();
        for peer in sources.values().flatten() {
            merged
                .entry(peer.host.clone())
                .or_insert_with(|| peer.clone());
        }
        merged.into_values().collect()
    }

    /// A receiver that changes whenever the peer set does.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.version.subscribe()
    }
}

/// One grant on the wire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireGrant {
    server: String,
    #[serde(default)]
    policy: McpSharePolicy,
}

/// Owner side: serves one grants session until the peer goes away, is revoked
/// or the connection drops.
///
/// `machine` is the verified identity of the caller, resolved by the device
/// registry from its Noise key before this runs. `still_peer` re-checks that
/// identity on every change so a revoked or re-roled key stops receiving.
pub(crate) async fn serve_grants(
    hub: Arc<McpHub>,
    peers: Arc<PeerDirectory>,
    machine: HostId,
    still_peer: impl Fn() -> bool,
    sender: h4rn_transport::server::SessionSender,
    mut frames: tokio::sync::mpsc::Receiver<AppFrame>,
) {
    let mut grants_rx = hub.subscribe_grants();
    let mut peers_rx = peers.subscribe();
    let mut last: Option<Vec<WireGrant>> = None;
    loop {
        // Mark both channels seen *before* reading state so a change that
        // lands while we send is never lost.
        grants_rx.borrow_and_update();
        peers_rx.borrow_and_update();
        if !still_peer() {
            tracing::info!(%machine, "closing a grants session: the peer was removed");
            sender.close();
            return;
        }
        let snapshot = match hub.grants_for(&machine).await {
            Ok(grants) => grants
                .into_iter()
                .map(|(server, policy)| WireGrant { server, policy })
                .collect::<Vec<_>>(),
            Err(error) => {
                tracing::warn!(%error, "could not read the share grants");
                return;
            }
        };
        if last.as_ref() != Some(&snapshot) {
            let frame = json!({ "grants": snapshot });
            if sender.send(AppFrame::text(frame.to_string())).is_err() {
                return;
            }
            last = Some(snapshot);
        }
        tokio::select! {
            changed = grants_rx.changed() => if changed.is_err() { return },
            changed = peers_rx.changed() => if changed.is_err() { return },
            frame = frames.recv() => if frame.is_none() { return },
        }
    }
}

/// Whether `addr` is in the CGNAT range Tailscale hands out.
fn is_tailnet(addr: &str) -> bool {
    let host = addr.rsplit_once(':').map_or(addr, |(host, _)| host);
    matches!(host.parse::<IpAddr>(), Ok(IpAddr::V4(ip))
        if ip.octets()[0] == 100 && (64..128).contains(&ip.octets()[1]))
}

/// The `ws://` URLs for a peer's endpoint candidates.
pub(crate) fn endpoint_urls(peer: &Peer) -> Vec<String> {
    peer.endpoints
        .iter()
        .filter(|endpoint| !endpoint.starts_with("relay:"))
        .map(|endpoint| format!("ws://{endpoint}/secure-mcp"))
        .collect()
}

/// Every candidate path to a peer, most preferred first.
///
/// Direct and tailnet sockets are probed in parallel by the engine, which
/// ranks the relay last. The relay path is only built when both the peer's
/// connect machine id and this daemon's shared relay socket are known.
pub(crate) fn peer_paths(
    peer: &Peer,
    relay: Option<&Arc<RelayDialer>>,
) -> Vec<ClientPath> {
    let mut paths: Vec<ClientPath> = endpoint_urls(peer)
        .into_iter()
        .map(|url| {
            let host = url
                .trim_start_matches("ws://")
                .split('/')
                .next()
                .unwrap_or_default();
            let kind = if is_tailnet(host) {
                TransportPathKind::Tailscale
            } else {
                TransportPathKind::Direct
            };
            ClientPath::websocket(kind, url)
        })
        .collect();
    if let (Some(dialer), Some(machine)) = (relay, peer.machine_id.as_deref()) {
        let connector = Arc::new(PeerRelayConnector::new(Arc::clone(dialer), machine));
        paths.push(ClientPath::relay(
            connector,
            peer.relay.clone().unwrap_or_else(|| "relay".into()),
            None,
        ));
    }
    paths
}

/// Opens an authenticated secure session to a peer's `/secure-mcp`, probing
/// every candidate endpoint through the multipath engine.
pub(crate) async fn establish(
    paths: Vec<ClientPath>,
    identity: &DeviceIdentity,
    daemon_public: [u8; KEY_LEN],
) -> Result<Client, String> {
    if paths.is_empty() {
        return Err("the peer has no reachable endpoint".into());
    }
    let session_id: [u8; SESSION_ID_LEN] = rand::random();
    let engine = EngineConfig::new(session_id, identity.clone(), daemon_public);
    let mut client =
        Client::connect(ClientConfig::new(engine, paths)).map_err(|error| error.to_string())?;
    loop {
        match tokio::time::timeout(CONNECT_TIMEOUT, client.recv()).await {
            Ok(Some(ClientEvent::Established(_))) => return Ok(client),
            Ok(Some(ClientEvent::Closed { reason })) => {
                return Err(format!("the secure MCP tunnel closed: {reason}"));
            }
            Ok(Some(_)) => {}
            Ok(None) => return Err("the secure MCP client stopped during connect".into()),
            Err(_) => return Err("timed out opening the secure MCP tunnel".into()),
        }
    }
}

/// A tunnel that always opens `remote`, whatever local alias it is registered
/// under (an alias differs when the owner's server name is already taken here).
struct AliasTunnel {
    inner: TransportMcpTunnel,
    remote: String,
}

#[async_trait::async_trait]
impl McpTunnel for AliasTunnel {
    async fn open(
        &self,
        _server: &str,
    ) -> Result<Arc<dyn h4rn_mcp::sharing::TunnelSession>, h4rn_mcp::upstream::UpstreamError> {
        self.inner.open(&self.remote).await
    }
}

/// Everything one machine B needs to follow its peers' grants.
pub(crate) struct PeerLinks {
    hub: Arc<McpHub>,
    peers: Arc<PeerDirectory>,
    identity: DeviceIdentity,
    local_host: HostId,
    /// The daemon's shared relay socket, when h4rn-connect is configured.
    /// Peer sessions are multiplexed over it as a fallback path.
    relay: Option<Arc<RelayDialer>>,
}

/// A running per-peer task.
struct PeerTask {
    peer: Peer,
    stop: watch::Sender<bool>,
    handle: JoinHandle<()>,
}

impl PeerLinks {
    pub(crate) fn new(
        hub: Arc<McpHub>,
        peers: Arc<PeerDirectory>,
        identity: DeviceIdentity,
        local_host: HostId,
        relay: Option<Arc<RelayDialer>>,
    ) -> Self {
        Self {
            hub,
            peers,
            identity,
            local_host,
            relay,
        }
    }

    /// Keeps one grants task per online peer until `stop` fires. A changed or
    /// removed peer stops its task, which unregisters everything it had
    /// registered.
    pub(crate) async fn run(self, mut stop: watch::Receiver<bool>) {
        let mut peers_rx = self.peers.subscribe();
        let mut tasks: HashMap<HostId, PeerTask> = HashMap::new();
        loop {
            peers_rx.borrow_and_update();
            let wanted: Vec<Peer> = self
                .peers
                .peers()
                .into_iter()
                .filter(|peer| peer.online && peer.host != self.local_host)
                .collect();
            let stale: Vec<HostId> = tasks
                .iter()
                .filter(|(host, task)| {
                    wanted
                        .iter()
                        .find(|peer| &peer.host == *host)
                        .is_none_or(|peer| peer != &task.peer)
                })
                .map(|(host, _)| host.clone())
                .collect();
            for host in stale {
                if let Some(task) = tasks.remove(&host) {
                    let _ = task.stop.send(true);
                    let _ = task.handle.await;
                }
            }
            for peer in wanted {
                if tasks.contains_key(&peer.host) {
                    continue;
                }
                let (stop_tx, stop_rx) = watch::channel(false);
                let handle = tokio::spawn(run_peer(
                    peer.clone(),
                    Arc::clone(&self.hub),
                    self.identity.clone(),
                    self.local_host.clone(),
                    self.relay.clone(),
                    stop_rx,
                ));
                tasks.insert(
                    peer.host.clone(),
                    PeerTask {
                        peer,
                        stop: stop_tx,
                        handle,
                    },
                );
            }
            tokio::select! {
                changed = peers_rx.changed() => if changed.is_err() { break },
                _ = stop.changed() => break,
            }
        }
        for (_, task) in tasks.drain() {
            let _ = task.stop.send(true);
            let _ = task.handle.await;
        }
    }
}

/// Follows one peer: connect, mirror its grants, clean up, back off, repeat.
async fn run_peer(
    peer: Peer,
    hub: Arc<McpHub>,
    identity: DeviceIdentity,
    local_host: HostId,
    relay: Option<Arc<RelayDialer>>,
    mut stop: watch::Receiver<bool>,
) {
    let registered: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
    let mut backoff = MIN_BACKOFF;
    loop {
        let outcome = tokio::select! {
            outcome = follow(&peer, &hub, &identity, &local_host, &registered, relay.as_ref()) => outcome,
            _ = stop.changed() => Err(String::new()),
        };
        // A closed session or a stop both drop everything the peer granted;
        // a reconnect starts from the peer's fresh snapshot.
        for (_, alias) in registered.lock().await.drain() {
            hub.unregister_remote(&alias).await;
        }
        if *stop.borrow() {
            return;
        }
        match outcome {
            Ok(()) => backoff = MIN_BACKOFF,
            Err(error) => tracing::debug!(peer = %peer.host, %error, "grants session ended"),
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = stop.changed() => return,
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

async fn follow(
    peer: &Peer,
    hub: &Arc<McpHub>,
    identity: &DeviceIdentity,
    local_host: &HostId,
    registered: &Arc<Mutex<HashMap<String, String>>>,
    relay: Option<&Arc<RelayDialer>>,
) -> Result<(), String> {
    let paths = peer_paths(peer, relay);
    let mut client = establish(paths.clone(), identity, peer.noise_public).await?;
    client
        .send(AppFrame::text(
            json!({ "h4rn_mcp_grants": GRANTS_HANDSHAKE }).to_string(),
        ))
        .await
        .map_err(|error| error.to_string())?;
    loop {
        match client.recv().await {
            Some(ClientEvent::Frame(frame)) => {
                let Some(text) = frame.as_text() else {
                    continue;
                };
                let Ok(value) = serde_json::from_str::<Value>(text) else {
                    continue;
                };
                let Some(grants) = value.get("grants").cloned() else {
                    continue;
                };
                let Ok(grants) = serde_json::from_value::<Vec<WireGrant>>(grants) else {
                    continue;
                };
                reconcile(peer, hub, identity, local_host, registered, &paths, &grants).await;
            }
            Some(ClientEvent::Closed { reason }) => return Err(reason.to_string()),
            Some(_) => {}
            None => return Err("the grants session stopped".into()),
        }
    }
}

/// Makes the hub's remote bindings for `peer` match `grants` exactly.
async fn reconcile(
    peer: &Peer,
    hub: &Arc<McpHub>,
    identity: &DeviceIdentity,
    local_host: &HostId,
    registered: &Arc<Mutex<HashMap<String, String>>>,
    paths: &[ClientPath],
    grants: &[WireGrant],
) {
    let mut registered = registered.lock().await;
    let wanted: Vec<&str> = grants.iter().map(|grant| grant.server.as_str()).collect();
    let gone: Vec<String> = registered
        .keys()
        .filter(|server| !wanted.contains(&server.as_str()))
        .cloned()
        .collect();
    for server in gone {
        if let Some(alias) = registered.remove(&server) {
            hub.unregister_remote(&alias).await;
            tracing::info!(peer = %peer.host, %server, "a shared MCP server was withdrawn");
        }
    }
    for grant in grants {
        if registered.contains_key(&grant.server) {
            continue;
        }
        // A local server, or another peer's server, may already own the name.
        let alias = if hub.has_local_server(&grant.server).await || hub.is_remote(&grant.server) {
            format!(
                "{}-{}",
                grant.server,
                peer.host
                    .as_str()
                    .trim_start_matches("host_")
                    .replace('_', "-")
            )
        } else {
            grant.server.clone()
        };
        let tunnel = AliasTunnel {
            inner: TransportMcpTunnel::new(TunnelConnection {
                paths: paths.to_vec(),
                identity: identity.clone(),
                daemon_public: peer.noise_public,
                machine: local_host.clone(),
                device_id: None,
            }),
            remote: grant.server.clone(),
        };
        hub.register_remote(alias.clone(), peer.host.clone(), Arc::new(tunnel))
            .await;
        tracing::info!(peer = %peer.host, server = %grant.server, %alias, "registered a shared MCP server");
        registered.insert(grant.server.clone(), alias);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(host: &str, key: u8) -> Peer {
        Peer {
            host: HostId::new(host),
            name: host.into(),
            noise_public: [key; KEY_LEN],
            endpoints: vec!["10.0.0.2:7000".into(), "relay:https://c.example".into()],
            machine_id: Some("machine_a".into()),
            relay: Some("https://c.example".into()),
            device_id: None,
            online: true,
        }
    }

    #[test]
    fn peers_register_as_scopeless_hosts_and_sources_are_independent() {
        let directory = PeerDirectory::new();
        let devices = Arc::new(DeviceRegistry::in_memory());
        directory.attach_devices(Arc::clone(&devices));
        let changes = directory.subscribe();
        directory.replace_source("connect", vec![peer("host_a", 1)]);
        directory.replace_source("static", vec![peer("host_b", 2)]);
        assert!(changes.has_changed().unwrap());
        let a = devices.authorization(&[1; KEY_LEN]).expect("registered");
        assert_eq!(a.role, Role::Host);
        assert!(a.scopes.is_empty(), "a peer daemon holds no client scopes");
        assert_eq!(a.machine, Some(HostId::new("host_a")));
        assert_eq!(
            devices.scopes_for_key(&[1; KEY_LEN]),
            None,
            "a host is not a client"
        );
        assert!(devices.authorization(&[3; KEY_LEN]).is_none());
        directory.replace_source("connect", Vec::new());
        assert!(devices.authorization(&[1; KEY_LEN]).is_none());
        assert!(devices.authorization(&[2; KEY_LEN]).is_some());
        assert_eq!(directory.peers().len(), 1);
    }

    #[test]
    fn a_paired_client_key_is_never_reroled_into_a_peer() {
        let directory = PeerDirectory::new();
        let devices = Arc::new(DeviceRegistry::in_memory());
        devices.insert_direct([1; KEY_LEN], vec![h4rn_protocol::PairScope::View]);
        directory.attach_devices(Arc::clone(&devices));
        directory.replace_source("connect", vec![peer("host_a", 1)]);
        assert_eq!(
            devices.authorization(&[1; KEY_LEN]).unwrap().role,
            Role::Client
        );
        directory.replace_source("connect", Vec::new());
        assert!(
            devices.authorization(&[1; KEY_LEN]).is_some(),
            "removing a peer must not delete a client entry"
        );
    }

    #[test]
    fn endpoints_skip_the_relay_and_tailnet_addresses_are_detected() {
        assert_eq!(
            endpoint_urls(&peer("host_a", 1)),
            vec!["ws://10.0.0.2:7000/secure-mcp".to_owned()]
        );
        assert!(is_tailnet("100.101.102.103:7000"));
        assert!(!is_tailnet("100.200.1.1:7000"));
        assert!(!is_tailnet("192.168.1.2:7000"));
    }
}
