//! Harness CLI versions, install detection and update execution.
//!
//! The four harness CLIs are npm-distributed tools with three different update
//! paths, so the daemon needs three facts per installation: the version the CLI
//! reports, the version the registry publishes, and *how* the copy on this host
//! was installed. Detection is deliberately conservative — anything that looks
//! like Homebrew or a system package is reported as a command to run by hand
//! rather than updated behind the user's back (ADR-043).
//!
//! Everything here is pure or injectable: [`RegistryClient`] and
//! [`UpdateRunner`] are seams so tests never touch the network or a real global
//! CLI. The process runner uses no shell and passes arguments as an array.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use h4rn_protocol::{HarnessKind, HarnessOutputStream, HarnessUpdateMethod};
use tokio::io::{AsyncBufReadExt, BufReader};

/// How long a successful registry answer stays fresh.
pub const CACHE_TTL: Duration = Duration::from_secs(3 * 60 * 60);

/// How long an offline or failed registry answer is remembered before retrying.
///
/// Shorter than [`CACHE_TTL`] so a laptop that wakes up does not spend the rest
/// of the afternoon claiming every harness is unknown.
pub const UNKNOWN_TTL: Duration = Duration::from_secs(5 * 60);

/// The public npm registry.
pub const NPM_REGISTRY: &str = "https://registry.npmjs.org";

/// The npm package that ships one harness CLI, when one exists.
///
/// Antigravity has no registry source; callers report its latest version as
/// unknown and do not offer an update.
pub fn npm_package(kind: HarnessKind) -> Option<&'static str> {
    match kind {
        HarnessKind::ClaudeCode => Some("@anthropic-ai/claude-code"),
        HarnessKind::Codex => Some("@openai/codex"),
        HarnessKind::Opencode => Some("opencode-ai"),
        HarnessKind::Antigravity => None,
    }
}

/// The CLI's own self-update subcommand, when it has one.
pub fn self_update_args(kind: HarnessKind) -> Option<&'static [&'static str]> {
    match kind {
        HarnessKind::ClaudeCode => Some(&["update"]),
        HarnessKind::Opencode => Some(&["upgrade"]),
        HarnessKind::Codex | HarnessKind::Antigravity => None,
    }
}

/// Extracts a dotted version from a CLI's real `--version` output.
///
/// Accepted shapes, all seen in the wild:
/// `2.1.323 (Claude Code)`, `codex-cli 0.168.0`, `opencode v2.2.7`, `1.5.8`.
pub fn parse_version(text: &str) -> Option<String> {
    for line in text.lines() {
        for token in line.split(|character: char| {
            character.is_whitespace() || matches!(character, '(' | ')' | '[' | ']' | ',')
        }) {
            if let Some(version) = version_token(token) {
                return Some(version);
            }
        }
    }
    None
}

fn version_token(token: &str) -> Option<String> {
    let token = token.trim_matches(|character: char| {
        !character.is_ascii_alphanumeric() && !matches!(character, '.' | '-' | '+')
    });
    let token = token.strip_prefix('v').unwrap_or(token);
    let core = token.split(['-', '+']).next()?;
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    if !parts
        .iter()
        .all(|part| !part.is_empty() && part.chars().all(|character| character.is_ascii_digit()))
    {
        return None;
    }
    Some(core.to_string())
}

/// Compares two dotted versions, major/minor/patch only.
///
/// A prerelease suffix never wins against the same release, and unparsable
/// values are never newer.
pub fn is_newer(latest: &str, installed: &str) -> bool {
    match (version_parts(latest), version_parts(installed)) {
        (Some(latest), Some(installed)) => latest > installed,
        _ => false,
    }
}

fn version_parts(version: &str) -> Option<(u64, u64, u64)> {
    let core = version.split(['-', '+']).next()?.trim();
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let patch = parts.next().unwrap_or("0").parse().unwrap_or(0);
    Some((major, minor, patch))
}

