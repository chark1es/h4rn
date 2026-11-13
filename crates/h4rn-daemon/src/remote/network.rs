//! LAN address discovery and the machine address list advertised to connect.
//!
//! The daemon cannot discover every NAT mapping, so it advertises the addresses
//! it can prove it is bound to: a non-loopback LAN address, the tailnet address
//! when Tailscale is up, and the relay. A client probes them in order and falls
//! back to the relay, which is always advertised while the daemon is signed in.

use std::net::{IpAddr, SocketAddr, UdpSocket};

use crate::config::Config;

/// The host's primary non-loopback IPv4 address, discovered without sending
/// traffic by asking the routing table which source address reaches a public
/// address. Returns `None` on a host with no default route.
pub fn local_ipv4() -> Option<IpAddr> {
    for probe in ["8.8.8.8:80", "1.1.1.1:80"] {
        let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
        if socket.connect(probe).is_err() {
            continue;
        }
        let Ok(local) = socket.local_addr() else {
            continue;
        };
        if let IpAddr::V4(ip) = local.ip()
            && !ip.is_loopback()
            && !ip.is_unspecified()
        {
            return Some(IpAddr::V4(ip));
        }
    }
    None
}

/// Every candidate a client should try, most preferred first.
///
/// `relay_url` is the connect base URL; it is rendered as `relay:<url>` so a
/// client knows to open an authenticated relay session rather than dial a
/// WebSocket at that path.
pub fn advertised_addresses(config: &Config, relay_url: Option<&str>) -> Vec<String> {
    let port = config.bind.port();
    let mut out = Vec::new();
    let mut push = |address: String| {
        if !address.is_empty() && !out.contains(&address) {
            out.push(address);
        }
    };

    if let Some(extra) = config.extra_bind
        && !extra.ip().is_loopback()
    {
        push(extra.to_string());
    }
    if let Some(ip) = local_ipv4() {
        push(SocketAddr::new(ip, port).to_string());
    }
    if let Some(ip) = super::tailscale::tailnet_ip() {
        push(SocketAddr::new(ip, port).to_string());
    }
    if let Some(url) = relay_url {
        push(format!("relay:{}", url.trim_end_matches('/')));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_is_advertised_last_and_once() {
        let mut config = Config::with_data_dir("/tmp/h4rn-net");
        config.bind = "127.0.0.1:47800".parse().unwrap();
        let addresses = advertised_addresses(&config, Some("https://connect.example.com/"));
        assert_eq!(
            addresses.last().map(String::as_str),
            Some("relay:https://connect.example.com")
        );
        assert_eq!(
            addresses
                .iter()
                .filter(|address| address.starts_with("relay:"))
                .count(),
            1
        );
    }
}
