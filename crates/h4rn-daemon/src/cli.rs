//! Command-line surface for `h4rnd`.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::config::{Config, parse_addr};

/// The h4rn per-host daemon.
#[derive(Clone, Debug, Parser)]
#[command(
    name = "h4rnd",
    version,
    about = "Run the local h4rn daemon, inspect it, or manage its access token"
)]
pub struct Cli {
    /// Data directory (default: platform application data directory).
    #[arg(long, global = true, value_name = "PATH", env = "H4RN_DATA_DIR")]
    pub data_dir: Option<PathBuf>,

    /// Primary listen address.
    #[arg(long, global = true, value_name = "ADDR", env = "H4RN_BIND")]
    pub bind: Option<String>,

    /// Extra listen address, for example a tailnet IP.
    #[arg(long, global = true, value_name = "ADDR", env = "H4RN_EXTRA_BIND")]
    pub extra_bind: Option<String>,

    /// Tailscale handling: `auto` (detect and use) or `off`.
    #[arg(long, global = true, value_name = "MODE", env = "H4RN_TAILSCALE")]
    pub tailscale: Option<String>,

    /// Log filter directive.
    #[arg(long, global = true, value_name = "DIRECTIVE", env = "H4RN_LOG")]
    pub log_level: Option<String>,

    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// Daemon subcommands.
#[derive(Clone, Debug, Subcommand)]
pub enum Command {
    /// Start the daemon and listen for clients.
    Serve,
    /// Print the local access token, optionally rotating it first.
    Token {
        /// Replace the token before printing it.
        #[arg(long)]
        rotate: bool,
    },
    /// Report hosts, harnesses, WSL distributions and state paths.
    Doctor,
    /// Sign this daemon in to an h4rn-connect deployment.
    Login {
        /// Public base URL of the connect deployment.
        #[arg(long, value_name = "URL", env = "H4RN_CONNECT_URL")]
        connect: String,
        /// OIDC provider id configured on the deployment.
        #[arg(long, default_value = "orbitid", value_name = "ID")]
        provider: String,
        /// Machine name shown to other devices.
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
    },
    /// Revoke this daemon's connect session and forget it locally.
    Logout,
    /// Pair a new device: over connect by default, or directly over LAN/tailnet.
    Pair {
        /// Skip connect and pair over a LAN or Tailscale endpoint.
        #[arg(long)]
        direct: bool,
        /// Label printed on the QR (defaults to the host name).
        #[arg(long, value_name = "NAME")]
        label: Option<String>,
    },
}

impl Cli {
    /// Resolves configuration and returns the subcommand.
    pub fn into_parts(self) -> Result<(Config, Command)> {
        let mut config = Config::default();
        if let Some(data_dir) = self.data_dir {
            config.data_dir = data_dir;
        }
        if let Some(bind) = self.bind {
            config.bind = parse_addr(&bind)?;
        }
        if let Some(extra_bind) = self.extra_bind {
            config.extra_bind = Some(parse_addr(&extra_bind)?);
        }
        if let Some(tailscale) = self.tailscale {
            config.tailscale =
                crate::remote::TailscaleMode::parse(&tailscale).ok_or_else(|| {
                    anyhow::anyhow!(
                        "invalid tailscale mode {tailscale:?}; expected `auto` or `off`"
                    )
                })?;
        }
        if let Some(log_level) = self.log_level {
            config.log_level = log_level;
        }
        Ok((config, self.command))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_flags_apply_to_every_subcommand() {
        let cli = Cli::try_parse_from([
            "h4rnd",
            "--data-dir",
            "/tmp/h4rn-cli",
            "--bind",
            "127.0.0.1:0",
            "token",
            "--rotate",
        ])
        .unwrap();
        let (config, command) = cli.into_parts().unwrap();
        assert_eq!(config.data_dir, PathBuf::from("/tmp/h4rn-cli"));
        assert_eq!(config.bind.port(), 0);
        assert!(matches!(command, Command::Token { rotate: true }));
    }

    #[test]
    fn invalid_bind_is_rejected() {
        let cli = Cli::try_parse_from(["h4rnd", "--bind", "nonsense", "serve"]).unwrap();
        assert!(cli.into_parts().is_err());
    }
}
