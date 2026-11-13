//! `h4rnd doctor`: what this host has, what is missing, where state lives.
//!
//! Doctor is read-only apart from ensuring the data directory exists and can be
//! written. It never starts a harness and never prints the access token.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use h4rn_protocol::{HarnessKind, HostSummary};

use crate::auth::TokenStore;
use crate::config::Config;
use crate::harnesses::{HarnessCatalog, StaticHarnessCatalog, all_harness_kinds};
use crate::hosts::{HostInventory, is_wsl, wsl_distro_name, wsl_distros};

/// Detected harness installation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HarnessStatus {
    /// Harness kind.
    pub kind: HarnessKind,
    /// Executable path, when found.
    pub path: Option<PathBuf>,
    /// Version string, when the binary answered `--version`.
    pub version: Option<String>,
}

impl HarnessStatus {
    /// A stable name for diagnostics.
    pub fn name(&self) -> &'static str {
        match self.kind {
            HarnessKind::ClaudeCode => "claude-code",
            HarnessKind::Codex => "codex",
            HarnessKind::Opencode => "opencode",
            HarnessKind::Antigravity => "antigravity",
        }
    }
}

/// Everything doctor observed.
#[derive(Clone, Debug)]
pub struct DoctorReport {
    /// Daemon version.
    pub version: String,
    /// Resolved data directory.
    pub data_dir: PathBuf,
    /// Whether the data directory could be created and written.
    pub data_dir_writable: bool,
    /// Whether an access token file exists.
    pub token_present: bool,
    /// Token path, without its contents.
    pub token_path: PathBuf,
    /// Primary listen address.
    pub bind: String,
    /// Extra listen address, if configured.
    pub extra_bind: Option<String>,
    /// Hosts this daemon knows about.
    pub hosts: Vec<HostSummary>,
    /// Harness detection results.
    pub harnesses: Vec<HarnessStatus>,
    /// Whether this process runs inside WSL.
    pub inside_wsl: bool,
    /// WSL distribution name when inside WSL.
    pub wsl_distro: Option<String>,
    /// WSL distributions visible from here.
    pub wsl_distros: Vec<String>,
}

impl fmt::Display for DoctorReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "h4rn daemon {}", self.version)?;
        writeln!(
            formatter,
            "  data dir      {} ({})",
            self.data_dir.display(),
            if self.data_dir_writable {
                "writable"
            } else {
                "NOT WRITABLE"
            }
        )?;
        writeln!(
            formatter,
            "  access token  {} ({})",
            self.token_path.display(),
            if self.token_present {
                "present"
            } else {
                "missing; run `h4rnd token`"
            }
        )?;
        writeln!(formatter, "  bind          {}", self.bind)?;
        writeln!(
            formatter,
            "  extra bind    {}",
            self.extra_bind.as_deref().unwrap_or("-")
        )?;
        writeln!(
            formatter,
            "  wsl           {}",
            match (&self.wsl_distro, self.inside_wsl) {
                (Some(distro), true) => format!("inside {distro}"),
                _ => "not running inside WSL".to_owned(),
            }
        )?;
        writeln!(
            formatter,
            "  wsl distros   {}",
            if self.wsl_distros.is_empty() {
                "-".to_owned()
            } else {
                self.wsl_distros.join(", ")
            }
        )?;

        writeln!(formatter, "  hosts")?;
        for host in &self.hosts {
            writeln!(
                formatter,
                "    {:<24} {:<16} {}",
                host.host_id.as_str(),
                host.platform,
                if host.online { "online" } else { "unreachable" }
            )?;
        }

        writeln!(formatter, "  harnesses")?;
        for harness in &self.harnesses {
            match (&harness.path, &harness.version) {
                (Some(path), Some(version)) => writeln!(
                    formatter,
                    "    {:<16} {} ({version})",
                    harness.name(),
                    path.display()
                )?,
                (Some(path), None) => writeln!(
                    formatter,
                    "    {:<16} {} (version unknown)",
                    harness.name(),
                    path.display()
                )?,
                (None, _) => writeln!(formatter, "    {:<16} not found", harness.name())?,
            }
        }
        Ok(())
    }
}

/// Runs doctor and prints the report.
pub async fn run(config: &Config) -> anyhow::Result<()> {
    let report = report(config).await;
    print!("{report}");
    Ok(())
}