/// The home directory used for install-location detection.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Where `bun add -g` puts binaries on this host.
fn bun_global_bin() -> Option<PathBuf> {
    if let Some(install) = std::env::var_os("BUN_INSTALL") {
        return Some(PathBuf::from(install).join("bin"));
    }
    home_dir().map(|home| home.join(".bun").join("bin"))
}

/// Decide how one detected CLI would be updated.
///
/// Preference order is the CLI's own updater, then the package manager whose
/// global directory contains the binary, then a command for the person to run.
pub fn detect_update_method(kind: HarnessKind, path: &Path) -> HarnessUpdateMethod {
    detect_update_method_in(kind, path, bun_global_bin().as_deref())
}

fn detect_update_method_in(
    kind: HarnessKind,
    path: &Path,
    bun_bin: Option<&Path>,
) -> HarnessUpdateMethod {
    let program = path.display().to_string();

    if let Some(args) = self_update_args(kind) {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
        return HarnessUpdateMethod::SelfUpdate {
            command: format!("{program} {}", args.join(" ")),
            program,
            args,
        };
    }

    let Some(package) = npm_package(kind) else {
        return HarnessUpdateMethod::Unavailable;
    };

    if is_bun_global(path, bun_bin) {
        return HarnessUpdateMethod::Bun {
            package: package.to_string(),
            command: format!("bun add -g {package}@latest"),
        };
    }

    let npm_command = format!("npm i -g {package}@latest");
    if is_npm_global(path) {
        HarnessUpdateMethod::Npm {
            package: package.to_string(),
            command: npm_command,
        }
    } else {
        // Homebrew, a distro package, or a manual copy: name the command but
        // never run it.
        HarnessUpdateMethod::Manual {
            command: npm_command,
        }
    }
}

fn is_bun_global(path: &Path, bun_bin: Option<&Path>) -> bool {
    let Some(bin) = bun_bin else {
        return false;
    };
    let canonical_bin = bin.canonicalize().unwrap_or_else(|_| bin.to_path_buf());
    path.starts_with(&canonical_bin)
        || path.parent().is_some_and(|parent| {
            parent
                .canonicalize()
                .unwrap_or_else(|_| parent.to_path_buf())
                == canonical_bin
        })
}

fn is_npm_global(path: &Path) -> bool {
    if let Some(home) = home_dir()
        && path.starts_with(home.join(".npm-global").join("bin"))
    {
        return true;
    }
    if cfg!(windows)
        && let Some(appdata) = std::env::var_os("APPDATA")
        && path.starts_with(PathBuf::from(appdata).join("npm"))
    {
        return true;
    }

    // npm's global tree is `<prefix>/lib/node_modules/<pkg>/...`, and every
    // package-manager shim resolves into it. `~/.local/bin/claude` does not.
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    resolved
        .components()
        .any(|component| component.as_os_str() == "node_modules")
}

/// The exact argv h4rn would run for one method, or `None` when it must not run
/// anything (manual and unknown methods).
pub fn update_command(method: &HarnessUpdateMethod) -> Option<(String, Vec<String>)> {
    match method {
        HarnessUpdateMethod::SelfUpdate { program, args, .. } => {
            Some((program.clone(), args.clone()))
        }
        HarnessUpdateMethod::Npm { package, .. } => Some((
            "npm".to_string(),
            vec![
                "i".to_string(),
                "-g".to_string(),
                format!("{package}@latest"),
            ],
        )),
        HarnessUpdateMethod::Bun { package, .. } => Some((
            "bun".to_string(),
            vec![
                "add".to_string(),
                "-g".to_string(),
                format!("{package}@latest"),
            ],
        )),
        HarnessUpdateMethod::Manual { .. } | HarnessUpdateMethod::Unavailable => None,
    }
}

/// One line of update output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateLine {
    /// Which stream the line came from.
    pub stream: HarnessOutputStream,
    /// The line without its terminator.
    pub text: String,
}

/// Reads the newest version of one npm package.
#[async_trait]
pub trait RegistryClient: Send + Sync {
    /// The published version, or `None` when offline or the package is unknown.
    async fn latest(&self, package: &str) -> Option<String>;
}

