//! Transport path state surfaced to the UI (ADR-009, ADR-010).
//!
//! `h4rn-transport` maintains one logical session across several candidate
//! paths. The daemon cannot observe the client's path choice, so the client
//! synthesises a [`TransportStatus`] event from its local transport state and
//! feeds it into the same event stream the UI already consumes. Like
//! filesystem hints, these events are **transient**: they carry the current
//! durable sequence number rather than allocating a new one, so they never
//! advance the client's resume cursor.
//!
//! `encrypted` is always `true` for a Noise path and is part of the contract
//! rather than an implication of `state`, because the status bar must be able
//! to say "encrypted" without knowing anything about Noise.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Which kind of physical path carries the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum TransportPathKind {
    /// A LAN or loopback WebSocket straight to the daemon.
    Direct,
    /// A tailnet address (100.64.0.0/10) or MagicDNS name.
    Tailscale,
    /// The self-hosted h4rn-connect relay, which forwards opaque frames.
    Relay,
}

impl TransportPathKind {
    /// Preference rank: lower is better. Direct and Tailscale beat the relay,
    /// and a direct LAN path beats the tailnet because it usually has a lower
    /// round trip.
    pub fn rank(self) -> u8 {
        match self {
            Self::Direct => 0,
            Self::Tailscale => 1,
            Self::Relay => 2,
        }
    }

    /// Stable label for logs and the status bar.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Tailscale => "tailscale",
            Self::Relay => "relay",
        }
    }
}

/// Lifecycle of one candidate path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum TransportPathState {
    /// The socket is opening or the Noise handshake is in flight.
    Probing,
    /// The handshake finished and the path can carry frames, but another path
    /// is active.
    Ready,
    /// The path currently carries application frames.
    Active,
    /// The last probe failed or the peer rejected the path.
    Failed,
    /// The caller disabled the path (for example the relay is not configured).
    Disabled,
}

/// Lifecycle of the logical session across all paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum TransportSessionState {
    /// No path is ready yet.
    Connecting,
    /// At least one path is ready and a path is active.
    Online,
    /// The active path failed and the transport is failing over.
    Degraded,
    /// Every candidate failed or the session was closed.
    Offline,
}

/// One candidate path as the UI sees it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct TransportPathStatus {
    /// Physical path kind.
    pub kind: TransportPathKind,
    /// Human-readable endpoint (`127.0.0.1:47800`, `host.tailnet.ts.net:47800`,
    /// or `relay`). Never contains a token.
    pub endpoint: String,
    /// Path lifecycle.
    pub state: TransportPathState,
    /// Smoothed round-trip time of the most recent probes.
    pub rtt_ms: Option<u32>,
    /// Whether the path carries a Noise-encrypted session. Always `true` for a
    /// reachable path; a future plaintext loopback path would set it `false`.
    pub encrypted: bool,
}

/// The client's current multipath state, delivered as a transient event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct TransportStatus {
    /// Logical session id (hex). Stable across path switches and reconnects.
    pub session_id: String,
    /// Session lifecycle.
    pub state: TransportSessionState,
    /// The path carrying application frames, when one is active.
    pub active: Option<TransportPathStatus>,
    /// Every candidate the transport knows about.
    pub candidates: Vec<TransportPathStatus>,
    /// The active path changed in this update.
    pub switched: bool,
    /// Why the last switch happened, for diagnostics.
    pub detail: Option<String>,
    /// When this snapshot was produced.
    pub updated_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_preference_orders_direct_then_tailscale_then_relay() {
        assert!(TransportPathKind::Direct.rank() < TransportPathKind::Tailscale.rank());
        assert!(TransportPathKind::Tailscale.rank() < TransportPathKind::Relay.rank());
    }

    #[test]
    fn status_round_trips_through_json() {
        let status = TransportStatus {
            session_id: "00112233445566778899aabbccddeeff".into(),
            state: TransportSessionState::Online,
            active: Some(TransportPathStatus {
                kind: TransportPathKind::Tailscale,
                endpoint: "machine.tailnet.ts.net:47800".into(),
                state: TransportPathState::Active,
                rtt_ms: Some(23),
                encrypted: true,
            }),
            candidates: Vec::new(),
            switched: true,
            detail: Some("direct path lost".into()),
            updated_at: Utc::now(),
        };
        let json = serde_json::to_string(&status).unwrap();
        let back: TransportStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, status);
        assert!(json.contains("\"tailscale\""));
    }
}