/// Collects the doctor report without printing it.
pub async fn report(config: &Config) -> DoctorReport {
    let data_dir_writable =
        config.ensure_data_dir().is_ok() && write_probe(&config.data_dir).unwrap_or(false);
    let tokens = TokenStore::new(&config.data_dir);
    let catalog: std::sync::Arc<dyn HarnessCatalog> =
        std::sync::Arc::new(StaticHarnessCatalog::new());
    let hosts = HostInventory::detect(catalog).summaries();

    let mut harnesses = Vec::new();
    for kind in all_harness_kinds() {
        harnesses.push(detect_harness(kind).await);
    }

    DoctorReport {
        version: crate::version().to_owned(),
        data_dir: config.data_dir.clone(),
        data_dir_writable,
        token_present: tokens.exists(),
        token_path: tokens.path().to_path_buf(),
        bind: config.bind.to_string(),
        extra_bind: config.extra_bind.map(|addr| addr.to_string()),
        hosts,
        harnesses,
        inside_wsl: is_wsl(),
        wsl_distro: wsl_distro_name(),
        wsl_distros: wsl_distros(),
    }
}

/// Detects one harness executable and asks for its version.
pub async fn detect_harness(kind: HarnessKind) -> HarnessStatus {
    let binary = match kind {
        HarnessKind::ClaudeCode => "claude",
        HarnessKind::Codex => "codex",
        HarnessKind::Opencode => "opencode",
        HarnessKind::Antigravity => "agy",
    };

    let Some(path) = find_in_path(binary) else {
        return HarnessStatus {
            kind,
            path: None,
            version: None,
        };
    };

    let version = probe_version(&path).await;
    HarnessStatus {
        kind,
        path: Some(path),
        version,
    }
}

async fn probe_version(path: &Path) -> Option<String> {
    let output = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::process::Command::new(path)
            .arg("--version")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;

    let text = if output.stdout.is_empty() {
        String::from_utf8_lossy(&output.stderr).into_owned()
    } else {
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    Some(line.chars().take(120).collect())
}

/// Finds an executable on `PATH`, honouring `PATHEXT` on Windows.
pub fn find_in_path(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".to_owned())
            .split(';')
            .map(|extension| extension.to_ascii_lowercase())
            .collect()
    } else {
        Vec::new()
    };

    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(binary);
        if candidate.is_file() && is_executable(&candidate) {
            return Some(candidate);
        }
        for extension in &extensions {
            let with_extension = directory.join(format!("{binary}{extension}"));
            if with_extension.is_file() {
                return Some(with_extension);
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

fn write_probe(directory: &Path) -> std::io::Result<bool> {
    let probe = directory.join(".doctor-write-probe");
    match std::fs::write(&probe, b"ok") {
        Ok(()) => {
            let _ = std::fs::remove_file(probe);
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_names_are_stable() {
        let status = HarnessStatus {
            kind: HarnessKind::ClaudeCode,
            path: None,
            version: None,
        };
        assert_eq!(status.name(), "claude-code");
    }

    #[test]
    fn report_renders_without_secrets() {
        let report = DoctorReport {
            version: "0.1.0".into(),
            data_dir: PathBuf::from("/tmp/h4rn"),
            data_dir_writable: true,
            token_present: true,
            token_path: PathBuf::from("/tmp/h4rn/access_token"),
            bind: "127.0.0.1:47800".into(),
            extra_bind: None,
            hosts: vec![HostSummary {
                host_id: h4rn_protocol::HostId::new("host_test"),
                name: "test".into(),
                platform: "linux".into(),
                online: true,
                last_seen: chrono::Utc::now(),
                harnesses: Vec::new(),
                latency_ms: None,
                path: None,
            }],
            harnesses: vec![HarnessStatus {
                kind: HarnessKind::Codex,
                path: Some(PathBuf::from("/usr/bin/codex")),
                version: Some("1.2.3".into()),
            }],
            inside_wsl: false,
            wsl_distro: None,
            wsl_distros: Vec::new(),
        };
        let rendered = report.to_string();
        assert!(rendered.contains("host_test"));
        assert!(rendered.contains("/usr/bin/codex"));
        assert!(!rendered.contains("secret"));
    }

    #[test]
    fn find_in_path_finds_a_known_binary() {
        // `sh` exists on every Unix CI image; Windows has no assertion here.
        if cfg!(unix) {
            assert!(find_in_path("sh").is_some());
            assert!(find_in_path("h4rn-no-such-binary").is_none());
        }
    }
}