/// The public npm registry over HTTPS.
pub struct NpmRegistry {
    http: reqwest::Client,
    base: String,
}

impl NpmRegistry {
    /// A client against the public registry.
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            base: NPM_REGISTRY.to_string(),
        }
    }

    /// A client against another registry, for self-hosting and tests.
    pub fn with_base(base: impl Into<String>) -> Self {
        let mut registry = Self::new();
        registry.base = base.into().trim_end_matches('/').to_string();
        registry
    }
}

impl Default for NpmRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RegistryClient for NpmRegistry {
    async fn latest(&self, package: &str) -> Option<String> {
        // Scoped packages keep their `@` and encode the slash, exactly as
        // `npm view` does.
        let encoded = package.replace('/', "%2F");
        let response = self
            .http
            .get(format!("{}/{encoded}/latest", self.base))
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        parse_registry_latest(&response.text().await.ok()?)
    }
}

/// Extracts `version` from an npm `/<package>/latest` document.
pub fn parse_registry_latest(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    parse_version(value.get("version")?.as_str()?)
}

/// Runs one update command and reports each line as it arrives.
#[async_trait]
pub trait UpdateRunner: Send + Sync {
    /// Runs `program` with `args` and calls `on_line` for every output line.
    ///
    /// Returns the process exit code. An unspawnable program is an error, never
    /// a silent success.
    async fn run(
        &self,
        program: &str,
        args: &[String],
        on_line: &(dyn Fn(UpdateLine) + Send + Sync),
    ) -> Result<i32, String>;
}

/// The real process runner: no shell, arguments as an array.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessUpdateRunner;

#[async_trait]
impl UpdateRunner for ProcessUpdateRunner {
    async fn run(
        &self,
        program: &str,
        args: &[String],
        on_line: &(dyn Fn(UpdateLine) + Send + Sync),
    ) -> Result<i32, String> {
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("could not run {program}: {error}"))?;

