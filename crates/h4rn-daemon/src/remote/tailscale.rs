//! Tailscale detection.
//!
//! Tailscale is optional: when it is installed and up, the daemon binds its
//! secure listener on the tailnet address and advertises that address ahead of
//! the relay. Detection is best-effort and never blocks startup; a missing or
//! failing `tailscale` binary simply means "off". `tailscale status --json` is
//! preferred because it also names the node, and `tailscale ip -4` is the
//! fallback for older CLIs.
//!
//! The CLI talks to `tailscaled`, which can wedge; the whole detection shares
//! one [`DETECT_TIMEOUT`] and a child that outlives it is killed, so a stuck
//! Tailscale reads as "off" instead of hanging the daemon.

use std::io::Read;
use std::net::IpAddr;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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

/// Upper bound on one whole detection, across every CLI invocation it tries.
pub const DETECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Detects the local Tailscale node, trying the platform binaries in order.
///
/// Blocks for at most [`DETECT_TIMEOUT`]; async callers should run it on the
/// blocking pool.
pub fn detect() -> Option<TailscaleStatus> {
    let deadline = Instant::now() + DETECT_TIMEOUT;
    status_json(deadline).or_else(|| ip_command(deadline))
}

/// The tailnet IPv4 address, when Tailscale is up.
pub fn tailnet_ip() -> Option<IpAddr> {
    let status = detect()?;
    status
        .usable()
        .then_some(status.ipv4?)
        .filter(|ip| !ip.is_loopback())
}

/// [`tailnet_ip`] without blocking the async runtime.
pub async fn tailnet_ip_async() -> Option<IpAddr> {
    tokio::task::spawn_blocking(tailnet_ip).await.ok().flatten()
}

/// Runs `tailscale status --json` and parses the self node.
fn status_json(deadline: Instant) -> Option<TailscaleStatus> {
    let output = run(&["status", "--json"], deadline)?;
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
fn ip_command(deadline: Instant) -> Option<TailscaleStatus> {
    let output = run(&["ip", "-4"], deadline)?;
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

fn run(args: &[&str], deadline: Instant) -> Option<String> {
    ["tailscale", "tailscale.exe"]
        .into_iter()
        .find_map(|binary| run_until(Path::new(binary), args, deadline))
}

/// Runs `binary` and returns its stdout when it exits successfully before
/// `deadline`. A child still running at the deadline is killed.
fn run_until(binary: &Path, args: &[&str], deadline: Instant) -> Option<String> {
    if Instant::now() >= deadline {
        return None;
    }
    let mut child = Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Drain stdout on its own thread so a large status JSON cannot fill the
    // pipe and stall the child. The thread is not joined on a timeout: a
    // grandchild holding the pipe open must not block the caller either.
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        let _ = tx.send(buffer);
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                tracing::debug!(binary = %binary.display(), "tailscale CLI timed out");
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    let output = rx
        .recv_timeout(remaining.max(Duration::from_millis(100)))
        .ok()?;
    Some(String::from_utf8_lossy(&output).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Budget for a fake CLI that is supposed to answer promptly.
    ///
    /// The tests below exercise "does a prompt CLI still answer", not the
    /// production [`DETECT_TIMEOUT`]. Injecting that budget would make the
    /// assertion about the machine's scheduling speed: a loaded runner can
    /// delay a `sh` that echoes by more than two seconds, and `run_until` would
    /// then correctly report a CLI that outlived its budget as off. The
    /// deadline itself is covered by
    /// [`a_hung_cli_is_killed_at_the_deadline`], which keeps a short one.
    const ANSWER_BUDGET: Duration = Duration::from_secs(60);

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

    #[cfg(unix)]
    fn script(dir: &Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("tailscale");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn a_hung_cli_is_killed_at_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let binary = script(dir.path(), "exec sleep 30");
        let started = Instant::now();
        let output = run_until(
            &binary,
            &["status", "--json"],
            started + Duration::from_millis(300),
        );
        assert_eq!(output, None);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "a hung tailscale CLI must not stall detection: {:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_prompt_cli_still_answers() {
        let dir = tempfile::tempdir().unwrap();
        let binary = script(dir.path(), "echo 100.64.1.5");
        let output = run_until(&binary, &["ip", "-4"], Instant::now() + ANSWER_BUDGET);
        assert_eq!(output.as_deref().map(str::trim), Some("100.64.1.5"));
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_cli_reads_as_off() {
        let dir = tempfile::tempdir().unwrap();
        let binary = script(dir.path(), "exit 1");
        assert_eq!(
            run_until(&binary, &["ip", "-4"], Instant::now() + ANSWER_BUDGET),
            None
        );
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
