//! Tailscale detection.
//!
//! Tailscale is optional: when it is installed and up, the daemon binds its
//! secure listener on the tailnet address and advertises that address ahead of
//! the relay. Detection is best-effort and never blocks startup; a missing or
//! failing `tailscale` binary simply means "off". `tailscale status --json` is
//! preferred because it also names the node, and `tailscale ip -4` is the
//! fallback for older CLIs.

use std::net::IpAddr;
use std::process::Command;

use serde::Deserialize;

/// How the daemon should treat Tailscale.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TailscaleMode {
    /// Detect and use Tailscale when available (default).
    #[default]
    Auto,
    /// Never bind or advertise a tailnet address.
    Off,
}

impl TailscaleMode {
    /// Parses `auto` or `off`.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" | "on" | "1" | "true" => Some(Self::Auto),
            "off" | "0" | "false" | "none" => Some(Self::Off),
            _ => None,
        }
    }

    /// Stable spelling for diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Off => "off",
        }
    }
}

/// What Tailscale reports about this node.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TailscaleStatus {
    /// Stable node name, when the status JSON names one.
    pub hostname: Option<String>,
    /// Tailnet IPv4 address.
    pub ipv4: Option<IpAddr>,
    /// Whether the backend reports the node online and logged in.
    pub online: bool,
}

impl TailscaleStatus {
    /// Whether a usable tailnet address was found.
    pub fn usable(&self) -> bool {
        self.online && self.ipv4.is_some()
    }
}

/// Detects the local Tailscale node, trying the platform binaries in order.
pub fn detect() -> Option<TailscaleStatus> {
    status_json().or_else(ip_command)
}

/// The tailnet IPv4 address, when Tailscale is up.
pub fn tailnet_ip() -> Option<IpAddr> {
    let status = detect()?;
    status
        .usable()
        .then_some(status.ipv4?)
        .filter(|ip| !ip.is_loopback())
}

/// Runs `tailscale status --json` and parses the self node.
fn status_json() -> Option<TailscaleStatus> {
    let output = run(&["status", "--json"])?;
    parse_status_json(&output)
}

/// Parses the subset of `tailscale status --json` the daemon needs.
pub fn parse_status_json(text: &str) -> Option<TailscaleStatus> {
    #[derive(Deserialize)]
    struct Wrapper {
        #[serde(rename = "BackendState", default)]
        backend_state: Option<String>,
        #[serde(rename = "Self", default)]
        this: Option<Node>,
    }
    #[derive(Deserialize)]
    struct Node {
        #[serde(rename = "HostName", default)]
        hostname: Option<String>,
        #[serde(rename = "DNSName", default)]
        dns_name: Option<String>,
        #[serde(rename = "TailscaleIPs", default)]
        ips: Vec<String>,
        #[serde(rename = "Online", default)]
        online: Option<bool>,
    }

    let wrapper: Wrapper = serde_json::from_str(text).ok()?;
    let backend = wrapper.backend_state.unwrap_or_default();
    let node = wrapper.this?;
    let ipv4 = node
        .ips
        .iter()
        .find_map(|value| value.parse::<IpAddr>().ok())
        .filter(|ip| ip.is_ipv4());
    Some(TailscaleStatus {
        hostname: node.hostname.or_else(|| {
            node.dns_name
                .map(|name| name.trim_end_matches('.').to_owned())
        }),
        ipv4,
        online: node.online.unwrap_or(false)
            && matches!(backend.as_str(), "Running" | "NeedsLogin" | ""),
    })
}

/// Runs `tailscale ip -4` for older CLIs without a usable status JSON.
fn ip_command() -> Option<TailscaleStatus> {
    let output = run(&["ip", "-4"])?;
    let ipv4 = output
        .lines()
        .find_map(|line| line.trim().parse::<IpAddr>().ok())
        .filter(|ip| ip.is_ipv4());
    Some(TailscaleStatus {
        hostname: None,
        ipv4,
        online: true,
    })
}

fn run(args: &[&str]) -> Option<String> {
    for binary in ["tailscale", "tailscale.exe"] {
        let Ok(output) = Command::new(binary).args(args).output() else {
            continue;
        };
        if output.status.success() {
            return Some(String::from_utf8_lossy(&output.stdout).into_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_parse_strictly() {
        assert_eq!(TailscaleMode::parse("AUTO"), Some(TailscaleMode::Auto));
        assert_eq!(TailscaleMode::parse("off"), Some(TailscaleMode::Off));
        assert_eq!(TailscaleMode::parse("sometimes"), None);
    }

    #[test]
    fn status_json_yields_the_tailnet_address() {
        let status = parse_status_json(
            r#"{
              "BackendState": "Running",
              "Self": {
                "HostName": "studio",
                "DNSName": "studio.tailnet.ts.net.",
                "TailscaleIPs": ["100.64.1.5", "fd7a:115c:a1e0::5"],
                "Online": true
              }
            }"#,
        )
        .unwrap();
        assert_eq!(status.ipv4, Some("100.64.1.5".parse().unwrap()));
        assert_eq!(status.hostname.as_deref(), Some("studio"));
        assert!(status.usable());
    }

    #[test]
    fn a_stopped_node_is_not_usable() {
        let status = parse_status_json(
            r#"{"BackendState": "Stopped", "Self": {"TailscaleIPs": ["100.64.1.5"], "Online": false}}"#,
        )
        .unwrap();
        assert!(!status.usable());
    }
}
