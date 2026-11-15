//! Sibling daemons of the signed-in account, from h4rn-connect.
//!
//! The MCP sharing target needs three facts about each other daemon: which
//! host id a share names, its pinned Noise key and where to dial it. connect
//! already holds all three (the account's devices, machines, hosts and the
//! advertised addresses). The refresh is triggered by relay events (snapshot,
//! presence, pairing, revocation) rather than a timer.

use std::sync::Arc;

use h4rn_protocol::{ConnectDeviceKind, HostId, MachineSummary};
use h4rn_transport::keys::KEY_LEN;

use super::{ConnectClient, DeviceRegistry};
use crate::hosts::host_id_for;
use crate::mcp_peers::{Peer, PeerDirectory};

/// Source name under which connect reports peers.
const SOURCE: &str = "connect";

/// Rebuilds the connect-sourced peers whenever the relay signals a change.
pub async fn run_peer_sync(
    client: Arc<ConnectClient>,
    peers: Arc<PeerDirectory>,
    devices: Arc<DeviceRegistry>,
) {
    // `notify_one` stores a permit, so the first pass runs immediately.
    devices.topology().notify_one();
    loop {
        devices.topology().notified().await;
        match refresh(&client).await {
            Ok(found) => peers.replace_source(SOURCE, found),
            Err(error) => tracing::debug!(%error, "could not refresh peer daemons from connect"),
        }
    }
}

async fn refresh(client: &ConnectClient) -> anyhow::Result<Vec<Peer>> {
    let me = client.me().await?;
    let machines = client.machines().await?;
    let own = client.machine_id();
    Ok(peers_from(&me, &machines, own.as_deref()))
}

/// Maps connect's device and machine lists onto the peer directory.
///
/// The peer daemon's connect device id is carried into [`Peer::device_id`] so
/// `McpSharePolicy::allowed_devices` compares the id the owner recorded for
/// that daemon, never a caller-supplied claim. The `relay:` marker is split
/// out of the address list: the daemon opens relay sessions through its own
/// authenticated socket, so it needs the connect base URL, not a `ws://`
/// candidate.
fn peers_from(
    me: &h4rn_protocol::ConnectMeResponse,
    machines: &[h4rn_protocol::MachineSummary],
    own: Option<&str>,
) -> Vec<Peer> {
    let mut peers = Vec::new();
    for device in &me.devices {
        if device.kind != ConnectDeviceKind::Daemon || device.revoked_at.is_some() {
            continue;
        }
        let Some(machine_id) = device.machine_id.clone() else {
            continue;
        };
        if own == Some(machine_id.as_str()) {
            continue;
        }
        let Some(key) = device
            .noise_static_public_key
            .as_deref()
            .and_then(decode_key)
        else {
            continue;
        };
        let Some(machine) = machines.iter().find(|machine| machine.id == machine_id) else {
            continue;
        };
        let relay = machine
            .addresses
            .iter()
            .find_map(|address| address.strip_prefix("relay:").map(str::to_owned));
        peers.push(Peer {
            host: primary_host(machine),
            name: machine.name.clone(),
            noise_public: key,
            endpoints: machine
                .addresses
                .iter()
                .filter(|address| !address.starts_with("relay:"))
                .cloned()
                .collect(),
            machine_id: Some(machine_id),
            relay,
            device_id: Some(device.id.clone()),
            online: machine.online,
        });
    }
    peers
}

/// The host id of the daemon's own host: connect lists it first.
fn primary_host(machine: &MachineSummary) -> HostId {
    match machine.hosts.first() {
        Some(host) => host_id_for(&host.name, host.distro.as_deref()),
        None => host_id_for(&machine.name, None),
    }
}

fn decode_key(value: &str) -> Option<[u8; KEY_LEN]> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value.trim())
        .ok()?;
    bytes.as_slice().try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use h4rn_protocol::{ConnectDevice, ConnectMeResponse, ConnectUser};

    fn device(
        id: &str,
        kind: ConnectDeviceKind,
        machine: &str,
        key: [u8; KEY_LEN],
        revoked: bool,
    ) -> ConnectDevice {
        ConnectDevice {
            id: id.into(),
            kind,
            name: id.into(),
            machine_id: Some(machine.into()),
            proof_public_key: "proof".into(),
            noise_static_public_key: Some(
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key),
            ),
            platform: None,
            created_at: chrono::Utc::now(),
            last_seen: None,
            revoked_at: revoked.then(chrono::Utc::now),
            scopes: Vec::new(),
        }
    }

    fn machine(id: &str, online: bool, addresses: &[&str]) -> h4rn_protocol::MachineSummary {
        h4rn_protocol::MachineSummary {
            id: id.into(),
            name: id.into(),
            online,
            last_seen: None,
            daemon_device_id: None,
            hosts: Vec::new(),
            addresses: addresses
                .iter()
                .map(|address| address.to_string())
                .collect(),
        }
    }

    #[test]
    fn discovery_carries_the_peer_device_id_machine_and_relay() {
        let me = ConnectMeResponse {
            user: ConnectUser {
                id: "user_1".into(),
                email: None,
                display_name: None,
                avatar_url: None,
            },
            devices: vec![
                device(
                    "dev_b",
                    ConnectDeviceKind::Daemon,
                    "machine_b",
                    [7; KEY_LEN],
                    false,
                ),
                device(
                    "dev_self",
                    ConnectDeviceKind::Daemon,
                    "machine_a",
                    [8; KEY_LEN],
                    false,
                ),
                device(
                    "dev_client",
                    ConnectDeviceKind::Desktop,
                    "machine_b",
                    [9; KEY_LEN],
                    false,
                ),
                device(
                    "dev_revoked",
                    ConnectDeviceKind::Daemon,
                    "machine_b",
                    [10; KEY_LEN],
                    true,
                ),
            ],
        };
        let machines = vec![
            machine(
                "machine_b",
                true,
                &["192.168.1.9:7000", "relay:https://connect.example"],
            ),
            machine("machine_a", true, &["192.168.1.10:7000"]),
        ];
        let peers = peers_from(&me, &machines, Some("machine_a"));
        assert_eq!(
            peers.len(),
            1,
            "only the other live daemon is a peer: {peers:?}"
        );
        let peer = &peers[0];
        assert_eq!(peer.device_id.as_deref(), Some("dev_b"));
        assert_eq!(peer.machine_id.as_deref(), Some("machine_b"));
        assert_eq!(peer.relay.as_deref(), Some("https://connect.example"));
        assert_eq!(peer.endpoints, ["192.168.1.9:7000"]);
        assert_eq!(peer.noise_public, [7; KEY_LEN]);
        assert!(peer.online);
    }
}
