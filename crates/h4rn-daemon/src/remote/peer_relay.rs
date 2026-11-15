//! The peer-dialing half of the relay connector (ADR-037).
//!
//! A cross-machine MCP tunnel is a daemon-to-daemon Noise session. h4rn-connect
//! already holds one authenticated WebSocket per daemon, and registering the
//! same device twice would evict the control connection, so the tunnel is
//! multiplexed as a new `conn_id` on that socket. [`RelayDialer`] owns the
//! multiplexing; this adapter names the peer's machine and selects the
//! peer-only [`RelayService::SecureMcp`] route.
//!
//! [`RelayDialer`]: super::relay::RelayDialer

use std::sync::Arc;

use async_trait::async_trait;
use h4rn_protocol::RelayService;
use h4rn_transport::relay::{RelayConnector, RelayError, RelayLink, RelayRequest};

use super::relay::RelayDialer;

/// Opens `/secure-mcp` sessions to one sibling daemon.
pub struct PeerRelayConnector {
    dialer: Arc<RelayDialer>,
    machine_id: String,
}

impl PeerRelayConnector {
    /// Targets `machine_id` through the daemon's shared relay socket.
    pub fn new(dialer: Arc<RelayDialer>, machine_id: impl Into<String>) -> Self {
        Self {
            dialer,
            machine_id: machine_id.into(),
        }
    }
}

impl std::fmt::Debug for PeerRelayConnector {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PeerRelayConnector")
            .field("machine_id", &self.machine_id)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl RelayConnector for PeerRelayConnector {
    async fn connect(&self, request: RelayRequest) -> Result<Box<dyn RelayLink>, RelayError> {
        self.dialer
            .dial(&self.machine_id, RelayService::SecureMcp, request)
            .await
    }
}
