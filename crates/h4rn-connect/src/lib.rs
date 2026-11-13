//! The self-hostable h4rn relay (ADR-018).
//!
//! `h4rn-connect` routes opaque Noise frames between paired devices and
//! daemons, authenticates users over OIDC, and persists accounts, pairings,
//! environment links and the synced event log in Postgres (ADR-010, ADR-012).
//! Phase 1a ships a health endpoint and the deployment skeleton; routing,
//! pairing and persistence arrive in later phases.

use std::net::SocketAddr;

use axum::Router;
use axum::routing::get;

/// Default listen address for the relay.
pub fn default_addr() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], 8080))
}

/// Builds the relay router.
///
/// Only liveness exists in Phase 1a. Keeping the router as a function makes it
/// testable without binding a socket.
pub fn router() -> Router {
    Router::new().route("/healthz", get(|| async { "ok" }))
}

/// Serves the relay until the process is stopped.
pub async fn serve(addr: SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "h4rn-connect listening");
    axum::serve(listener, router()).await
}

#[cfg(test)]
mod tests {
    #[test]
    fn default_address_is_the_relay_port() {
        assert_eq!(super::default_addr().port(), 8080);
    }
}
