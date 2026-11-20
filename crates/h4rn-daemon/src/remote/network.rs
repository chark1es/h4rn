//! LAN address discovery and the machine address list advertised to connect.
//!
//! The daemon cannot discover every NAT mapping, so it advertises the addresses
//! it can prove it is bound to: the network listener's interface addresses
//! (LAN and tailnet), and the relay. A client probes them in order and falls
//! back to the relay, which is always advertised while the daemon is signed in.
//!
//! Discovery never needs a default route: it enumerates the host's interfaces,
//! so an isolated LAN still yields the address a phone must dial. The
//! `8.8.8.8` route probe is kept only to *order* the list (the routing table's
//! preferred address first), never to find addresses in the first place.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};

use crate::config::Config;
use crate::remote::TailscaleMode;

/// The host's primary non-loopback IPv4 address, as the routing table would
/// use it for outbound traffic.
///
/// This sends no traffic (a UDP `connect` only consults the routing table) and
/// returns `None` on a host with no default route. It is a hint for ordering,
/// not the discovery mechanism: see [`interface_addresses`].
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

/// Every usable non-loopback address on this host, most preferred first.
///
/// IPv4 comes before IPv6 unless the routing table prefers an IPv4 address, in
/// which case that address leads. Link-local addresses (IPv4 `169.254/16` and
/// IPv6 `fe80::/10`), loopback interfaces and obvious container/bridge
/// interfaces are skipped; tailnet addresses (`tailscale0`, `utun*`) are kept.
///
/// An empty result is normal on a host with only loopback, so callers must
/// treat "no address" as a state rather than an error.
pub fn interface_addresses() -> Vec<IpAddr> {
    ordered_addresses(enumerate_interfaces(), local_ipv4())
}

/// Orders reported interfaces into the advertised list.
///
/// Split out so "no interfaces" is testable: an empty input yields an empty
/// list rather than an error or a panic.
fn ordered_addresses(interfaces: Vec<(String, IpAddr)>, preferred: Option<IpAddr>) -> Vec<IpAddr> {
    let mut physical = Vec::new();
    let mut virtual_ = Vec::new();
    for (name, ip) in interfaces {
        if !is_usable(ip) {
            continue;
        }
        let bucket = if is_virtual_interface(&name) {
            &mut virtual_
        } else {
            &mut physical
        };
        if !bucket.contains(&ip) {
            bucket.push(ip);
        }
    }

    // A container-heavy host may have nothing but bridge addresses; they are
    // still better than refusing to pair, so fall back to them.
    let mut candidates = physical;
    if candidates.is_empty() {
        candidates = virtual_;
    }
    // Stable split keeps IPv4 ahead of IPv6 while preserving interface order.
    candidates.sort_by_key(|ip| match ip {
        IpAddr::V4(_) => 0,
        IpAddr::V6(_) => 1,
    });

    let mut out = Vec::new();
    if let Some(ip) = preferred
        && is_usable(ip)
        && !out.contains(&ip)
    {
        out.push(ip);
    }
    for ip in candidates {
        if !out.contains(&ip) {
            out.push(ip);
        }
    }
    out
}

/// The dial candidates a device should try for a listener on `port`.
///
/// IPv6 addresses render as `[addr]:port`, which is what `SocketAddr`'s
/// `Display` produces and what the pairing payload expects.
pub fn addresses_for_port(port: u16) -> Vec<String> {
    interface_addresses()
        .into_iter()
        .map(|ip| SocketAddr::new(ip, port).to_string())
        .collect()
}

/// Whether an address is worth advertising.
fn is_usable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            !ip.is_loopback() && !ip.is_unspecified() && !ip.is_link_local() && !ip.is_broadcast()
        }
        IpAddr::V6(ip) => {
            !ip.is_loopback()
                && !ip.is_unspecified()
                // fe80::/10 link-local
                && (ip.segments()[0] & 0xffc0) != 0xfe80
                // ff00::/8 multicast
                && (ip.segments()[0] & 0xff00) != 0xff00
                // IPv4-mapped addresses belong on the IPv4 list.
                && !matches!(ip.to_ipv4_mapped(), Some(mapped) if !is_usable(IpAddr::V4(mapped)))
        }
    }
}

/// Container and bridge interfaces that are not reachable from another
/// device. Cheap name matching; an unusual hypervisor may still leak one.
fn is_virtual_interface(name: &str) -> bool {
    const PREFIXES: [&str; 8] = [
        "docker", "veth", "virbr", "br-", "cni", "podman", "lxc", "vmnet",
    ];
    PREFIXES.iter().any(|prefix| name.starts_with(prefix))
}

/// Every `(interface name, address)` pair the OS reports.
#[cfg(unix)]
fn enumerate_interfaces() -> Vec<(String, IpAddr)> {
    use std::ffi::CStr;

    let mut out = Vec::new();
    // SAFETY: `getifaddrs` initialises the list head and `freeifaddrs` releases
    // it once; every pointer dereference below is checked for null first.
    unsafe {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut head) != 0 {
            return out;
        }
        let mut current = head;
        while !current.is_null() {
            let entry = &*current;
            let name = if entry.ifa_name.is_null() {
                String::new()
            } else {
                CStr::from_ptr(entry.ifa_name)
                    .to_string_lossy()
                    .into_owned()
            };
            let loopback = entry.ifa_flags & libc::IFF_LOOPBACK as u32 != 0;
            let up = entry.ifa_flags & libc::IFF_UP as u32 != 0;
            if !entry.ifa_addr.is_null() && up && !loopback {
                let family = (*entry.ifa_addr).sa_family as i32;
                if family == libc::AF_INET {
                    let addr = &*(entry.ifa_addr as *const libc::sockaddr_in);
                    out.push((
                        name,
                        IpAddr::V4(Ipv4Addr::from(addr.sin_addr.s_addr.to_ne_bytes())),
                    ));
                } else if family == libc::AF_INET6 {
                    let addr = &*(entry.ifa_addr as *const libc::sockaddr_in6);
                    out.push((name, IpAddr::V6(Ipv6Addr::from(addr.sin6_addr.s6_addr))));
                }
            }
            current = entry.ifa_next;
        }
        libc::freeifaddrs(head);
    }
    out
}

