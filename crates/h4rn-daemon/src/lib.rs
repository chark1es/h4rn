//! The per-host h4rn daemon.
//!
//! The daemon owns every behaviour behind the UI (ADR-001): it runs harness
//! sessions, projects their events through the `h4rn-core` lifecycle into the
//! local SQLite journal (ADR-003), and serves one typed WebSocket contract to
//! every client. Machine-specific concerns — the access token, WSL host
//! inventory, harness detection, process supervision — live here rather than in
//! the wire contract.
//!
//! Phase 1b ships:
//! - config, CLI (`serve`, `token`, `doctor`) and data-directory defaults;
//! - 256-bit local access-token authentication for the WebSocket upgrade;
//! - `GET /healthz` and `GET /ws`;
//! - Hello, ListHosts, ListThreads, GetThread, CreateThread, SendMessage,
//!   Interrupt, RespondApproval, SetThreadTab, Snooze and Subscribe;
//! - a thread manager with live harness sessions, durable replay and a
//!   background placement ticker;
//! - per-connection bounded queues with resume hints, and graceful shutdown.

#![deny(missing_docs)]

pub mod auth;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod doctor;
pub mod fs;
pub mod harnesses;
pub mod hosts;
pub mod journal;
pub mod server;
pub mod supervisor;
pub mod terminal;
pub mod threads;

pub use cli::{Cli, Command};
pub use config::Config;
pub use daemon::{BoundDaemon, Daemon, DaemonBuilder};

use crate::auth::TokenStore;

/// Daemon application version.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Initializes `tracing` with the configured filter.
pub fn init_tracing(log_level: &str) {
    let filter = tracing_subscriber::EnvFilter::try_new(log_level)
        .or_else(|_| tracing_subscriber::EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("h4rn_daemon=info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

/// Runs one CLI command.
pub async fn run(config: Config, command: Command) -> anyhow::Result<()> {
    match command {
        Command::Serve => daemon::serve(config).await,
        Command::Token { rotate } => {
            let tokens = TokenStore::new(&config.data_dir);
            let token = if rotate {
                tokens.rotate()?
            } else {
                tokens.load_or_create()?
            };
            // stdout is the token alone so scripts can capture it.
            println!("{token}");
            Ok(())
        }
        Command::Doctor => doctor::run(&config).await,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_not_empty() {
        assert!(!super::version().is_empty());
    }
}
