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

    /// Let other devices reach this daemon directly on the LAN or tailnet.
    ///
    /// Persisted in the data directory; `h4rnd pair --direct` needs it on.
    #[arg(
        long,
        global = true,
        value_name = "on|off",
        env = "H4RN_NETWORK_ACCESS",
        value_parser = parse_switch
    )]
    pub network_access: Option<bool>,

    /// Port for the direct network listener (wildcard, Noise routes only).
    #[arg(long, global = true, value_name = "PORT", env = "H4RN_NETWORK_PORT")]
    pub network_port: Option<u16>,

    /// Log filter directive.
    #[arg(long, global = true, value_name = "DIRECTIVE", env = "H4RN_LOG")]
    pub log_level: Option<String>,

    /// Finish an interrupted update and exit (used by the Windows logon task).
    #[arg(long, hide = true)]
    pub recover_only: bool,

    /// What to do.
    #[command(subcommand)]
    pub command: Option<Command>,
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
        /// Scope to grant the new device (repeatable): view, approve, terminal,
        /// files, admin. `view` is always granted. Over connect the default is
        /// everything but admin. With `--direct` and no `--scope` the device
        /// gets only `view` and `approve`; add `--scope terminal`, `--scope
        /// files` or `--scope admin` to widen it.
        #[arg(long = "scope", value_name = "SCOPE", value_parser = parse_scope)]
        scopes: Vec<h4rn_protocol::PairScope>,
    },
    /// Run h4rnd as a per-user background service.
    Service {
        /// What to do with the service.
        #[command(subcommand)]
        command: crate::service::ServiceCommand,
    },
    /// Install the latest signed release, or roll back the last update.
    Update {
        /// Print the current and latest versions without changing anything.
        #[arg(long)]
        check: bool,
        /// Install this exact version instead of the latest release.
        #[arg(long, value_name = "X.Y.Z")]
        version: Option<String>,
        /// Restore the binary saved as h4rnd.previous by the last update.
        #[arg(long)]
        rollback: bool,
        /// Update even while a turn is active or a running daemon cannot be
        /// reached or report its turns. Also applies to --rollback.
        #[arg(long)]
        force: bool,
    },
}

fn parse_scope(value: &str) -> Result<h4rn_protocol::PairScope, String> {
    h4rn_protocol::PairScope::parse(&value.to_ascii_lowercase()).ok_or_else(|| {
        format!("unknown scope `{value}`; expected view, approve, terminal, files or admin")
    })
}

fn parse_switch(value: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "1" | "yes" => Ok(true),
        "off" | "false" | "0" | "no" => Ok(false),
        _ => Err(format!("expected on or off, not {value:?}")),
    }
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
        if let Some(network_access) = self.network_access {
            config.network_access = Some(network_access);
        }
        if let Some(port) = self.network_port {
            if port == 0 {
                anyhow::bail!("--network-port must be between 1 and 65535");
            }
            config.network_port = port;
        }
        if let Some(log_level) = self.log_level {
            config.log_level = log_level;
        }
        let command = self
            .command
            .ok_or_else(|| anyhow::anyhow!("a subcommand is required (try `h4rnd --help`)"))?;
        Ok((config, command))
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

    #[test]
    fn network_access_flags_apply_to_every_subcommand() {
        let cli = Cli::try_parse_from([
            "h4rnd",
            "--network-access",
            "on",
            "--network-port",
            "47910",
            "pair",
            "--direct",
        ])
        .unwrap();
        let (config, command) = cli.into_parts().unwrap();
        assert_eq!(config.network_access, Some(true));
        assert_eq!(config.network_port, 47910);
        assert!(matches!(command, Command::Pair { direct: true, .. }));
    }

    #[test]
    fn an_unknown_network_access_value_is_rejected() {
        assert!(Cli::try_parse_from(["h4rnd", "--network-access", "maybe", "serve"]).is_err());
        assert!(
            Cli::try_parse_from(["h4rnd", "--network-port", "0", "serve"])
                .unwrap()
                .into_parts()
                .is_err()
        );
    }

    #[test]
    fn service_and_update_subcommands_parse() {
        let cli = Cli::try_parse_from(["h4rnd", "service", "install", "--force"]).unwrap();
        assert!(matches!(
            cli.into_parts().unwrap().1,
            Command::Service {
                command: crate::service::ServiceCommand::Install { force: true }
            }
        ));

        let cli = Cli::try_parse_from([
            "h4rnd",
            "update",
            "--check",
            "--version",
            "1.2.3",
            "--force",
        ])
        .unwrap();
        assert!(matches!(
            cli.into_parts().unwrap().1,
            Command::Update {
                check: true,
                version: Some(version),
                rollback: false,
                force: true,
            } if version == "1.2.3"
        ));
    }

    #[test]
    fn recover_only_is_a_hidden_top_level_flag() {
        let cli = Cli::try_parse_from(["h4rnd", "--recover-only"]).unwrap();
        assert!(cli.recover_only);
        assert!(cli.command.is_none());
        let help = Cli::try_parse_from(["h4rnd", "--help"])
            .unwrap_err()
            .to_string();
        assert!(
            !help.contains("recover-only"),
            "the internal flag stays out of help output"
        );
    }
}
