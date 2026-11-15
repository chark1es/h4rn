//! Host inventory: this machine plus any WSL distributions (ADR-019).
//!
//! A daemon is scoped to one host. When it runs on Windows it can still see
//! the WSL distributions installed on the machine, and when it runs inside WSL
//! the distribution itself is the host. Each entry gets its own stable id so
//! `machine:<id>` blobatar seeds never alias the Windows host with a distro.

use std::process::Command;

use chrono::Utc;
use h4rn_protocol::{HarnessKind, HostId, HostSummary};

use crate::harnesses::HarnessCatalog;

/// Resolved host inventory for one daemon.
#[derive(Clone)]
pub struct HostInventory {
    local: HostSummary,
    extra: Vec<HostSummary>,
    catalog: std::sync::Arc<dyn HarnessCatalog>,
}

impl HostInventory {
    /// Detects this host and any WSL distributions.
    pub fn detect(catalog: std::sync::Arc<dyn HarnessCatalog>) -> Self {
        let now = Utc::now();
        let distro = wsl_distro_name();
        let local_platform = local_platform(distro.as_deref());
        let local_id = if distro.is_some() {
            HostId::new(format!(
                "host_wsl_{}",
                slug(&distro.clone().unwrap_or_default())
            ))
        } else {
            HostId::new(format!("host_{}", slug(&hostname())))
        };

        let local = HostSummary {
            host_id: local_id.clone(),
            name: display_host_name(distro.as_deref()),
            platform: local_platform,
            online: true,
            last_seen: now,
            harnesses: catalog.available(),
            latency_ms: None,
            path: None,
        };

        let mut extra = Vec::new();
        if cfg!(windows) {
            for distro in windows_wsl_distros() {
                let host_id = HostId::new(format!("host_wsl_{}", slug(&distro)));
                if host_id == local_id {
                    continue;
                }
                extra.push(HostSummary {
                    host_id,
                    name: distro.clone(),
                    platform: format!("wsl:{distro}"),
                    // Seeing a distro in `wsl.exe -l -q` does not prove a
                    // daemon is running inside it.
                    online: false,
                    last_seen: now,
                    harnesses: Vec::new(),
                    latency_ms: None,
                    path: None,
                });
            }
        }

        Self {
            local,
            extra,
            catalog,
        }
    }

    /// Replaces the detected local host id and name (embedders and tests that
    /// run several daemons on one machine).
    pub fn with_local(mut self, id: HostId, name: impl Into<String>) -> Self {
        self.local.host_id = id;
        self.local.name = name.into();
        self
    }

    /// The id of the host this daemon process runs on.
    pub fn local_id(&self) -> HostId {
        self.local.host_id.clone()
    }

    /// Whether `host` is the daemon's own host.
    pub fn is_local(&self, host: &HostId) -> bool {
        &self.local.host_id == host
    }

    /// Whether `host` is a known (but not necessarily reachable) host.
    pub fn contains(&self, host: &HostId) -> bool {
        self.is_local(host) || self.extra.iter().any(|entry| &entry.host_id == host)
    }

    /// Whether a known host is a WSL distribution entry rather than this host.
    pub fn is_remote_entry(&self, host: &HostId) -> bool {
        !self.is_local(host) && self.extra.iter().any(|entry| &entry.host_id == host)
    }

    /// Current host summaries, with fresh `last_seen` and harness lists.
    pub fn summaries(&self) -> Vec<HostSummary> {
        let now = Utc::now();
        let mut local = self.local.clone();
        local.online = true;
        local.last_seen = now;
        local.harnesses = self.catalog.available();

        let mut summaries = Vec::with_capacity(1 + self.extra.len());
        summaries.push(local);
        summaries.extend(self.extra.iter().cloned().map(|mut entry| {
            entry.last_seen = now;
            entry
        }));
        summaries
    }

    /// Harnesses available on this host.
    pub fn available_harnesses(&self) -> Vec<HarnessKind> {
        self.catalog.available()
    }
}

/// The id a daemon derives for a host that another daemon reported to connect
/// (`name`, plus the distribution name for a WSL host). It mirrors
/// [`HostInventory::detect`], so both sides agree on who `to_machine` names.
pub fn host_id_for(name: &str, distro: Option<&str>) -> HostId {
    match distro {
        Some(distro) => HostId::new(format!("host_wsl_{}", slug(distro))),
        None => HostId::new(format!("host_{}", slug(name))),
    }
}

