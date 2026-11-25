//! The self-hostable h4rn relay (ADR-018).
//!
//! `h4rn-connect` routes opaque Noise frames between paired devices and
//! daemons, authenticates users over OIDC, and persists accounts, pairings,
//! environment links and the synced event log in Postgres (ADR-010, ADR-012).
//!
//! The crate is split so the wire contract stays testable without a socket:
//!
//! - [`config`] loads deployment configuration from TOML and the environment;
//! - [`crypto`] owns the KEK, envelope encryption and pairing-secret hashing;
//! - [`jwt`] issues and verifies h4rn's EdDSA session tokens;
//! - [`dpop`] verifies the per-request proof that binds a token to a device key;
//! - [`oidc`] performs discovery, the authorization-code flow and the device grant;
//! - [`store`] is the Postgres system of record;
//! - [`pairing`] creates and claims `h4rn://pair?...` payloads;
//! - [`relay`] routes opaque frames and broadcasts presence;
//! - [`api`] exposes the REST surface and serves the built web UI.

use std::net::SocketAddr;

pub mod alerts;
pub mod api;
pub mod config;
pub mod crypto;
pub mod dpop;
pub mod error;
pub mod jwt;
pub mod oidc;
pub mod pairing;
pub mod push;
pub mod rate_limit;
pub mod relay;
pub mod store;

pub use api::{AppState, build_state, router};
pub use config::Config;
pub use error::{AppError, Result};

/// Default listen address for the relay.
pub fn default_addr() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], 8080))
}

/// Serves the relay until the process is stopped.
pub async fn serve(addr: SocketAddr, state: AppState) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "h4rn-connect listening");
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
}

#[cfg(test)]
mod tests {
    #[test]
    fn default_address_is_the_relay_port() {
        assert_eq!(super::default_addr().port(), 8080);
    }
}