/// Platforms without `getifaddrs` fall back to the route probe.
#[cfg(not(unix))]
fn enumerate_interfaces() -> Vec<(String, IpAddr)> {
    match local_ipv4() {
        Some(ip) => vec![(String::new(), ip)],
        None => Vec::new(),
    }
}

/// Every candidate a client should try, most preferred first.
///
/// `relay_url` is the connect base URL; it is rendered as `relay:<url>` so a
/// client knows to open an authenticated relay session rather than dial a
/// WebSocket at that path.
///
/// LAN candidates are advertised only while network access is on, and always on
/// the network listener's port: an address on the loopback port would be a
/// dead end. An explicit `--extra-bind` is advertised as given, and the relay
/// entry is unchanged.
pub fn advertised_addresses(config: &Config, relay_url: Option<&str>) -> Vec<String> {
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
    if config.network_access_enabled() {
        if config.network_port != 0 {
            for address in addresses_for_port(config.network_port) {
                push(address);
            }
        }
    } else if config.tailscale == TailscaleMode::Auto
        && let Some(ip) = super::tailscale::tailnet_ip()
    {
        // Tailscale keeps its own listener on the primary port even without
        // network access, because the tailnet is already authenticated.
        push(SocketAddr::new(ip, config.bind.port()).to_string());
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
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = Config::with_data_dir(dir.path());
        config.bind = "127.0.0.1:47800".parse().unwrap();
        config.tailscale = TailscaleMode::Off;
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

    #[test]
    fn the_network_port_is_advertised_only_when_access_is_on() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = Config::with_data_dir(dir.path());
        config.tailscale = TailscaleMode::Off;
        config.network_port = 47810;
        assert!(
            advertised_addresses(&config, None).is_empty(),
            "network access is off by default"
        );

        crate::network_access::write_enabled(dir.path(), true).expect("persist");
        for address in advertised_addresses(&config, None) {
            assert!(
                address.ends_with(":47810"),
                "a LAN candidate must name the network port: {address}"
            );
        }
        // The loopback port is never a LAN candidate.
        assert!(
            !advertised_addresses(&config, None)
                .iter()
                .any(|address| address.ends_with(":47800"))
        );
    }

    #[test]
    fn interface_discovery_does_not_panic_and_skips_link_local() {
        for ip in interface_addresses() {
            assert!(is_usable(ip), "{ip} must not be advertised");
            match ip {
                IpAddr::V4(ip) => assert!(!ip.is_loopback() && !ip.is_link_local()),
                IpAddr::V6(ip) => assert!(!ip.is_loopback()),
            }
        }
    }

    #[test]
    fn a_host_with_no_interfaces_yields_an_empty_list() {
        assert!(ordered_addresses(Vec::new(), None).is_empty());
        // Loopback and link-local addresses alone are not candidates.
        let only_unusable = vec![
            ("lo".to_owned(), "127.0.0.1".parse().unwrap()),
            ("eth0".to_owned(), "169.254.10.1".parse().unwrap()),
            ("eth0".to_owned(), "::1".parse().unwrap()),
        ];
        assert!(ordered_addresses(only_unusable, None).is_empty());
    }

    #[test]
    fn the_routing_preference_leads_but_never_replaces_collected_addresses() {
        let interfaces = vec![
            ("eth0".to_owned(), "10.0.0.5".parse().unwrap()),
            ("wlan0".to_owned(), "192.168.1.20".parse().unwrap()),
            ("tailscale0".to_owned(), "100.64.1.5".parse().unwrap()),
            ("fe80::1".to_owned(), "fe80::1".parse().unwrap()),
            ("docker0".to_owned(), "172.17.0.1".parse().unwrap()),
        ];
        let preferred: IpAddr = "192.168.1.20".parse().unwrap();
        assert_eq!(
            ordered_addresses(interfaces.clone(), Some(preferred)),
            vec![
                preferred,
                "10.0.0.5".parse().unwrap(),
                "100.64.1.5".parse().unwrap(),
            ],
            "the preferred address leads, virtual and link-local stay out"
        );
        // With no default route the collected addresses still work.
        assert_eq!(
            ordered_addresses(interfaces, None),
            vec![
                "10.0.0.5".parse::<IpAddr>().unwrap(),
                "192.168.1.20".parse::<IpAddr>().unwrap(),
                "100.64.1.5".parse::<IpAddr>().unwrap(),
            ]
        );
    }

    #[test]
    fn addresses_for_port_renders_dialable_sockets() {
        for address in addresses_for_port(47810) {
            let parsed: SocketAddr = address.parse().expect("a dialable socket address");
            assert_eq!(parsed.port(), 47810);
            assert!(!parsed.ip().is_loopback());
        }
    }

    #[test]
    fn virtual_interfaces_are_recognised() {
        assert!(is_virtual_interface("docker0"));
        assert!(is_virtual_interface("veth1a2b"));
        assert!(is_virtual_interface("br-9f8e"));
        assert!(is_virtual_interface("virbr0"));
        assert!(!is_virtual_interface("eth0"));
        assert!(!is_virtual_interface("tailscale0"));
        assert!(!is_virtual_interface("wlan0"));
    }
}