        let mut stdout = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| "the update command has no stdout".to_string())?,
        )
        .lines();
        let mut stderr = BufReader::new(
            child
                .stderr
                .take()
                .ok_or_else(|| "the update command has no stderr".to_string())?,
        )
        .lines();

        let mut stdout_done = false;
        let mut stderr_done = false;
        while !stdout_done || !stderr_done {
            tokio::select! {
                line = stdout.next_line(), if !stdout_done => match line {
                    Ok(Some(text)) => on_line(UpdateLine { stream: HarnessOutputStream::Stdout, text }),
                    Ok(None) => stdout_done = true,
                    Err(error) => {
                        on_line(UpdateLine {
                            stream: HarnessOutputStream::Stderr,
                            text: format!("could not read stdout: {error}"),
                        });
                        stdout_done = true;
                    }
                },
                line = stderr.next_line(), if !stderr_done => match line {
                    Ok(Some(text)) => on_line(UpdateLine { stream: HarnessOutputStream::Stderr, text }),
                    Ok(None) => stderr_done = true,
                    Err(error) => {
                        on_line(UpdateLine {
                            stream: HarnessOutputStream::Stderr,
                            text: format!("could not read stderr: {error}"),
                        });
                        stderr_done = true;
                    }
                },
            }
        }

        let status = child
            .wait()
            .await
            .map_err(|error| format!("could not wait for {program}: {error}"))?;
        Ok(status.code().unwrap_or(-1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real `--version` output of each CLI on a development machine.
    #[test]
    fn parses_each_real_version_banner() {
        assert_eq!(
            parse_version("2.1.323 (Claude Code)").as_deref(),
            Some("2.1.323")
        );
        assert_eq!(
            parse_version("codex-cli 0.168.0").as_deref(),
            Some("0.168.0")
        );
        assert_eq!(parse_version("opencode v2.2.7").as_deref(), Some("2.2.7"));
        assert_eq!(parse_version("1.5.8").as_deref(), Some("1.5.8"));
    }

    #[test]
    fn ignores_output_without_a_version() {
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("error: unknown option"), None);
        assert_eq!(parse_version("build 2026"), None);
        assert_eq!(parse_version("version 3").as_deref(), None);
    }

    #[test]
    fn compares_dotted_versions() {
        assert!(is_newer("2.2.7", "2.2.6"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("2.2.7", "2.2.7"));
        assert!(!is_newer("unknown", "2.2.7"));
        assert!(!is_newer("2.2.7", "not a version"));
    }

    #[test]
    fn self_updating_clis_win_over_the_package_manager() {
        for (kind, command) in [
            (HarnessKind::ClaudeCode, "update"),
            (HarnessKind::Opencode, "upgrade"),
        ] {
            let method = detect_update_method(kind, Path::new("/usr/local/bin/claude"));
            match method {
                HarnessUpdateMethod::SelfUpdate { args, .. } => {
                    assert_eq!(args, vec![command.to_string()]);
                }
                other => panic!("expected a self-update, got {other:?}"),
            }
        }
    }

    #[test]
    fn bun_global_binaries_use_bun() {
        let home = tempfile::tempdir().expect("tempdir");
        let bun_bin = home.path().join(".bun").join("bin");
        std::fs::create_dir_all(&bun_bin).expect("create bun bin");
        let binary = bun_bin.join("codex");
        std::fs::write(&binary, b"").expect("write binary");
        assert_eq!(
            detect_update_method_in(HarnessKind::Codex, &binary, Some(&bun_bin)),
            HarnessUpdateMethod::Bun {
                package: "@openai/codex".into(),
                command: "bun add -g @openai/codex@latest".into(),
            }
        );
    }

    #[test]
    fn node_modules_installs_use_npm() {
        let dir = tempfile::tempdir().expect("tempdir");
        let binary = dir
            .path()
            .join("lib")
            .join("node_modules")
            .join("@openai")
            .join("codex")
            .join("bin")
            .join("codex");
        std::fs::create_dir_all(binary.parent().expect("parent")).expect("create tree");
        std::fs::write(&binary, b"").expect("write binary");
        assert_eq!(
            detect_update_method(HarnessKind::Codex, &binary),
            HarnessUpdateMethod::Npm {
                package: "@openai/codex".into(),
                command: "npm i -g @openai/codex@latest".into(),
            }
        );
    }

    #[test]
    fn an_unrecognised_install_is_a_command_to_run_by_hand() {
        assert_eq!(
            detect_update_method(HarnessKind::Codex, Path::new("/opt/homebrew/bin/codex")),
            HarnessUpdateMethod::Manual {
                command: "npm i -g @openai/codex@latest".into(),
            }
        );
    }

    #[test]
    fn antigravity_has_no_update_source() {
        assert_eq!(
            detect_update_method(HarnessKind::Antigravity, Path::new("/usr/local/bin/agy")),
            HarnessUpdateMethod::Unavailable
        );
        assert_eq!(npm_package(HarnessKind::Antigravity), None);
    }

    #[test]
    fn update_commands_pass_arguments_as_an_array() {
        assert_eq!(
            update_command(&HarnessUpdateMethod::Npm {
                package: "opencode-ai".into(),
                command: String::new(),
            }),
            Some((
                "npm".to_string(),
                vec![
                    "i".to_string(),
                    "-g".to_string(),
                    "opencode-ai@latest".to_string()
                ]
            ))
        );
        assert_eq!(
            update_command(&HarnessUpdateMethod::Manual {
                command: "npm i -g @openai/codex@latest".into(),
            }),
            None
        );
        assert_eq!(update_command(&HarnessUpdateMethod::Unavailable), None);
    }

    #[test]
    fn parses_a_registry_document() {
        assert_eq!(
            parse_registry_latest(r#"{"name":"opencode-ai","version":"2.2.7"}"#).as_deref(),
            Some("2.2.7")
        );
        assert_eq!(
            parse_registry_latest(r#"{"dist-tags":{"latest":"2.2.7"}}"#),
            None
        );
        assert_eq!(parse_registry_latest("not json"), None);
        assert_eq!(parse_registry_latest(""), None);
    }
}
