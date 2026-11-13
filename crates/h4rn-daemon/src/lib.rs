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
pub mod blobs;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod doctor;
pub mod fs;
pub mod harnesses;
pub mod hosts;
pub mod journal;
pub mod remote;
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
        Command::Login {
            connect,
            provider,
            name,
        } => login(&config, &connect, &provider, name.as_deref()).await,
        Command::Logout => logout(&config).await,
        Command::Pair { direct, label } => pair(&config, direct, label).await,
    }
}

/// Loads (or creates) the daemon's transport identity.
fn daemon_identity(config: &Config) -> anyhow::Result<h4rn_transport::keys::DeviceIdentity> {
    config.ensure_data_dir()?;
    h4rn_transport::keys::load_or_create(&h4rn_transport::keys::FileKeyStore::new(
        config.transport_identity_path(),
    ))
    .map_err(|error| anyhow::anyhow!("load the transport identity: {error}"))
}

/// Signs the daemon in with a device authorization grant.
async fn login(
    config: &Config,
    connect: &str,
    provider: &str,
    machine_name: Option<&str>,
) -> anyhow::Result<()> {
    let identity = daemon_identity(config)?;
    let client = remote::ConnectClient::new(&config.data_dir, identity)?;
    println!("Requesting a device code from {connect} ...");
    let outcome = client
        .login(connect, provider, machine_name, |summary| {
            println!();
            println!("  Open:  {}", summary.verification_uri);
            if let Some(complete) = &summary.verification_uri_complete {
                println!("  Or:    {complete}");
            }
            println!("  Code:  {}", summary.user_code);
            println!();
            println!(
                "Waiting for approval (code expires in {}s) ...",
                summary.expires_in
            );
        })
        .await?;

    println!();
    match &outcome.email {
        Some(email) => println!("Signed in as {email}."),
        None => println!("Signed in."),
    }
    println!("  device:  {}", outcome.device_id);
    println!("  machine: {}", outcome.machine_id);

    // Register this machine's hosts and address candidates now, so a client
    // sees them before the next `serve`. A failure is only a warning: the
    // daemon republishes on startup.
    let inventory = crate::hosts::HostInventory::detect(std::sync::Arc::new(
        crate::harnesses::StaticHarnessCatalog::new(),
    ));
    if let Err(error) = client
        .update_hosts(crate::daemon::machine_hosts(&inventory))
        .await
    {
        eprintln!("warning: could not publish hosts: {error}");
    }
    let candidates = remote::advertised_addresses(config, Some(connect));
    if let Err(error) = client.update_addresses(candidates).await {
        eprintln!("warning: could not publish addresses: {error}");
    }
    println!();
    println!("Start `h4rnd serve` to come online, then run `h4rnd pair`.");
    Ok(())
}

/// Revokes the daemon's connect session and forgets it locally.
async fn logout(config: &Config) -> anyhow::Result<()> {
    let identity = daemon_identity(config)?;
    let client = remote::ConnectClient::new(&config.data_dir, identity)?;
    if !client.is_logged_in() {
        println!("This daemon is not signed in to h4rn-connect.");
        return Ok(());
    }
    client.logout().await?;
    println!("Signed out of h4rn-connect and removed the local session.");
    Ok(())
}

/// Prints a pairing QR for a new device.
async fn pair(config: &Config, direct: bool, label: Option<String>) -> anyhow::Result<()> {
    if direct {
        return direct_pair(config, label);
    }
    let identity = daemon_identity(config)?;
    let client = remote::ConnectClient::new(&config.data_dir, identity)?;
    if !client.is_logged_in() {
        anyhow::bail!(
            "not signed in to h4rn-connect; run `h4rnd login --connect <url>` or use `--direct`"
        );
    }
    let response = client.create_pairing().await?;
    println!("Scan this code with the h4rn app to pair a device:");
    println!();
    println!("{}", remote::render_qr(&response.qr)?);
    println!();
    println!("  {}", response.qr);
    println!(
        "  Expires {}",
        response.pairing.expires_at.format("%Y-%m-%d %H:%M:%S UTC")
    );
    Ok(())
}

/// Writes a LAN/Tailscale pairing for a daemon that is not signed in.
fn direct_pair(config: &Config, label: Option<String>) -> anyhow::Result<()> {
    use crate::remote::pairing::{
        DEFAULT_DIRECT_TTL_SECS, DirectPairingFile, direct_payload, expiry_hint,
        write_direct_pairing,
    };

    config.ensure_data_dir()?;
    let identity = daemon_identity(config)?;
    let endpoints = remote::advertised_addresses(config, None);
    if endpoints.is_empty() {
        anyhow::bail!(
            "no LAN or Tailscale address found to advertise; pass --extra-bind or use connect pairing"
        );
    }
    let label = label.or_else(|| Some(crate::hosts::hostname()));
    let psk: [u8; h4rn_transport::keys::KEY_LEN] = rand::random();
    let expires_at = chrono::Utc::now().timestamp() + DEFAULT_DIRECT_TTL_SECS;
    let pairing = DirectPairingFile {
        psk: hex::encode(psk),
        expires_at,
        endpoints: endpoints.clone(),
        label: label.clone(),
    };
    let path = write_direct_pairing(&config.data_dir, &pairing)?;
    let payload = direct_payload(&endpoints, identity.public().noise_public, psk, label)?;
    let uri = payload.encode();

    println!("Direct pairing opened until {}.", expiry_hint(expires_at));
    println!();
    println!("{}", remote::render_qr(&uri)?);
    println!();
    println!("  {uri}");
    println!();
    println!("  Endpoints: {}", endpoints.join(", "));
    println!("  State:     {}", path.display());
    println!("A running daemon picks this up within a couple of seconds.");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_not_empty() {
        assert!(!super::version().is_empty());
    }
}