/// The daemon host name, without domain decoration.
pub fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|value| value.trim().to_owned())
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "local".to_owned())
}

/// The WSL distribution name when this process runs inside WSL.
pub fn wsl_distro_name() -> Option<String> {
    if let Ok(value) = std::env::var("WSL_DISTRO_NAME") {
        let value = value.trim();
        if !value.is_empty() {
            return Some(value.to_owned());
        }
    }

    if cfg!(target_os = "linux") {
        let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .or_else(|_| std::fs::read_to_string("/proc/version"))
            .unwrap_or_default()
            .to_ascii_lowercase();
        if release.contains("microsoft") || release.contains("wsl") {
            return Some(distro_from_os_release().unwrap_or_else(|| "wsl".to_owned()));
        }
    }
    None
}

/// Whether the process is running inside WSL.
pub fn is_wsl() -> bool {
    wsl_distro_name().is_some()
}

/// Lists installed WSL distributions via `wsl.exe -l -q` on Windows.
///
/// `wsl.exe` writes UTF-16LE on most builds and UTF-8 on some, so the decoder
/// sniffs NUL bytes instead of assuming either.
pub fn windows_wsl_distros() -> Vec<String> {
    if !cfg!(windows) {
        return Vec::new();
    }
    let output = Command::new("wsl.exe").args(["-l", "-q"]).output();
    let Ok(output) = output else {
        return Vec::new();
    };
    let text = decode_wsl_output(&output.stdout);
    text.lines()
        .map(|line| line.trim_matches(|c: char| c == '\0' || c.is_whitespace()))
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Lists all WSL distributions relevant to this process.
///
/// On Windows this shells out; inside WSL it reports the current distro. On
/// other platforms it is empty.
pub fn wsl_distros() -> Vec<String> {
    if cfg!(windows) {
        return windows_wsl_distros();
    }
    wsl_distro_name().into_iter().collect()
}

/// Decodes `wsl.exe` output, which may be UTF-16LE or UTF-8.
pub fn decode_wsl_output(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes.iter().filter(|byte| **byte == 0).count() > bytes.len() / 4 {
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    String::from_utf8_lossy(bytes).into_owned()
}

fn local_platform(distro: Option<&str>) -> String {
    if let Some(distro) = distro {
        return format!("wsl:{distro}");
    }
    if cfg!(target_os = "windows") {
        "windows".to_owned()
    } else if cfg!(target_os = "macos") {
        "macos".to_owned()
    } else if cfg!(target_os = "linux") {
        "linux".to_owned()
    } else {
        std::env::consts::OS.to_owned()
    }
}

fn display_host_name(distro: Option<&str>) -> String {
    match distro {
        Some(distro) => distro.to_owned(),
        None => hostname(),
    }
}

fn distro_from_os_release() -> Option<String> {
    let contents = std::fs::read_to_string("/etc/os-release").ok()?;
    for line in contents.lines() {
        if let Some(value) = line.strip_prefix("PRETTY_NAME=") {
            let value = value.trim_matches('"');
            if !value.is_empty() {
                return Some(value.to_owned());
            }
        }
    }
    None
}

fn slug(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    out.trim_matches('_').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harnesses::StaticHarnessCatalog;

    #[test]
    fn slugs_are_stable_and_lowercase() {
        assert_eq!(slug("Charks-Laptop"), "charks_laptop");
        assert_eq!(slug("Ubuntu 24.04"), "ubuntu_24_04");
        assert_eq!(slug(""), "");
    }

    #[test]
    fn utf16_output_is_decoded() {
        let text = "Ubuntu\r\nDebian\r\n";
        let units: Vec<u8> = text
            .encode_utf16()
            .flat_map(|unit| unit.to_le_bytes())
            .collect();
        assert_eq!(decode_wsl_output(&units), text);
        assert_eq!(decode_wsl_output(b"Ubuntu\n"), "Ubuntu\n");
    }

    #[test]
    fn local_host_is_always_present_and_online() {
        let catalog = std::sync::Arc::new(StaticHarnessCatalog::new());
        let inventory = HostInventory::detect(catalog);
        let hosts = inventory.summaries();
        assert!(!hosts.is_empty());
        assert!(hosts[0].online);
        assert_eq!(hosts[0].host_id, inventory.local_id());
        assert!(inventory.is_local(&inventory.local_id()));
    }
}
