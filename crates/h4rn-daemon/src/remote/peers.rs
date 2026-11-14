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
    let mut peers = Vec::new();
    for device in me.devices {
        if device.kind != ConnectDeviceKind::Daemon || device.revoked_at.is_some() {
            continue;
        }
        let Some(machine_id) = device.machine_id.clone() else {
            continue;
        };
        if Some(&machine_id) == own.as_ref() {
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
            online: machine.online,
        });
    }
    Ok(peers)
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
