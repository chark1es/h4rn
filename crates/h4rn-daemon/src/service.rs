//! `h4rnd service`: run the daemon as a per-user background service.
//!
//! Linux uses the systemd user unit that ships in `packaging/daemon/systemd`,
//! macOS the launch agent in `packaging/daemon/launchd`, and Windows a
//! per-user Task Scheduler task (a scheduled task, not a service: the daemon
//! does not speak the Service Control Manager protocol). Every external command
//! runs from an argument array — never a shell string — so a path with a space
//! or a quote cannot turn into a second command.
//!
//! The unit/plist/task is rendered from the file that ships with the release,
//! with this executable's absolute path and the current data directory
//! substituted. Installation is idempotent: an identical unit is kept, and the
//! platform's enable/start step is still applied so a stopped service is
//! repaired. A launch agent that is already running with an unchanged plist is
//! left alone, so reinstalling does not bounce a healthy daemon.
//!
//! The Windows task is named per account (`h4rn-<user>`), because Task
//! Scheduler names are machine-wide and two users must not share one task.

use std::ffi::OsStr;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};

use crate::config::Config;

/// The systemd user unit name.
pub const UNIT_NAME: &str = "h4rnd.service";
/// The launchd job label.
pub const LAUNCHD_LABEL: &str = "dev.h4rn.h4rnd";
/// The Task Scheduler name older releases used for every account.
pub const LEGACY_WINDOWS_TASK: &str = "h4rn";

const SYSTEMD_TEMPLATE: &str = include_str!("../../../packaging/daemon/systemd/h4rnd.service");
const LAUNCHD_TEMPLATE: &str =
    include_str!("../../../packaging/daemon/launchd/dev.h4rn.h4rnd.plist");
const WINDOWS_INSTALL_SCRIPT: &str =
    include_str!("../../../packaging/daemon/windows/install-scheduled-task.ps1");
const WINDOWS_UNINSTALL_SCRIPT: &str =
    include_str!("../../../packaging/daemon/windows/uninstall-scheduled-task.ps1");

/// One external command, as an executable and its arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandSpec {
    /// Program to execute.
    pub program: String,
    /// Arguments, passed one by one without a shell.
    pub args: Vec<String>,
}

impl CommandSpec {
    /// Builds a command from a program and arguments.
    pub fn new<I, S>(program: impl Into<String>, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }

    /// A readable form for error messages.
    pub fn display(&self) -> String {
        std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(String::as_str))
            .map(quote_for_display)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// The platform whose service manager this host uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServicePlatform {
    /// Linux systemd user units.
    Systemd,
    /// macOS launch agents.
    Launchd,
    /// Windows Task Scheduler.
    Windows,
}

impl ServicePlatform {
    /// The platform this binary was built for.
    pub const fn current() -> Self {
        #[cfg(target_os = "linux")]
        {
            Self::Systemd
        }
        #[cfg(target_os = "macos")]
        {
            Self::Launchd
        }
        #[cfg(target_os = "windows")]
        {
            Self::Windows
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            Self::Systemd
        }
    }

    /// A short name for output.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Systemd => "systemd",
            Self::Launchd => "launchd",
            Self::Windows => "Task Scheduler",
        }
    }
}

/// What the platform's service manager reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ServiceStatus {
    /// Whether the unit/plist/task exists.
    pub installed: bool,
    /// Whether it is running now.
    pub active: bool,
    /// Whether it starts at login/boot; `None` when the platform does not say.
    pub enabled: Option<bool>,
}

/// What the installed service actually runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstalledService {
    /// No unit/plist/task is installed.
    NotInstalled,
    /// The service launches this executable.
    Runs(PathBuf),
    /// A service is installed, but its executable could not be read.
    Unknown,
}

/// The registered service and the health endpoint its own daemon uses.
///
/// `h4rnd update` probes this endpoint, never the updater's own `--bind`: the
/// daemon that has to restart is the one the unit, plist or task starts, and its
/// bind address lives in that service's environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegisteredTarget {
    /// No unit/plist/task is installed.
    NotInstalled,
    /// The service and the endpoint resolved from its own configuration.
    Runs {
        /// Executable the service launches.
        binary: PathBuf,
        /// Health endpoint the service's daemon listens on.
        health: SocketAddr,
    },
    /// A service exists, but its executable or endpoint could not be verified.
    Unverified {
        /// Why the target could not be verified.
        reason: String,
    },
}

/// The decision about restarting after a binary was replaced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestartDecision {
    /// Restart the service: it runs the binary that was replaced.
    Restart,
    /// Nothing is installed, so the user restarts the daemon by hand.
    NotInstalled,
    /// The service runs another installation; do not restart it.
    OtherInstall(PathBuf),
    /// The installed service's executable could not be read.
    Unknown,
}

/// What `h4rnd update` should tell the user after replacing the binary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestartOutcome {
    /// The service was restarted.
    Restarted,
    /// No service is installed.
    NotInstalled,
    /// The service runs a different binary and was left alone.
    OtherInstall(PathBuf),
    /// The service is installed but its executable could not be read.
    UnknownInstall,
    /// The restart was attempted and failed; the binary is still installed.
    Failed(String),
}

/// The `service` subcommands.
#[derive(Clone, Copy, Debug, clap::Subcommand)]
pub enum ServiceCommand {
    /// Install (or repair) the service and start it.
    Install {
        /// Install even from a build or temporary directory.
        #[arg(long)]
        force: bool,
    },
    /// Stop and remove the service; the data directory is kept.
    Uninstall,
    /// Report whether the service is installed, running and enabled.
    Status,
    /// Restart the service.
    Restart,
    /// Show recent service logs.
    Logs,
}

/// Runs one `h4rnd service` subcommand.
pub fn run(config: &Config, command: ServiceCommand) -> Result<()> {
    match command {
        ServiceCommand::Install { force } => install(config, force),
        ServiceCommand::Uninstall => uninstall(),
        ServiceCommand::Status => status(),
        ServiceCommand::Restart => restart(config),
        ServiceCommand::Logs => logs(),
    }
}

/// Installs and starts the service for the current user.
pub fn install(config: &Config, force: bool) -> Result<()> {
    let binary = current_binary_path()?;
    if !force && is_build_or_temp_path(&binary, &std::env::temp_dir()) {
        bail!(
            "refusing to install a service for {}: it is a build or temporary path and would \
             disappear; install h4rnd outside target/ (or pass --force if you are sure)",
            binary.display()
        );
    }
    let data_dir = absolute_path(&config.data_dir)?;
    config.ensure_data_dir()?;

    match ServicePlatform::current() {
        ServicePlatform::Systemd => install_systemd(&binary, &data_dir)?,
        ServicePlatform::Launchd => install_launchd(&binary, &data_dir)?,
        ServicePlatform::Windows => install_windows(&binary, &data_dir)?,
    }
    Ok(())
}

/// Stops and removes the service without touching the data directory.
pub fn uninstall() -> Result<()> {
    match ServicePlatform::current() {
        ServicePlatform::Systemd => uninstall_systemd(),
        ServicePlatform::Launchd => uninstall_launchd(),
        ServicePlatform::Windows => uninstall_windows(),
    }
}

/// Prints installed/active/enabled and, on Linux, whether lingering is on.
pub fn status() -> Result<()> {
    let (platform, path, status) = match ServicePlatform::current() {
        ServicePlatform::Systemd => {
            let path = systemd_unit_path()?;
            let mut status = systemd_status()?;
            status.installed |= path.exists();
            (ServicePlatform::Systemd, path, status)
        }
        ServicePlatform::Launchd => {
            let path = launchd_plist_path()?;
            let mut status = launchd_status()?;
            status.installed |= path.exists();
            (ServicePlatform::Launchd, path, status)
        }
        ServicePlatform::Windows => (
            ServicePlatform::Windows,
            PathBuf::from(windows_installed_task().unwrap_or_else(windows_task_name)),
            windows_status()?,
        ),
    };

    print!("{}", format_status(platform, &path, status));
    if platform == ServicePlatform::Systemd {
        print_linger_advice();
    }
    Ok(())
}

/// Restarts the installed service.
pub fn restart(_config: &Config) -> Result<()> {
    if !is_installed()? {
        bail!("the h4rnd service is not installed; run `h4rnd service install` first");
    }
    restart_current_service()
}

fn restart_current_service() -> Result<()> {
    match ServicePlatform::current() {
        ServicePlatform::Systemd => {
            ensure_available("systemctl")?;
            run_checked(&systemd_restart_command())?;
            println!("Restarted {UNIT_NAME}.");
        }
        ServicePlatform::Launchd => {
            ensure_available("launchctl")?;
            run_checked(&launchd_restart_command(current_uid()?))?;
            println!("Restarted {LAUNCHD_LABEL}.");
        }
        ServicePlatform::Windows => {
            ensure_available("schtasks")?;
            let task = windows_installed_task().unwrap_or_else(windows_task_name);
            run_windows_restart(&task, |command| run_checked(command).map(|_| ()))?;
            println!("Restarted scheduled task {task}.");
        }
    }
    Ok(())
}

/// Shows recent logs, or the command that shows them when the tool is absent.
pub fn logs() -> Result<()> {
    match ServicePlatform::current() {
        ServicePlatform::Systemd => {
            if command_available("journalctl") {
                run_inherit(&systemd_logs_command())
            } else {
                println!("journalctl is not installed; the service logs go to the journal.");
                println!(
                    "Run this on a host with journald: {}",
                    systemd_logs_command().display()
                );
                Ok(())
            }
        }
        ServicePlatform::Launchd => {
            let home = home_dir()?;
            let logs = home.join("Library").join("Logs").join("h4rn");
            println!("Standard output: {}", logs.join("h4rnd.log").display());
            println!("Standard error:  {}", logs.join("h4rnd.err.log").display());
            println!();
            println!("  tail -n 200 {}", logs.join("h4rnd.err.log").display());
            Ok(())
        }
        ServicePlatform::Windows => {
            let task = windows_installed_task().unwrap_or_else(windows_task_name);
            println!("Task Scheduler runs {task} without a console, so h4rnd writes no log file.");
            println!("Run `h4rnd serve` in a terminal to watch logs, or open Task Scheduler →");
            println!("{task} → History for start and exit events.");
            Ok(())
        }
    }
}

/// Whether the service is installed.
pub fn is_installed() -> Result<bool> {
    Ok(match ServicePlatform::current() {
        ServicePlatform::Systemd => systemd_unit_path()?.exists(),
        ServicePlatform::Launchd => launchd_plist_path()?.exists(),
        ServicePlatform::Windows => !matches!(installed_service(), InstalledService::NotInstalled),
    })
}

/// Whether the service manager says the service is running now.
///
/// Used by `h4rnd update`: an active service whose daemon does not answer the
/// health check must block the update rather than look stopped. Errors count as
/// "not active"; the update's TCP probe still fails closed.
pub fn is_active() -> bool {
    match ServicePlatform::current() {
        ServicePlatform::Systemd => systemd_status()
            .map(|status| status.active)
            .unwrap_or(false),
        ServicePlatform::Launchd => launchd_status()
            .map(|status| status.active)
            .unwrap_or(false),
        ServicePlatform::Windows => windows_status()
            .map(|status| status.active)
            .unwrap_or(false),
    }
}

/// The executable the installed service launches, read from the effective
/// unit/plist/task configuration.
pub fn installed_service() -> InstalledService {
    match registered_target() {
        RegisteredTarget::NotInstalled => InstalledService::NotInstalled,
        RegisteredTarget::Runs { binary, .. } => InstalledService::Runs(binary),
        RegisteredTarget::Unverified { .. } => InstalledService::Unknown,
    }
}

/// One external command's result, reduced to what the resolvers need.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CommandOutput {
    /// Whether the command exited successfully.
    pub success: bool,
    /// Standard output.
    pub stdout: Vec<u8>,
}

impl CommandOutput {
    fn from_process(output: Output) -> Self {
        Self {
            success: output.status.success(),
            stdout: output.stdout,
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

/// Runs one external command, so a test can answer like systemctl/PowerShell
/// without running the real service manager.
pub(crate) type CommandRunner<'a> = &'a dyn Fn(&CommandSpec) -> Result<CommandOutput>;

/// Resolves the registered service and the health endpoint its daemon uses.
///
/// The endpoint follows the daemon's own precedence: the `--bind` argument in
/// the registered command line first, then `H4RN_BIND` from the service's
/// environment (systemd `Environment=`/`EnvironmentFile=` and the user manager
/// environment, launchd `EnvironmentVariables`, or the account-wide Windows
/// environment), then the compiled default. h4rnd has no config file, so there
/// is no config-file step. It is deliberately independent of the current
/// process's `--bind`/`H4RN_BIND`, which may describe a different daemon.
pub fn registered_target() -> RegisteredTarget {
    registered_target_with(&|spec| run_capture_allow_failure(spec).map(CommandOutput::from_process))
}

fn registered_target_with(run: CommandRunner<'_>) -> RegisteredTarget {
    match ServicePlatform::current() {
        ServicePlatform::Systemd => systemd_registered_target(run),
        ServicePlatform::Launchd => launchd_registered_target(),
        ServicePlatform::Windows => windows_registered_target(run),
    }
}

/// Decides whether `h4rnd update` should restart the service.
pub fn restart_decision(service: &InstalledService, current: &Path) -> RestartDecision {
    match service {
        InstalledService::NotInstalled => RestartDecision::NotInstalled,
        InstalledService::Unknown => RestartDecision::Unknown,
        InstalledService::Runs(installed) if !same_binary(installed, current) => {
            RestartDecision::OtherInstall(installed.clone())
        }
        InstalledService::Runs(_) => RestartDecision::Restart,
    }
}

/// Restarts the service after replacing `current`, only when it runs that path.
pub fn restart_after_update(current: &Path) -> RestartOutcome {
    match restart_decision(&installed_service(), current) {
        RestartDecision::NotInstalled => RestartOutcome::NotInstalled,
        RestartDecision::Unknown => RestartOutcome::UnknownInstall,
        RestartDecision::OtherInstall(installed) => RestartOutcome::OtherInstall(installed),
        RestartDecision::Restart => match restart_current_service() {
            Ok(()) => RestartOutcome::Restarted,
            // The binary is already installed; a failed bounce is a warning.
            Err(error) => RestartOutcome::Failed(error.to_string()),
        },
    }
}

/// Whether two paths name the same file, tolerating symlinks and `..`.
pub fn same_binary(left: &Path, right: &Path) -> bool {
    let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    canonical(left) == canonical(right)
}

// --- Linux -----------------------------------------------------------------

/// The text of a systemd unit and the drop-ins that override it.
///
/// Drop-ins change the effective `ExecStart=` and `Environment=`, so both the
/// restart decision and the busy probe read the combined file set instead of
/// the base unit alone.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SystemdUnitText {
    /// The unit file itself.
    pub fragment: String,
    /// Drop-in files, in the order systemd applies them.
    pub dropins: Vec<String>,
}

impl SystemdUnitText {
    /// Reads a unit and every drop-in that systemd would apply to it.
    pub fn read(fragment: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(fragment)
            .with_context(|| format!("read {}", fragment.display()))?;
        let mut dropins = Vec::new();
        for path in systemd_dropin_files(fragment) {
            if let Ok(dropin) = std::fs::read_to_string(&path) {
                dropins.push(dropin);
            }
        }
        Ok(Self {
            fragment: text,
            dropins,
        })
    }

    /// The last `ExecStart=` in the unit and its drop-ins, expanded.
    ///
    /// `%h` and `%%` are resolved because systemd expands them before running
    /// the command; an empty assignment resets the list, as systemd documents.
    pub fn exec_start(&self, home: &Path) -> Option<String> {
        let mut current = None;
        for text in std::iter::once(&self.fragment).chain(self.dropins.iter()) {
            for line in text.lines() {
                let Some(value) = line.trim().strip_prefix("ExecStart=") else {
                    continue;
                };
                let value = value.trim();
                current = if value.is_empty() {
                    None
                } else {
                    Some(expand_systemd_specifiers(value, home))
                };
            }
        }
        current
    }

    /// The effective environment after drop-in overrides.
    pub fn environment(&self, home: &Path) -> Vec<(String, String)> {
        let mut variables: Vec<(String, String)> = Vec::new();
        for text in std::iter::once(&self.fragment).chain(self.dropins.iter()) {
            for line in text.lines() {
                let Some(value) = line.trim().strip_prefix("Environment=") else {
                    continue;
                };
                let assignments = parse_systemd_assignments(value);
                if assignments.is_empty() {
                    // `Environment=` with no assignment clears the list.
                    variables.clear();
                    continue;
                }
                for (key, value) in assignments {
                    let key = expand_systemd_specifiers(&key, home);
                    let value = expand_systemd_specifiers(&value, home);
                    match variables.iter_mut().find(|(existing, _)| *existing == key) {
                        Some(existing) => existing.1 = value,
                        None => variables.push((key, value)),
                    }
                }
            }
        }
        variables
    }

    /// The `EnvironmentFile=` files, in order, with whether a missing file is
    /// tolerated (`-/path`).
    ///
    /// A relative path is resolved against the unit's directory, which is where
    /// a user unit keeps its files.
    pub fn environment_files(&self, home: &Path) -> Vec<(PathBuf, bool)> {
        let mut files = Vec::new();
        for text in std::iter::once(&self.fragment).chain(self.dropins.iter()) {
            for line in text.lines() {
                let Some(value) = line.trim().strip_prefix("EnvironmentFile=") else {
                    continue;
                };
                for word in parse_systemd_words(value) {
                    let (path, optional) = match word.strip_prefix('-') {
                        Some(path) => (path, true),
                        None => (word.as_str(), false),
                    };
                    let path = expand_systemd_specifiers(path, home);
                    if !path.is_empty() {
                        files.push((PathBuf::from(path), optional));
                    }
                }
            }
        }
        files
    }

    /// The unit's environment after `EnvironmentFile=` files are applied.
    ///
    /// systemd applies the files in order and they override `Environment=`, so
    /// a later file wins over both an earlier file and an inline assignment.
    pub fn effective_environment(
        &self,
        home: &Path,
        unit_dir: &Path,
    ) -> std::result::Result<Vec<(String, String)>, String> {
        let mut variables = self.environment(home);
        for (path, optional) in self.environment_files(home) {
            let path = if path.is_absolute() {
                path
            } else {
                unit_dir.join(path)
            };
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) if optional && error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(format!(
                        "EnvironmentFile {} could not be read: {error}",
                        path.display()
                    ));
                }
            };
            for (key, value) in parse_environment_file(&text) {
                let key = expand_systemd_specifiers(&key, home);
                let value = expand_systemd_specifiers(&value, home);
                match variables.iter_mut().find(|(existing, _)| *existing == key) {
                    Some(existing) => existing.1 = value,
                    None => variables.push((key, value)),
                }
            }
        }
        Ok(variables)
    }

    /// The `H4RN_BIND` the unit sets, if any.
    pub fn bind(
        &self,
        home: &Path,
        unit_dir: &Path,
    ) -> std::result::Result<Option<String>, String> {
        Ok(self
            .effective_environment(home, unit_dir)?
            .into_iter()
            .find(|(key, _)| key == "H4RN_BIND")
            .map(|(_, value)| value))
    }

    /// The health endpoint the service's daemon uses.
    ///
    /// A unit that names no bind uses the daemon's compiled default, so this is
    /// still the service's own address and not the updater's.
    pub fn health_endpoint(
        &self,
        home: &Path,
        unit_dir: &Path,
    ) -> std::result::Result<SocketAddr, String> {
        let bind = self.bind(home, unit_dir)?;
        health_endpoint_for(bind.as_deref(), "the service unit")
    }
}

/// `KEY=VALUE` lines of a systemd `EnvironmentFile=`.
///
/// Blank lines and comments are skipped, and one layer of single or double
/// quotes is removed, as systemd does for a plain assignment.
fn parse_environment_file(text: &str) -> Vec<(String, String)> {
    let mut variables = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.is_empty() {
            continue;
        }
        let value = value.trim();
        let value = match value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
            Some(unquoted) => unquoted.to_owned(),
            None => match value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
                Some(unquoted) => unquoted.to_owned(),
                None => value.to_owned(),
            },
        };
        variables.push((key.trim().to_owned(), value));
    }
    variables
}

/// The health endpoint for a bind a service names, or the compiled default.
fn health_endpoint_for(
    bind: Option<&str>,
    source: &str,
) -> std::result::Result<SocketAddr, String> {
    match bind {
        None => Ok(default_health_endpoint()),
        Some(value) => crate::config::parse_addr(value).map_err(|error| {
            format!("H4RN_BIND={value:?} in {source} is not a listen address: {error}")
        }),
    }
}

/// The value after `--bind` or `--bind=` in an argument list.
///
/// The last occurrence wins, as a later flag overrides an earlier one.
pub fn bind_argument<I, S>(arguments: I) -> Option<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut arguments = arguments.into_iter();
    let mut bind = None;
    while let Some(argument) = arguments.next() {
        let argument = argument.as_ref();
        if argument == "--bind" {
            if let Some(value) = arguments.next() {
                bind = Some(value.as_ref().to_owned());
            }
        } else if let Some(value) = argument.strip_prefix("--bind=") {
            bind = Some(value.to_owned());
        }
    }
    bind
}

/// The daemon's compiled default health endpoint.
fn default_health_endpoint() -> SocketAddr {
    crate::config::DEFAULT_BIND
        .parse()
        .expect("the default bind address is valid")
}

/// Expands the systemd specifiers a path or environment value may contain.
fn expand_systemd_specifiers(value: &str, home: &Path) -> String {
    let home = home.to_string_lossy();
    let mut expanded = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            expanded.push(ch);
            continue;
        }
        match chars.peek() {
            Some('%') => {
                chars.next();
                expanded.push('%');
            }
            Some('h') => {
                chars.next();
                expanded.push_str(&home);
            }
            _ => expanded.push('%'),
        }
    }
    expanded
}

/// Splits `Environment=` words the way systemd does, honouring quotes and
/// backslash escapes.
fn parse_systemd_assignments(value: &str) -> Vec<(String, String)> {
    parse_systemd_words(value)
        .into_iter()
        .filter_map(|word| {
            let (key, value) = word.split_once('=')?;
            (!key.is_empty()).then(|| (key.to_owned(), value.to_owned()))
        })
        .collect()
}

/// Splits a unit directive's value into words, honouring quotes and escapes.
fn parse_systemd_words(value: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut chars = value.trim().chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                while let Some(inner) = chars.next() {
                    match inner {
                        '"' => break,
                        '\\' => {
                            if let Some(escaped) = chars.next() {
                                word.push(escaped);
                            }
                        }
                        other => word.push(other),
                    }
                }
            }
            '\\' => {
                if let Some(escaped) = chars.next() {
                    word.push(escaped);
                }
            }
            ch if ch.is_whitespace() => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            other => word.push(other),
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// Every drop-in file systemd applies to a unit, in application order.
///
/// Later file names override earlier ones; when the same file name exists in
/// two directories, the higher-priority directory wins, as in systemd.
pub fn systemd_dropin_files(fragment: &Path) -> Vec<PathBuf> {
    let name = fragment
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| UNIT_NAME.to_owned());
    let mut directories = Vec::new();
    if let Some(parent) = fragment.parent() {
        directories.push(parent.join(format!("{name}.d")));
    }
    for base in [
        "/etc/systemd/user",
        "/usr/local/lib/systemd/user",
        "/usr/lib/systemd/user",
    ] {
        directories.push(Path::new(base).join(format!("{name}.d")));
    }
    let mut found: Vec<(String, PathBuf)> = Vec::new();
    for directory in directories {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name().to_string_lossy().into_owned();
            if !file_name.ends_with(".conf") {
                continue;
            }
            if found.iter().any(|(existing, _)| *existing == file_name) {
                continue;
            }
            found.push((file_name, entry.path()));
        }
    }
    found.sort_by(|left, right| left.0.cmp(&right.0));
    found.into_iter().map(|(_, path)| path).collect()
}

/// The unit file systemd actually loaded, which may not be at the default path.
fn systemd_fragment_path_with(run: CommandRunner<'_>) -> Option<PathBuf> {
    if let Ok(output) = run(&systemd_fragment_path_command())
        && output.success
    {
        let path = output.text().trim().to_owned();
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    systemd_unit_path().ok()
}

fn systemd_registered_target(run: CommandRunner<'_>) -> RegisteredTarget {
    let Some(fragment) = systemd_fragment_path_with(run) else {
        return RegisteredTarget::Unverified {
            reason: "the systemd user unit path could not be resolved".to_owned(),
        };
    };
    let home = match home_dir() {
        Ok(home) => home,
        Err(error) => {
            return RegisteredTarget::Unverified {
                reason: error.to_string(),
            };
        }
    };
    systemd_target_from(&fragment, &home, run)
}

/// Resolves a systemd unit at an explicit path.
///
/// The loaded unit is the source of truth: `systemctl show` reports the
/// effective `ExecStart` including runtime drop-ins. The files are read only for
/// the environment and when systemctl cannot answer (it is unavailable, or the
/// unit is on disk but not loaded yet).
pub(crate) fn systemd_target_from(
    fragment: &Path,
    home: &Path,
    run: CommandRunner<'_>,
) -> RegisteredTarget {
    if !fragment.exists() {
        return RegisteredTarget::NotInstalled;
    }
    let text = match SystemdUnitText::read(fragment) {
        Ok(text) => text,
        Err(error) => {
            return RegisteredTarget::Unverified {
                reason: error.to_string(),
            };
        }
    };
    let manager_argv = match run(&systemd_exec_start_command()) {
        Ok(output) if output.success => systemd_show_exec_argv(&output.text()),
        _ => None,
    };
    let argv = match manager_argv {
        Some(argv) if !argv.is_empty() => argv,
        _ => match text.exec_start(home) {
            Some(value) => systemd_exec_argv(&value),
            None => Vec::new(),
        },
    };
    let Some(program) = argv.first() else {
        return RegisteredTarget::Unverified {
            reason: format!("{} has no readable ExecStart", fragment.display()),
        };
    };
    let unit_dir = fragment.parent().unwrap_or(home);
    let mut bind = bind_argument(argv.iter().skip(1));
    if bind.is_none() {
        match text.bind(home, unit_dir) {
            Ok(from_unit) => bind = from_unit,
            Err(reason) => return RegisteredTarget::Unverified { reason },
        }
    }
    if bind.is_none() {
        // The unit inherits H4RN_BIND from the user manager unless it names its
        // own, so an unreadable manager environment leaves the endpoint unknown.
        match run(&systemd_show_environment_command()) {
            Ok(output) if output.success => {
                bind = parse_show_environment(&output.text(), "H4RN_BIND");
            }
            _ => {
                return RegisteredTarget::Unverified {
                    reason: "the systemd user manager environment (`systemctl --user \
                             show-environment`) could not be read"
                        .to_owned(),
                };
            }
        }
    }
    match health_endpoint_for(bind.as_deref(), "the service unit") {
        Ok(health) => RegisteredTarget::Runs {
            binary: PathBuf::from(program),
            health,
        },
        Err(reason) => RegisteredTarget::Unverified { reason },
    }
}

/// The command that names the unit file systemd loaded.
pub fn systemd_fragment_path_command() -> CommandSpec {
    CommandSpec::new(
        "systemctl",
        [
            "--user",
            "show",
            UNIT_NAME,
            "--property=FragmentPath",
            "--value",
            "--no-pager",
        ],
    )
}

/// The command that prints the unit's effective `ExecStart=`.
///
/// `systemctl show` applies runtime drop-ins, so this is what systemd would run
/// even when the files on disk have been changed since the last daemon-reload.
pub fn systemd_exec_start_command() -> CommandSpec {
    CommandSpec::new(
        "systemctl",
        [
            "--user",
            "show",
            UNIT_NAME,
            "--property=ExecStart",
            "--value",
            "--no-pager",
        ],
    )
}

/// The command that prints the user manager's environment.
pub fn systemd_show_environment_command() -> CommandSpec {
    CommandSpec::new("systemctl", ["--user", "show-environment"])
}

/// The systemd user unit path.
pub fn systemd_unit_path() -> Result<PathBuf> {
    let home = home_dir()?;
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    Ok(systemd_unit_path_in(&home, config_home.as_deref()))
}

/// The systemd user unit path under a given home and XDG config directory.
pub fn systemd_unit_path_in(home: &Path, xdg_config_home: Option<&Path>) -> PathBuf {
    let config = xdg_config_home
        .filter(|path| path.is_absolute())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".config"));
    config.join("systemd").join("user").join(UNIT_NAME)
}

fn systemd_status() -> Result<ServiceStatus> {
    if !command_available("systemctl") {
        return Ok(ServiceStatus::default());
    }
    let output = run_capture_allow_failure(&systemd_status_command())?;
    Ok(parse_systemd_show(&String::from_utf8_lossy(&output.stdout)))
}

fn install_systemd(binary: &Path, data_dir: &Path) -> Result<()> {
    ensure_available("systemctl")?;
    let unit_path = systemd_unit_path()?;
    let rendered = render_systemd_unit(binary, data_dir);
    let write = should_write_file(&unit_path, rendered.as_bytes())
        .with_context(|| format!("inspect {}", unit_path.display()))?;
    if write {
        if let Some(parent) = unit_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        std::fs::write(&unit_path, rendered.as_bytes())
            .with_context(|| format!("write {}", unit_path.display()))?;
        println!("Wrote {}.", unit_path.display());
    } else {
        println!("{} is already up to date.", unit_path.display());
    }
    for command in systemd_install_commands() {
        run_checked(&command)?;
    }
    println!("Installed and started {UNIT_NAME}.");
    println!("Data directory: {}", data_dir.display());
    print_linger_advice();
    Ok(())
}

fn uninstall_systemd() -> Result<()> {
    let unit_path = systemd_unit_path()?;
    if !unit_path.exists() {
        println!("{UNIT_NAME} is not installed.");
        return Ok(());
    }
    ensure_available("systemctl")?;
    for command in systemd_uninstall_commands() {
        run_checked(&command)?;
    }
    std::fs::remove_file(&unit_path).with_context(|| format!("remove {}", unit_path.display()))?;
    run_checked(&CommandSpec::new("systemctl", ["--user", "daemon-reload"]))?;
    println!("Stopped and removed {UNIT_NAME}. The data directory was left untouched.");
    Ok(())
}

/// Renders the shipped systemd unit with this executable and data directory.
pub fn render_systemd_unit(binary: &Path, data_dir: &Path) -> String {
    let mut rendered = String::with_capacity(SYSTEMD_TEMPLATE.len() + 128);
    for line in SYSTEMD_TEMPLATE.lines() {
        if line.starts_with("ExecStart=") {
            rendered.push_str("ExecStart=");
            // systemd does not expand variables in the executable token, so a
            // literal `$` stays single there. Only the arguments that follow
            // undergo `$` expansion and need `$$`.
            rendered.push_str(&systemd_quote(&binary.to_string_lossy(), false));
            rendered.push(' ');
            rendered.push_str(&systemd_escape_exec("serve"));
        } else if line.starts_with("Environment=H4RN_DATA_DIR=") {
            rendered.push_str("Environment=\"H4RN_DATA_DIR=");
            rendered.push_str(&systemd_escape_env(&data_dir.to_string_lossy()));
            rendered.push('"');
        } else {
            rendered.push_str(line);
        }
        rendered.push('\n');
    }
    rendered
}

/// Quotes one `ExecStart=` argument.
fn systemd_quote(value: &str, escape_dollar: bool) -> String {
    format!("\"{}\"", systemd_escape(value, escape_dollar))
}

/// Escapes an `Environment=` value.
///
/// Systemd expands `%` specifiers in environment values, so a literal percent
/// is doubled; quotes, backslashes and newlines are the other characters that
/// matter. `$` is *not* expanded in `Environment=`, so it stays single.
fn systemd_escape_env(value: &str) -> String {
    systemd_escape(value, false)
}

/// Escapes an `ExecStart=` argument.
///
/// `ExecStart=` expands both `%` specifiers and `$` variables, so both are
/// doubled.
fn systemd_escape_exec(value: &str) -> String {
    systemd_escape(value, true)
}

fn systemd_escape(value: &str, escape_dollar: bool) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' | '\\' => {
                escaped.push('\\');
                escaped.push(ch);
            }
            '%' => {
                escaped.push('%');
                escaped.push('%');
            }
            '$' if escape_dollar => {
                escaped.push('$');
                escaped.push('$');
            }
            // C escapes keep a path containing a newline from starting a new
            // directive.
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

/// The program an `ExecStart=` line launches, with systemd quoting undone.
pub fn systemd_unit_program(unit: &str) -> Option<String> {
    unit.lines().find_map(|line| {
        let value = line.trim_start().strip_prefix("ExecStart=")?;
        systemd_exec_program(value)
    })
}

/// The argv of one `ExecStart=` value, with systemd quoting undone.
///
/// The value may carry the `-@!+:` prefixes systemd allows before the program.
pub fn systemd_exec_argv(value: &str) -> Vec<String> {
    let value = value
        .trim()
        .trim_start_matches(['-', '+', '!', '@', ':'])
        .trim_start();
    if value.is_empty() {
        return Vec::new();
    }
    parse_systemd_words(value)
        .into_iter()
        .map(|word| systemd_unescape(&word))
        .collect()
}

/// The program from one `ExecStart=` value (which may carry `-@!+:` prefixes).
pub fn systemd_exec_program(value: &str) -> Option<String> {
    systemd_exec_argv(value).into_iter().next()
}

/// The argv of the last `ExecStart=` in `systemctl show -p ExecStart --value`.
///
/// The property prints one `{ path=… ; argv[]=… ; … }` block per command, and
/// the last block is the one whose argv ends the list.
pub fn systemd_show_exec_argv(output: &str) -> Option<Vec<String>> {
    let argv = output
        .lines()
        .flat_map(|line| line.split("argv[]=").skip(1))
        .last()?;
    let argv = argv.split(" ; ").next().unwrap_or(argv).trim();
    (!argv.is_empty()).then(|| systemd_exec_argv(argv))
}

/// One `KEY=VALUE` line of `systemctl --user show-environment`.
pub fn parse_show_environment(output: &str, key: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let (name, value) = line.trim().split_once('=')?;
        (name == key).then(|| value.to_owned())
    })
}

/// Undoes the `%` and `$` doubling that keeps a literal in a unit.
fn systemd_unescape(value: &str) -> String {
    value.replace("%%", "%").replace("$$", "$")
}

/// Every command an install runs, in order.
pub fn systemd_install_commands() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("systemctl", ["--user", "daemon-reload"]),
        CommandSpec::new("systemctl", ["--user", "enable", "--now", UNIT_NAME]),
    ]
}

/// Every command an uninstall runs before the unit file is removed.
pub fn systemd_uninstall_commands() -> Vec<CommandSpec> {
    vec![CommandSpec::new(
        "systemctl",
        ["--user", "disable", "--now", UNIT_NAME],
    )]
}

/// The systemd restart command.
pub fn systemd_restart_command() -> CommandSpec {
    CommandSpec::new("systemctl", ["--user", "restart", UNIT_NAME])
}

/// The systemd status query.
pub fn systemd_status_command() -> CommandSpec {
    CommandSpec::new(
        "systemctl",
        [
            "--user",
            "show",
            UNIT_NAME,
            "--property=LoadState,ActiveState,UnitFileState",
            "--no-pager",
        ],
    )
}

/// The systemd log query.
pub fn systemd_logs_command() -> CommandSpec {
    CommandSpec::new(
        "journalctl",
        ["--user", "-u", "h4rnd", "-n", "200", "--no-pager"],
    )
}

/// The lingering query for a user or uid.
pub fn systemd_linger_command(user: &str) -> CommandSpec {
    CommandSpec::new(
        "loginctl",
        ["show-user", user, "--property=Linger", "--no-pager"],
    )
}

/// Parses `systemctl show` output.
pub fn parse_systemd_show(output: &str) -> ServiceStatus {
    let mut status = ServiceStatus::default();
    for line in output.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "LoadState" => status.installed = value.trim() != "not-found",
            "ActiveState" => status.active = value.trim() == "active",
            "UnitFileState" => {
                let value = value.trim();
                status.enabled = Some(value == "enabled" || value == "enabled-runtime");
            }
            _ => {}
        }
    }
    status
}

/// Parses `loginctl show-user --property=Linger` output.
pub fn parse_linger(output: &str) -> bool {
    output
        .lines()
        .find_map(|line| line.trim().strip_prefix("Linger="))
        .map(|value| value.trim() == "yes")
        .unwrap_or(false)
}

fn print_linger_advice() {
    if !command_available("loginctl") {
        return;
    }
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| current_uid().map(|uid| uid.to_string()).unwrap_or_default());
    if user.is_empty() {
        return;
    }
    let Ok(output) = run_capture_allow_failure(&systemd_linger_command(&user)) else {
        return;
    };
    if output.status.success() && !parse_linger(&String::from_utf8_lossy(&output.stdout)) {
        println!();
        println!("Lingering is off: the service stops when you log out.");
        println!("  To keep it running, run: loginctl enable-linger {user}");
        println!("(This command was not run for you; some systems need an administrator.)");
    }
}

// --- macOS -----------------------------------------------------------------

/// The launch agent path.
pub fn launchd_plist_path() -> Result<PathBuf> {
    Ok(launchd_plist_path_in(&home_dir()?))
}

/// The launch agent path under a given home.
pub fn launchd_plist_path_in(home: &Path) -> PathBuf {
    home.join("Library")
        .join("LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist"))
}

fn launchd_status() -> Result<ServiceStatus> {
    if !command_available("launchctl") {
        return Ok(ServiceStatus::default());
    }
    let uid = current_uid()?;
    let output = run_capture_allow_failure(&launchd_status_command(uid))?;
    if !output.status.success() {
        return Ok(ServiceStatus::default());
    }
    // "enabled" is only known when `launchctl print-disabled` was read.
    let disabled_output = run_capture_allow_failure(&launchd_print_disabled_command(uid))
        .ok()
        .filter(|disabled| disabled.status.success())
        .map(|disabled| String::from_utf8_lossy(&disabled.stdout).into_owned());
    Ok(launchd_service_status(
        &String::from_utf8_lossy(&output.stdout),
        disabled_output.as_deref(),
    ))
}

/// Combines `launchctl print` and `launchctl print-disabled` output.
pub fn launchd_service_status(print: &str, disabled: Option<&str>) -> ServiceStatus {
    ServiceStatus {
        installed: true,
        active: parse_launchd_print(print).active,
        enabled: disabled.map(|output| !parse_launchd_disabled(output, LAUNCHD_LABEL)),
    }
}

/// Whether `launchctl print-disabled` lists a job as disabled.
///
/// A job that is not listed is enabled; `None` means the output could not be
/// read at all, so the status reports "unknown".
pub fn parse_launchd_disabled(output: &str, label: &str) -> bool {
    let marker = format!("\"{label}\" =>");
    output.lines().any(|line| {
        line.trim()
            .strip_prefix(&marker)
            .map(str::trim)
            .is_some_and(|value| matches!(value, "true" | "disabled"))
    })
}

fn launchd_registered_target() -> RegisteredTarget {
    let Ok(path) = launchd_plist_path() else {
        return RegisteredTarget::Unverified {
            reason: "the launch agent path could not be resolved".to_owned(),
        };
    };
    launchd_target_from(&path)
}

/// Resolves a launch agent at an explicit path.
pub(crate) fn launchd_target_from(path: &Path) -> RegisteredTarget {
    if !path.exists() {
        return RegisteredTarget::NotInstalled;
    }
    let Ok(plist) = std::fs::read_to_string(path) else {
        return RegisteredTarget::Unverified {
            reason: format!("{} could not be read", path.display()),
        };
    };
    let Some(arguments) = launchd_program_arguments(&plist) else {
        return RegisteredTarget::Unverified {
            reason: format!("{} has no ProgramArguments", path.display()),
        };
    };
    let Some(program) = arguments.first() else {
        return RegisteredTarget::Unverified {
            reason: format!("{} has no ProgramArguments", path.display()),
        };
    };
    // A `--bind` in ProgramArguments wins over EnvironmentVariables, which wins
    // over the daemon's compiled default.
    let bind = bind_argument(arguments.iter().skip(1)).or_else(|| {
        launchd_environment(&plist)
            .into_iter()
            .find(|(key, _)| key == "H4RN_BIND")
            .map(|(_, value)| value)
    });
    match health_endpoint_for(bind.as_deref(), "the launch agent") {
        Ok(health) => RegisteredTarget::Runs {
            binary: PathBuf::from(program),
            health,
        },
        Err(reason) => RegisteredTarget::Unverified { reason },
    }
}

/// The health endpoint named in a launch agent's `EnvironmentVariables`.
pub fn launchd_health_endpoint(plist: &str) -> std::result::Result<SocketAddr, String> {
    let bind = launchd_environment(plist)
        .into_iter()
        .find(|(key, _)| key == "H4RN_BIND")
        .map(|(_, value)| value);
    health_endpoint_for(bind.as_deref(), "the launch agent")
}

/// The `EnvironmentVariables` dictionary of a launch agent.
pub fn launchd_environment(plist: &str) -> Vec<(String, String)> {
    let Some(key) = plist.find("<key>EnvironmentVariables</key>") else {
        return Vec::new();
    };
    let rest = &plist[key..];
    let Some(dict) = rest.find("<dict>") else {
        return Vec::new();
    };
    let rest = &rest[dict + "<dict>".len()..];
    let Some(end) = rest.find("</dict>") else {
        return Vec::new();
    };
    let body = &rest[..end];
    let mut variables = Vec::new();
    let mut rest = body;
    while let Some(key_start) = rest.find("<key>") {
        let after = &rest[key_start + "<key>".len()..];
        let Some(key_end) = after.find("</key>") else {
            break;
        };
        let name = xml_unescape(after[..key_end].trim());
        let after = &after[key_end + "</key>".len()..];
        let Some(value_start) = after.find("<string>") else {
            break;
        };
        let after = &after[value_start + "<string>".len()..];
        let Some(value_end) = after.find("</string>") else {
            break;
        };
        let value = xml_unescape(after[..value_end].trim());
        variables.push((name, value));
        rest = &after[value_end + "</string>".len()..];
    }
    variables
}

fn install_launchd(binary: &Path, data_dir: &Path) -> Result<()> {
    ensure_available("launchctl")?;
    let home = home_dir()?;
    let plist_path = launchd_plist_path_in(&home);
    let log_dir = home.join("Library").join("Logs").join("h4rn");
    std::fs::create_dir_all(&log_dir).with_context(|| format!("create {}", log_dir.display()))?;
    let rendered = render_launchd_plist(binary, data_dir, &home);
    let write = should_write_file(&plist_path, rendered.as_bytes())
        .with_context(|| format!("inspect {}", plist_path.display()))?;
    if write {
        if let Some(parent) = plist_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        std::fs::write(&plist_path, rendered.as_bytes())
            .with_context(|| format!("write {}", plist_path.display()))?;
        println!("Wrote {}.", plist_path.display());
    } else {
        println!("{} is already up to date.", plist_path.display());
    }
    let running = launchd_status()
        .map(|status| status.active)
        .unwrap_or(false);
    if !should_reload_launchd(write, running) {
        // Reinstalling the same plist must not bounce a healthy daemon.
        println!("{LAUNCHD_LABEL} is running with an unchanged plist; left it alone.");
        println!("Data directory: {}", data_dir.display());
        println!("Logs: {}", log_dir.display());
        return Ok(());
    }
    for command in launchd_install_commands(current_uid()?, &plist_path) {
        // `bootout` fails when the agent is not loaded, which is exactly the
        // first install; the bootstrap below is the step that must succeed.
        if command.args.first().is_some_and(|arg| arg == "bootout") {
            let _ = run_capture_allow_failure(&command)?;
        } else {
            run_checked(&command)?;
        }
    }
    println!("Installed and started {LAUNCHD_LABEL}.");
    println!("Data directory: {}", data_dir.display());
    println!("Logs: {}", log_dir.display());
    Ok(())
}

/// Whether an install must boot the agent out and back in.
pub fn should_reload_launchd(plist_changed: bool, running: bool) -> bool {
    plist_changed || !running
}

fn uninstall_launchd() -> Result<()> {
    let plist_path = launchd_plist_path()?;
    if !plist_path.exists() {
        println!("{LAUNCHD_LABEL} is not installed.");
        return Ok(());
    }
    ensure_available("launchctl")?;
    for command in launchd_uninstall_commands(current_uid()?) {
        // A plist that is present but not bootstrapped still uninstalls.
        let _ = run_capture_allow_failure(&command)?;
    }
    std::fs::remove_file(&plist_path)
        .with_context(|| format!("remove {}", plist_path.display()))?;
    println!("Stopped and removed {LAUNCHD_LABEL}. The data directory was left untouched.");
    Ok(())
}

/// Renders the shipped launch agent with this executable, data directory and home.
pub fn render_launchd_plist(binary: &Path, data_dir: &Path, home: &Path) -> String {
    let binary = xml_escape(&binary.to_string_lossy());
    let data_dir = xml_escape(&data_dir.to_string_lossy());
    let home = xml_escape(&home.to_string_lossy());
    replace_placeholders(
        LAUNCHD_TEMPLATE,
        &[
            ("__HOME__/.local/share/h4rn", &data_dir),
            ("/usr/local/bin/h4rnd", &binary),
            ("__HOME__", &home),
        ],
    )
}

/// Replaces placeholders in one pass, so a substituted value that happens to
/// contain another placeholder is never re-expanded.
fn replace_placeholders(template: &str, replacements: &[(&str, &str)]) -> String {
    let mut rendered = String::with_capacity(template.len());
    let mut rest = template;
    while !rest.is_empty() {
        if let Some((placeholder, value)) = replacements
            .iter()
            .find(|(placeholder, _)| rest.starts_with(placeholder))
        {
            rendered.push_str(value);
            rest = &rest[placeholder.len()..];
        } else {
            let ch = rest.chars().next().expect("rest is not empty");
            rendered.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
    }
    rendered
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn xml_unescape(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The program a launch agent starts, from its `ProgramArguments` array.
pub fn launchd_program(plist: &str) -> Option<String> {
    launchd_program_arguments(plist)?.into_iter().next()
}

/// The `ProgramArguments` array of a launch agent, in order.
pub fn launchd_program_arguments(plist: &str) -> Option<Vec<String>> {
    let key = plist.find("<key>ProgramArguments</key>")?;
    let rest = &plist[key..];
    let array = rest.find("<array>")?;
    let rest = &rest[array + "<array>".len()..];
    let end = rest.find("</array>")?;
    let body = &rest[..end];
    let mut arguments = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("<string>") {
        let after = &rest[start + "<string>".len()..];
        let end = after.find("</string>")?;
        arguments.push(xml_unescape(after[..end].trim()));
        rest = &after[end + "</string>".len()..];
    }
    (!arguments.is_empty()).then_some(arguments)
}

/// Every command a launchd install runs, in order.
pub fn launchd_install_commands(uid: u32, plist: &Path) -> Vec<CommandSpec> {
    vec![
        CommandSpec::new(
            "launchctl",
            ["bootout", &format!("gui/{uid}/{LAUNCHD_LABEL}")],
        ),
        CommandSpec::new(
            "launchctl",
            ["bootstrap", &format!("gui/{uid}"), &plist.to_string_lossy()],
        ),
    ]
}

/// Every command a launchd uninstall runs.
pub fn launchd_uninstall_commands(uid: u32) -> Vec<CommandSpec> {
    vec![CommandSpec::new(
        "launchctl",
        ["bootout", &format!("gui/{uid}/{LAUNCHD_LABEL}")],
    )]
}

/// The launchd restart command.
pub fn launchd_restart_command(uid: u32) -> CommandSpec {
    CommandSpec::new(
        "launchctl",
        ["kickstart", "-k", &format!("gui/{uid}/{LAUNCHD_LABEL}")],
    )
}

/// The launchd status query.
pub fn launchd_status_command(uid: u32) -> CommandSpec {
    CommandSpec::new(
        "launchctl",
        ["print", &format!("gui/{uid}/{LAUNCHD_LABEL}")],
    )
}

/// The launchd disabled-jobs query, which is the only source for `enabled`.
pub fn launchd_print_disabled_command(uid: u32) -> CommandSpec {
    CommandSpec::new("launchctl", ["print-disabled", &format!("gui/{uid}")])
}

/// What `launchctl print` says about the job.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LaunchdState {
    /// Whether the job is running.
    pub active: bool,
}

/// Parses `launchctl print` output.
pub fn parse_launchd_print(output: &str) -> LaunchdState {
    let active = output.lines().any(|line| {
        line.trim()
            .strip_prefix("state =")
            .is_some_and(|value| value.trim() == "running")
    });
    LaunchdState { active }
}

// --- Windows ---------------------------------------------------------------

/// The scheduled-task name for this account, for example `h4rn-alice`.
pub fn windows_task_name() -> String {
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .ok();
    windows_task_name_for(user.as_deref())
}

/// The scheduled-task name for a given account name.
///
/// Task Scheduler names are machine-wide, so the account is part of the name.
/// Anything outside `A-Z a-z 0-9 . _ -` becomes `-`, which keeps the name safe
/// to pass as one argument and inside a PowerShell single-quoted string.
pub fn windows_task_name_for(user: Option<&str>) -> String {
    let user = user.unwrap_or_default();
    let user = user.rsplit(['\\', '/']).next().unwrap_or_default();
    let sanitized: String = user
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let sanitized = sanitized.trim_matches('-');
    if sanitized.is_empty() {
        "h4rn".to_owned()
    } else {
        format!("h4rn-{sanitized}")
    }
}

/// A PowerShell single-quoted string literal.
fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn windows_status() -> Result<ServiceStatus> {
    let current = windows_status_for(&windows_task_name())?;
    if current.installed {
        return Ok(current);
    }
    // A task registered by an older release is still h4rnd: report it so
    // uninstall removes it and install migrates it.
    if windows_task_name() != LEGACY_WINDOWS_TASK {
        let legacy = windows_status_for(LEGACY_WINDOWS_TASK)?;
        if legacy.installed {
            return Ok(legacy);
        }
    }
    Ok(current)
}

fn windows_status_for(task: &str) -> Result<ServiceStatus> {
    if !command_available("powershell") {
        return Ok(ServiceStatus::default());
    }
    let output = run_capture_allow_failure(&windows_status_command(task))?;
    Ok(parse_windows_task_state(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

/// The registered task to restart, preferring this account's task and falling
/// back to one an older release registered under the shared `h4rn` name.
fn windows_installed_task() -> Option<String> {
    let current = windows_task_name();
    if windows_status_for(&current)
        .map(|status| status.installed)
        .unwrap_or(false)
    {
        return Some(current);
    }
    if current != LEGACY_WINDOWS_TASK
        && windows_status_for(LEGACY_WINDOWS_TASK)
            .map(|status| status.installed)
            .unwrap_or(false)
    {
        return Some(LEGACY_WINDOWS_TASK.to_owned());
    }
    None
}

fn windows_registered_target(run: CommandRunner<'_>) -> RegisteredTarget {
    if !command_available("powershell") {
        return RegisteredTarget::NotInstalled;
    }
    let Some(task) = windows_installed_task() else {
        return RegisteredTarget::NotInstalled;
    };
    let Ok(output) = run(&windows_task_executable_command(&task)) else {
        return RegisteredTarget::Unverified {
            reason: format!("the action of scheduled task {task} could not be read"),
        };
    };
    if !output.success {
        return RegisteredTarget::NotInstalled;
    }
    let stdout = output.text();
    let Some(action) = windows_task_action(&stdout) else {
        return match windows_installed_binary(&stdout) {
            InstalledService::NotInstalled => RegisteredTarget::NotInstalled,
            _ => RegisteredTarget::Unverified {
                reason: format!("the action of scheduled task {task} could not be parsed"),
            },
        };
    };
    windows_target_from_action(&action, || match run(&windows_bind_env_command()) {
        Ok(output) if output.success => Ok(parse_windows_bind_env(&output.text())),
        _ => Err(format!(
            "the account-wide H4RN_BIND used by scheduled task {task} could not be read"
        )),
    })
}

/// Resolves one task action and the account environment it inherits.
///
/// A `--bind` in the registered action wins over the account-wide
/// `H4RN_BIND`, which wins over the daemon's compiled default. The environment
/// is only read when the action names no bind.
fn windows_target_from_action<F>(action: &TaskAction, account_bind: F) -> RegisteredTarget
where
    F: FnOnce() -> std::result::Result<Option<String>, String>,
{
    let binary =
        inline_task_binary(&action.arguments).unwrap_or_else(|| PathBuf::from(&action.execute));
    let mut bind = bind_argument(action.arguments.split_whitespace());
    if bind.is_none() {
        // Task Scheduler starts the daemon from the account environment. If
        // that cannot be read, the endpoint cannot be verified, so the update
        // must ask for `--force`.
        match account_bind() {
            Ok(from_environment) => bind = from_environment,
            Err(reason) => return RegisteredTarget::Unverified { reason },
        }
    }
    match health_endpoint_for(bind.as_deref(), "the scheduled task") {
        Ok(health) => RegisteredTarget::Runs { binary, health },
        Err(reason) => RegisteredTarget::Unverified { reason },
    }
}

/// Prepares an installation for a Windows replace or rollback.
///
/// Both move the running `h4rnd.exe` aside, so the registered task must already
/// be able to recover a missing install path. The stable launcher
/// (which waits for the update lock and repairs the install path itself) is
/// written before any rename, so a crash mid-update leaves a task that can still
/// start, even when the binary at the install path is an older image that does
/// not understand `--recover-only`.
pub fn prepare_replace(current: &Path, data_dir: &Path) -> Result<()> {
    if ServicePlatform::current() != ServicePlatform::Windows {
        return Ok(());
    }
    if windows_installed_task().is_none() {
        return Ok(());
    }
    ensure_available("powershell")?;
    let task = windows_task_name();
    let script = PrivateScript::create("install-scheduled-task.ps1", WINDOWS_INSTALL_SCRIPT)?;
    run_checked(&windows_prepare_command(
        script.path(),
        current,
        data_dir,
        &task,
    ))?;
    drop(script);
    println!("Updated scheduled task {task} with the recovery launcher.");
    Ok(())
}

fn install_windows(binary: &Path, data_dir: &Path) -> Result<()> {
    ensure_available("powershell")?;
    let task = windows_task_name();
    let script = PrivateScript::create("install-scheduled-task.ps1", WINDOWS_INSTALL_SCRIPT)?;
    run_checked(&windows_install_command(
        script.path(),
        binary,
        data_dir,
        &task,
    ))?;
    drop(script);
    println!("Installed and started scheduled task {task}.");
    println!("Data directory: {}", data_dir.display());
    Ok(())
}

fn uninstall_windows() -> Result<()> {
    let task = windows_task_name();
    let current = windows_status_for(&task)?;
    let legacy = if task == LEGACY_WINDOWS_TASK {
        false
    } else {
        windows_status_for(LEGACY_WINDOWS_TASK)?.installed
    };
    if !current.installed && !legacy {
        println!("Scheduled task {task} is not installed.");
        return Ok(());
    }
    ensure_available("powershell")?;
    let script = PrivateScript::create("uninstall-scheduled-task.ps1", WINDOWS_UNINSTALL_SCRIPT)?;
    run_checked(&windows_uninstall_command(script.path(), &task))?;
    drop(script);
    println!(
        "Stopped and removed the h4rnd scheduled task(s). The data directory was left untouched."
    );
    Ok(())
}

/// A PowerShell script in a private, exclusively created directory.
///
/// The directory name carries the process id and a random suffix and is created
/// with `create_dir` (never `create_dir_all`), so an existing directory is never
/// reused. On Windows `%TEMP%` is per-user and the directory can only be opened
/// by its owner; on Unix it is mode 0700. The script is removed when the guard
/// is dropped, after the child process exits.
struct PrivateScript {
    directory: PathBuf,
    file: PathBuf,
}

impl PrivateScript {
    fn create(name: &str, contents: &str) -> Result<Self> {
        let base = std::env::temp_dir();
        for _ in 0..16 {
            let directory = base.join(format!(
                "h4rn-service-{}-{:08x}",
                std::process::id(),
                rand::random::<u32>()
            ));
            match std::fs::create_dir(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).with_context(|| format!("create {}", directory.display()));
                }
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ =
                    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700));
            }
            let file = directory.join(name);
            let opened = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&file);
            let mut file_handle = match opened {
                Ok(file_handle) => file_handle,
                Err(error) => {
                    let _ = std::fs::remove_dir_all(&directory);
                    return Err(error).with_context(|| format!("create {}", file.display()));
                }
            };
            if let Err(error) = file_handle.write_all(contents.as_bytes()) {
                let _ = std::fs::remove_dir_all(&directory);
                return Err(error).with_context(|| format!("write {}", file.display()));
            }
            drop(file_handle);
            return Ok(Self { directory, file });
        }
        bail!("could not create a private directory for the service script")
    }

    fn path(&self) -> &Path {
        &self.file
    }
}

impl Drop for PrivateScript {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.file);
        let _ = std::fs::remove_dir(&self.directory);
    }
}

/// The command that runs the shipped install script with explicit paths.
pub fn windows_install_command(
    script: &Path,
    binary: &Path,
    data_dir: &Path,
    task: &str,
) -> CommandSpec {
    windows_task_script_command(script, binary, data_dir, task, &[])
}

/// The command that rewrites the task's launcher before a Windows update.
///
/// It passes `-NoStart` because the daemon must keep running until the binary
/// has been replaced; the update restarts the task afterwards.
pub fn windows_prepare_command(
    script: &Path,
    binary: &Path,
    data_dir: &Path,
    task: &str,
) -> CommandSpec {
    windows_task_script_command(script, binary, data_dir, task, &["-NoStart"])
}

fn windows_task_script_command(
    script: &Path,
    binary: &Path,
    data_dir: &Path,
    task: &str,
    extra: &[&str],
) -> CommandSpec {
    let mut args = vec![
        "-NoProfile".to_owned(),
        "-NonInteractive".to_owned(),
        "-ExecutionPolicy".to_owned(),
        "Bypass".to_owned(),
        "-File".to_owned(),
        script.to_string_lossy().into_owned(),
        "-Binary".to_owned(),
        binary.to_string_lossy().into_owned(),
        "-DataDir".to_owned(),
        data_dir.to_string_lossy().into_owned(),
        "-TaskName".to_owned(),
        task.to_owned(),
        "-LegacyTaskName".to_owned(),
        LEGACY_WINDOWS_TASK.to_owned(),
    ];
    args.extend(extra.iter().map(|arg| (*arg).to_owned()));
    CommandSpec::new("powershell", args)
}

/// The command that runs the shipped uninstall script.
pub fn windows_uninstall_command(script: &Path, task: &str) -> CommandSpec {
    CommandSpec::new(
        "powershell",
        [
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-ExecutionPolicy".to_owned(),
            "Bypass".to_owned(),
            "-File".to_owned(),
            script.to_string_lossy().into_owned(),
            "-TaskName".to_owned(),
            task.to_owned(),
            "-LegacyTaskName".to_owned(),
            LEGACY_WINDOWS_TASK.to_owned(),
        ],
    )
}

/// The Task Scheduler restart steps, with whether each one must succeed.
///
/// `/End` fails harmlessly when the task is not running, so only `/Run` is
/// checked. `h4rnd service restart` and the update's restart use this list.
pub fn windows_restart_steps(task: &str) -> Vec<(CommandSpec, bool)> {
    vec![
        (CommandSpec::new("schtasks", ["/End", "/TN", task]), false),
        (CommandSpec::new("schtasks", ["/Run", "/TN", task]), true),
    ]
}

/// Runs the restart steps: a failed stop (`/End` on a task that is not running)
/// is ignored, while a failed start is returned.
fn run_windows_restart<F>(task: &str, mut run: F) -> Result<()>
where
    F: FnMut(&CommandSpec) -> Result<()>,
{
    for (command, required) in windows_restart_steps(task) {
        if required {
            run(&command)?;
        } else {
            let _ = run(&command);
        }
    }
    Ok(())
}

/// The Task Scheduler status query.
///
/// `Get-ScheduledTask` reports a locale-independent `State` enum, so a German
/// or Japanese Windows does not break the parser the way English `schtasks`
/// status text would.
pub fn windows_status_command(task: &str) -> CommandSpec {
    CommandSpec::new(
        "powershell",
        [
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-Command".to_owned(),
            format!(
                "$ErrorActionPreference='SilentlyContinue'; \
                 $task = Get-ScheduledTask -TaskName {}; \
                 if ($null -eq $task) {{ 'State=missing' }} else {{ 'State=' + $task.State }}",
                powershell_quote(task)
            ),
        ],
    )
}

/// The command that prints the executable and arguments each task action runs,
/// separated by a tab.
pub fn windows_task_executable_command(task: &str) -> CommandSpec {
    CommandSpec::new(
        "powershell",
        [
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-Command".to_owned(),
            format!(
                "$ErrorActionPreference='SilentlyContinue'; \
                 $task = Get-ScheduledTask -TaskName {}; \
                 if ($null -ne $task) {{ \
                   $task.Actions | ForEach-Object {{ \
                     $_.Execute + \"`t\" + $(if ($null -ne $_.Arguments) {{ $_.Arguments }} else {{ '' }}).Replace(\"`r\", ' ').Replace(\"`n\", ' ').Replace(\"`t\", ' ') \
                   }} \
                 }}",
                powershell_quote(task)
            ),
        ],
    )
}

/// The install binary named by the inline recovery command in a task action.
///
/// The command starts with `$b = '<install path>'; ...`; single quotes inside
/// the path are doubled by PowerShell.
pub fn inline_task_binary(arguments: &str) -> Option<PathBuf> {
    let marker = "$b = '";
    let start = arguments.find(marker)? + marker.len();
    let mut value = String::new();
    let mut chars = arguments[start..].chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\'' {
            if chars.peek() == Some(&'\'') {
                chars.next();
                value.push('\'');
            } else {
                return Some(PathBuf::from(value));
            }
        } else {
            value.push(ch);
        }
    }
    None
}

/// One action of a scheduled task: its executable and its arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskAction {
    /// The action's `Execute` value.
    pub execute: String,
    /// The action's `Arguments` value (`""` when it has none).
    pub arguments: String,
}

/// The first action of a task that names a binary, from `Execute<TAB>Arguments`
/// lines printed by [`windows_task_executable_command`].
pub fn windows_task_action(output: &str) -> Option<TaskAction> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .find_map(|line| {
            let (execute, arguments) = match line.split_once('\t') {
                Some((execute, arguments)) => (execute.trim(), arguments.trim()),
                None => (line, ""),
            };
            let named = !execute.is_empty() || inline_task_binary(arguments).is_some();
            named.then(|| TaskAction {
                execute: execute.to_owned(),
                arguments: arguments.to_owned(),
            })
        })
}

/// The binary a task's actions run, from `Execute<TAB>Arguments` lines.
///
/// `h4rnd service install` registers an inline recovery step (the task recovers
/// from an interrupted update before starting the daemon); a task registered by
/// an older install runs the binary directly.
pub fn windows_installed_binary(output: &str) -> InstalledService {
    let mut saw_action = false;
    for line in output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        saw_action = true;
        let (execute, arguments) = match line.split_once('\t') {
            Some((execute, arguments)) => (execute.trim(), arguments.trim()),
            None => (line, ""),
        };
        if let Some(binary) = inline_task_binary(arguments) {
            return InstalledService::Runs(binary);
        }
        if !execute.is_empty() {
            return InstalledService::Runs(PathBuf::from(execute));
        }
    }
    if saw_action {
        InstalledService::Unknown
    } else {
        InstalledService::NotInstalled
    }
}

/// The command that prints the account-wide `H4RN_BIND` the task inherits.
///
/// The user value wins over the machine value, as it does for the process
/// environment. `Unset` means neither scope names one.
pub fn windows_bind_env_command() -> CommandSpec {
    CommandSpec::new(
        "powershell",
        [
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-ExecutionPolicy".to_owned(),
            "Bypass".to_owned(),
            "-Command".to_owned(),
            "$u=[Environment]::GetEnvironmentVariable('H4RN_BIND','User'); \
             $m=[Environment]::GetEnvironmentVariable('H4RN_BIND','Machine'); \
             if ($u) { 'User=' + $u } elseif ($m) { 'Machine=' + $m } else { 'Unset' }"
                .to_owned(),
        ],
    )
}

/// Parses the output of [`windows_bind_env_command`].
pub fn parse_windows_bind_env(output: &str) -> Option<String> {
    let value = output.trim();
    if value == "Unset" {
        return None;
    }
    value
        .strip_prefix("User=")
        .or_else(|| value.strip_prefix("Machine="))
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Parses the `State=...` line printed by [`windows_status_command`].
pub fn parse_windows_task_state(output: &str) -> ServiceStatus {
    let state = output
        .lines()
        .find_map(|line| line.trim().strip_prefix("State="))
        .map(str::trim)
        .unwrap_or("missing");
    if state.eq_ignore_ascii_case("missing") {
        return ServiceStatus::default();
    }
    ServiceStatus {
        installed: true,
        active: state.eq_ignore_ascii_case("Running"),
        // Task Scheduler knows whether a task is enabled, but mapping its
        // states to "starts at logon" is not reliable; report unknown.
        enabled: None,
    }
}

// --- shared ----------------------------------------------------------------

/// Formats the human status block.
pub fn format_status(platform: ServicePlatform, path: &Path, status: ServiceStatus) -> String {
    let yes_no = |value: bool| if value { "yes" } else { "no" };
    let mut out = String::new();
    out.push_str(&format!("h4rnd service ({})\n", platform.name()));
    out.push_str(&format!(
        "  installed: {}\n",
        if status.installed {
            format!("yes — {}", path.display())
        } else {
            "no".to_owned()
        }
    ));
    out.push_str(&format!("  active:    {}\n", yes_no(status.active)));
    match status.enabled {
        Some(enabled) => out.push_str(&format!("  enabled:   {}\n", yes_no(enabled))),
        None => out.push_str("  enabled:   unknown\n"),
    }
    out
}

/// Whether a unit file must be written: missing or different from the render.
pub fn should_write_file(path: &Path, rendered: &[u8]) -> std::io::Result<bool> {
    match std::fs::read(path) {
        Ok(existing) => Ok(existing != rendered),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error),
    }
}

/// Whether a path looks like a build or temporary directory.
///
/// `~/.cargo/bin` is a legitimate install location; only a Cargo `target/`
/// (or `*-target/`) directory holding `debug`/`release` output is rejected. The
/// temp root is canonicalized first so `/tmp` and `/private/tmp` (or a symlinked
/// cache directory) compare equal.
pub fn is_build_or_temp_path(path: &Path, temp_root: &Path) -> bool {
    let path = canonical_or_owned(path);
    let temp_root = canonical_or_owned(temp_root);
    if path.starts_with(&temp_root) {
        return true;
    }
    let components: Vec<String> = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_ascii_lowercase())
        .collect();
    let mut seen_target = false;
    for component in &components {
        if component == "target" || component.ends_with("-target") {
            seen_target = true;
            continue;
        }
        if seen_target && (component == "debug" || component == "release") {
            return true;
        }
    }
    false
}

/// Canonicalizes a path, falling back to canonicalizing its existing parents so
/// a not-yet-created leaf still resolves a symlinked directory.
fn canonical_or_owned(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
            canonical_or_owned(parent).join(name)
        }
        _ => path.to_path_buf(),
    }
}

/// Resolves a possibly relative path against the current directory without
/// requiring the path to exist yet.
pub fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::path::absolute(path).context("resolve the data directory")
}

/// This executable's absolute path, with the Windows verbatim prefix removed
/// so Task Scheduler and launchd accept it.
pub fn current_binary_path() -> Result<PathBuf> {
    let executable = std::env::current_exe().context("locate the running h4rnd executable")?;
    let executable = executable.canonicalize().unwrap_or(executable);
    Ok(strip_verbatim_prefix(executable))
}

/// Removes the `\\?\` prefix Windows uses for long paths.
pub fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path
}

fn home_dir() -> Result<PathBuf> {
    dirs::home_dir()
        .filter(|home| !home.as_os_str().is_empty())
        .context("no home directory: set HOME (or USERPROFILE on Windows)")
}

fn current_uid() -> Result<u32> {
    #[cfg(unix)]
    {
        Ok(unsafe { libc::getuid() })
    }
    #[cfg(not(unix))]
    {
        bail!("a numeric user id is only available on Unix")
    }
}

fn quote_for_display(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "_-./:=@".contains(ch))
    {
        value.to_owned()
    } else {
        format!("{value:?}")
    }
}

fn command_available(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    find_program(program, &path, cfg!(windows)).is_some()
}

fn find_program(program: &str, path: &OsStr, windows: bool) -> Option<PathBuf> {
    let extensions: &[&str] = if windows {
        &["exe", "cmd", "bat", "com"]
    } else {
        &[""]
    };
    for directory in std::env::split_paths(path) {
        for extension in extensions {
            let candidate = if extension.is_empty() {
                directory.join(program)
            } else {
                directory.join(format!("{program}.{extension}"))
            };
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn ensure_available(program: &str) -> Result<()> {
    if command_available(program) {
        Ok(())
    } else {
        bail!(
            "{program} was not found in PATH; install it or run `h4rnd serve` in a terminal instead"
        )
    }
}

fn run_capture_allow_failure(spec: &CommandSpec) -> Result<Output> {
    Command::new(&spec.program)
        .args(&spec.args)
        .output()
        .with_context(|| format!("run {}", spec.display()))
}

fn run_capture(spec: &CommandSpec) -> Result<Output> {
    let output = run_capture_allow_failure(spec)?;
    if !output.status.success() {
        bail!(
            "`{}` failed ({}): {}",
            spec.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output)
}

fn run_checked(spec: &CommandSpec) -> Result<Output> {
    run_capture(spec)
}

fn run_inherit(spec: &CommandSpec) -> Result<()> {
    let status = Command::new(&spec.program)
        .args(&spec.args)
        .status()
        .with_context(|| format!("run {}", spec.display()))?;
    if !status.success() {
        bail!("`{}` failed ({status})", spec.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_unit_substitutes_and_escapes_paths() {
        let rendered = render_systemd_unit(
            Path::new("/opt/my h4rn/bin/h4rnd"),
            Path::new("/home/a/100% $data/\"quoted\""),
        );
        assert!(rendered.contains("ExecStart=\"/opt/my h4rn/bin/h4rnd\" serve"));
        // Environment= doubles `%` but not `$`: systemd expands specifiers, not
        // variables, in environment values.
        assert!(
            rendered.contains("Environment=\"H4RN_DATA_DIR=/home/a/100%% $data/\\\"quoted\\\"\"")
        );

        // ExecStart= does expand `$`, so it keeps the doubling.
        let rendered =
            render_systemd_unit(Path::new("/opt/price$bin/h4rnd"), Path::new("/home/a/data"));
        assert!(rendered.contains("ExecStart=\"/opt/price$bin/h4rnd\" serve"));

        // The shipped unit's other settings survive the substitution.
        assert!(rendered.contains("Restart=on-failure"));
        assert!(rendered.contains("WantedBy=default.target"));
        assert!(!rendered.contains("/usr/local/bin/h4rnd"));

        // A newline in a path must not become a new directive.
        let rendered =
            render_systemd_unit(Path::new("/bin/h4rnd"), Path::new("/data\nInjected=yes"));
        assert!(rendered.contains("H4RN_DATA_DIR=/data\\nInjected=yes"));
        assert!(!rendered.contains("\nInjected=yes"));
    }

    #[cfg(unix)]
    #[test]
    fn a_dollar_path_is_accepted_by_systemd_analyze() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("price$bin").join("h4rnd");
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::write(&binary, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let unit = render_systemd_unit(&binary, &dir.path().join("data"));
        assert!(
            unit.contains(&format!("ExecStart=\"{}\" serve", binary.display())),
            "the executable token keeps a single `$`: {unit}"
        );

        let unit_path = dir.path().join("h4rnd.service");
        std::fs::write(&unit_path, &unit).unwrap();
        if command_available("systemd-analyze") {
            let output = Command::new("systemd-analyze")
                .args(["verify", &unit_path.to_string_lossy()])
                .output()
                .expect("run systemd-analyze verify");
            assert!(
                output.status.success(),
                "systemd-analyze verify rejected the unit: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn a_rendered_unit_round_trips_to_its_program() {
        let binary = Path::new("/opt/my h4rn/price$%bin/h4rnd");
        let rendered = render_systemd_unit(binary, Path::new("/home/a/data"));
        assert_eq!(
            systemd_unit_program(&rendered),
            Some(binary.to_string_lossy().into_owned())
        );
        // Unquoted paths, prefixes and specifier escapes are understood.
        assert_eq!(
            systemd_exec_program("-/usr/bin/h4rnd serve"),
            Some("/usr/bin/h4rnd".to_owned())
        );
        assert_eq!(
            systemd_exec_program("\"/opt/a b/h4rnd\" serve"),
            Some("/opt/a b/h4rnd".to_owned())
        );
        assert_eq!(
            systemd_exec_program("\"/opt/price$$bin/h4rnd\" serve"),
            Some("/opt/price$bin/h4rnd".to_owned())
        );
        assert_eq!(systemd_exec_program("   "), None);
    }

    #[test]
    fn launchd_plist_substitutes_and_escapes_paths() {
        let rendered = render_launchd_plist(
            Path::new("/Applications/H4rn & Co/h4rnd"),
            Path::new("/Users/a/<data>"),
            Path::new("/Users/a & b"),
        );
        assert!(rendered.contains("<string>/Applications/H4rn &amp; Co/h4rnd</string>"));
        assert!(rendered.contains("<string>/Users/a/&lt;data&gt;</string>"));
        assert!(rendered.contains("<string>/Users/a &amp; b/Library/Logs/h4rn/h4rnd.log</string>"));
        assert!(!rendered.contains("__HOME__"));
        assert!(!rendered.contains("/usr/local/bin/h4rnd"));
        assert_eq!(
            launchd_program(&rendered).as_deref(),
            Some("/Applications/H4rn & Co/h4rnd")
        );

        // A data directory containing the literal placeholder is not
        // re-expanded by the later home substitution.
        let rendered = render_launchd_plist(
            Path::new("/bin/h4rnd"),
            Path::new("/data/__HOME__/x"),
            Path::new("/Users/a"),
        );
        assert!(rendered.contains("<string>/data/__HOME__/x</string>"));
    }

    #[test]
    fn unit_paths_follow_xdg_and_home() {
        assert_eq!(
            systemd_unit_path_in(Path::new("/home/a"), None),
            PathBuf::from("/home/a/.config/systemd/user/h4rnd.service")
        );
        assert_eq!(
            systemd_unit_path_in(Path::new("/home/a"), Some(Path::new("/cfg"))),
            PathBuf::from("/cfg/systemd/user/h4rnd.service")
        );
        // A relative XDG_CONFIG_HOME is ignored, as the XDG spec says.
        assert_eq!(
            systemd_unit_path_in(Path::new("/home/a"), Some(Path::new("cfg"))),
            PathBuf::from("/home/a/.config/systemd/user/h4rnd.service")
        );
        assert_eq!(
            launchd_plist_path_in(Path::new("/Users/a")),
            PathBuf::from("/Users/a/Library/LaunchAgents/dev.h4rn.h4rnd.plist")
        );
    }

    #[test]
    fn linux_commands_are_argument_arrays() {
        let install = systemd_install_commands();
        assert_eq!(
            install,
            vec![
                CommandSpec::new("systemctl", ["--user", "daemon-reload"]),
                CommandSpec::new("systemctl", ["--user", "enable", "--now", "h4rnd.service"]),
            ]
        );
        assert_eq!(
            systemd_uninstall_commands(),
            vec![CommandSpec::new(
                "systemctl",
                ["--user", "disable", "--now", "h4rnd.service"]
            )]
        );
        assert_eq!(
            systemd_restart_command(),
            CommandSpec::new("systemctl", ["--user", "restart", "h4rnd.service"])
        );
        assert_eq!(
            systemd_logs_command(),
            CommandSpec::new(
                "journalctl",
                ["--user", "-u", "h4rnd", "-n", "200", "--no-pager"]
            )
        );
        // No command may ask a shell to reinterpret a path.
        for command in install {
            assert!(!matches!(
                command.program.as_str(),
                "sh" | "bash" | "cmd" | "powershell"
            ));
            assert!(command.args.iter().all(|arg| arg != "-c"));
        }
    }

    #[test]
    fn macos_commands_are_argument_arrays() {
        let plist = Path::new("/Users/a/Library/LaunchAgents/dev.h4rn.h4rnd.plist");
        assert_eq!(
            launchd_install_commands(501, plist),
            vec![
                CommandSpec::new("launchctl", ["bootout", "gui/501/dev.h4rn.h4rnd"]),
                CommandSpec::new(
                    "launchctl",
                    [
                        "bootstrap",
                        "gui/501",
                        "/Users/a/Library/LaunchAgents/dev.h4rn.h4rnd.plist"
                    ]
                ),
            ]
        );
        assert_eq!(
            launchd_uninstall_commands(501),
            vec![CommandSpec::new(
                "launchctl",
                ["bootout", "gui/501/dev.h4rn.h4rnd"]
            )]
        );
        assert_eq!(
            launchd_restart_command(501),
            CommandSpec::new("launchctl", ["kickstart", "-k", "gui/501/dev.h4rn.h4rnd"])
        );
    }

    #[test]
    fn reinstalling_a_running_launchd_job_leaves_it_alone() {
        assert!(!should_reload_launchd(false, true));
        assert!(should_reload_launchd(true, true));
        assert!(should_reload_launchd(false, false));
    }

    #[test]
    fn windows_task_names_are_per_account() {
        assert_eq!(windows_task_name_for(Some("alice")), "h4rn-alice");
        assert_eq!(windows_task_name_for(Some(r"DESKTOP\alice")), "h4rn-alice");
        assert_eq!(
            windows_task_name_for(Some("alice smith")),
            "h4rn-alice-smith"
        );
        assert_eq!(windows_task_name_for(Some("a'b")), "h4rn-a-b");
        assert_eq!(windows_task_name_for(Some("")), "h4rn");
        assert_eq!(windows_task_name_for(None), "h4rn");
        // No quote can escape the PowerShell single-quoted literal.
        assert!(!windows_task_name_for(Some("a'; rm -rf")).contains('\''));
    }

    #[test]
    fn windows_commands_run_the_shipped_scripts_without_a_shell_string() {
        let script = Path::new(r"C:\Users\a\AppData\Local\Temp\h4rn-service-1\install.ps1");
        let install = windows_install_command(
            script,
            Path::new(r"C:\Program Files\h4rn\h4rnd.exe"),
            Path::new(r"C:\Users\a\AppData\Local\h4rn"),
            "h4rn-alice",
        );
        assert_eq!(install.program, "powershell");
        assert_eq!(
            install.args,
            vec![
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                r"C:\Users\a\AppData\Local\Temp\h4rn-service-1\install.ps1",
                "-Binary",
                r"C:\Program Files\h4rn\h4rnd.exe",
                "-DataDir",
                r"C:\Users\a\AppData\Local\h4rn",
                "-TaskName",
                "h4rn-alice",
                "-LegacyTaskName",
                "h4rn",
            ]
        );
        let uninstall = windows_uninstall_command(script, "h4rn-alice");
        assert_eq!(uninstall.program, "powershell");
        assert_eq!(
            uninstall.args,
            vec![
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                r"C:\Users\a\AppData\Local\Temp\h4rn-service-1\install.ps1",
                "-TaskName",
                "h4rn-alice",
                "-LegacyTaskName",
                "h4rn",
            ]
        );
        let restart = windows_restart_steps("h4rn-alice");
        assert_eq!(
            restart,
            vec![
                // `/End` may fail when the task is not running; only `/Run` is
                // required.
                (
                    CommandSpec::new("schtasks", ["/End", "/TN", "h4rn-alice"]),
                    false
                ),
                (
                    CommandSpec::new("schtasks", ["/Run", "/TN", "h4rn-alice"]),
                    true
                ),
            ]
        );
        let status = windows_status_command("h4rn-alice");
        assert!(
            status
                .args
                .iter()
                .any(|arg| arg.contains("Get-ScheduledTask")),
            "status must not depend on English schtasks text"
        );
        // The action query must not let a newline split one action into two.
        let action = windows_task_executable_command("h4rn-alice");
        assert!(
            action
                .args
                .iter()
                .any(|arg| arg.contains(".Replace(\"`r\", ' ')")),
            "action output must be flattened to one line"
        );
        // An action without arguments has a null `Arguments`, which used to
        // throw before any output was printed.
        assert!(
            action.args.iter().any(|arg| {
                arg.contains("$(if ($null -ne $_.Arguments) { $_.Arguments } else { '' })")
            }),
            "the action query must tolerate a null Arguments value"
        );
    }

    #[test]
    fn a_private_script_directory_is_exclusive_and_removed() {
        let first = PrivateScript::create("install.ps1", "Write-Host hi").unwrap();
        let second = PrivateScript::create("install.ps1", "Write-Host hi").unwrap();
        assert_ne!(first.directory, second.directory);
        assert!(first.path().exists());
        assert_eq!(
            std::fs::read_to_string(first.path()).unwrap(),
            "Write-Host hi"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&first.directory)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "the directory is private to its owner");
        }
        let file = first.path().to_path_buf();
        let directory = first.directory.clone();
        drop(first);
        assert!(!file.exists());
        assert!(!directory.exists());
        drop(second);
    }

    #[test]
    fn a_windows_restart_ignores_a_failed_stop_but_not_a_failed_start() {
        let mut seen = Vec::new();
        run_windows_restart("h4rn-alice", |command| {
            seen.push(command.args[0].clone());
            if command.args[0] == "/End" {
                anyhow::bail!("the task is not running");
            }
            Ok(())
        })
        .expect("a stop that fails because nothing is running is harmless");
        assert_eq!(seen, ["/End", "/Run"]);

        let error = run_windows_restart("h4rn-alice", |command| {
            if command.args[0] == "/Run" {
                anyhow::bail!("schtasks refused to start the task");
            }
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("refused to start"), "{error}");
    }

    #[test]
    fn status_parsers_report_installed_active_and_enabled() {
        let status =
            parse_systemd_show("LoadState=loaded\nActiveState=active\nUnitFileState=enabled\n");
        assert_eq!(
            status,
            ServiceStatus {
                installed: true,
                active: true,
                enabled: Some(true),
            }
        );
        let status =
            parse_systemd_show("LoadState=not-found\nActiveState=inactive\nUnitFileState=\n");
        assert!(!status.installed);
        assert!(!status.active);
        assert_eq!(status.enabled, Some(false));
        assert!(parse_linger("Linger=yes\n"));
        assert!(!parse_linger("Linger=no\n"));
        assert!(parse_launchd_print("state = running\n\tpid = 42\n").active);
        assert!(!parse_launchd_print("state = exited\n").active);

        // Task state comes from the enum, not localized status text.
        let status = parse_windows_task_state("State=Running\n");
        assert!(status.installed && status.active);
        assert_eq!(status.enabled, None, "enabled is unknown on Windows");
        let status = parse_windows_task_state("State=Ready\n");
        assert!(status.installed && !status.active);
        assert_eq!(
            parse_windows_task_state("State=missing\n"),
            ServiceStatus::default()
        );
        // English status text from an old schtasks call is not used.
        assert_eq!(
            parse_windows_task_state("Status: Running\n"),
            ServiceStatus::default()
        );
    }

    #[test]
    fn the_effective_exec_start_comes_from_home_and_dropins() {
        let home = Path::new("/home/a");
        let unit = SystemdUnitText {
            fragment: "[Service]\n\
                       ExecStart=%h/.local/bin/h4rnd serve\n\
                       Environment=H4RN_DATA_DIR=%h/.local/share/h4rn H4RN_BIND=127.0.0.1:47901\n"
                .to_owned(),
            dropins: Vec::new(),
        };
        assert_eq!(
            unit.exec_start(home)
                .as_deref()
                .and_then(systemd_exec_program),
            Some("/home/a/.local/bin/h4rnd".to_owned()),
            "%h is expanded the way systemd expands it"
        );
        assert_eq!(
            unit.health_endpoint(home, Path::new("/nonexistent"))
                .unwrap(),
            "127.0.0.1:47901".parse::<SocketAddr>().unwrap()
        );

        // A drop-in overrides both the command and the environment.
        let with_dropin = SystemdUnitText {
            fragment: unit.fragment.clone(),
            dropins: vec![
                "[Service]\nExecStart=/opt/other/h4rnd serve\n".to_owned(),
                "[Service]\nEnvironment=H4RN_BIND=\"127.0.0.1:47902\"\n".to_owned(),
            ],
        };
        assert_eq!(
            with_dropin
                .exec_start(home)
                .as_deref()
                .and_then(systemd_exec_program),
            Some("/opt/other/h4rnd".to_owned())
        );
        assert_eq!(
            with_dropin
                .health_endpoint(home, Path::new("/nonexistent"))
                .unwrap(),
            "127.0.0.1:47902".parse::<SocketAddr>().unwrap()
        );

        // No bind in the service means the daemon's compiled default, not this
        // updater's --bind.
        let default = SystemdUnitText {
            fragment: "ExecStart=/usr/local/bin/h4rnd serve\n".to_owned(),
            dropins: Vec::new(),
        };
        assert_eq!(
            default
                .health_endpoint(home, Path::new("/nonexistent"))
                .unwrap(),
            crate::config::DEFAULT_BIND.parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn unit_dropins_are_read_from_the_units_directories() {
        let dir = tempfile::tempdir().unwrap();
        let fragment = dir.path().join("h4rnd.service");
        std::fs::write(&fragment, "ExecStart=/usr/bin/one serve\n").unwrap();
        let dropins = dir.path().join("h4rnd.service.d");
        std::fs::create_dir(&dropins).unwrap();
        std::fs::write(
            dropins.join("20-other.conf"),
            "ExecStart=/usr/bin/two serve\n",
        )
        .unwrap();
        std::fs::write(
            dropins.join("10-bind.conf"),
            "Environment=H4RN_BIND=127.0.0.1:47903\n",
        )
        .unwrap();
        // Not a drop-in.
        std::fs::write(dropins.join("notes.txt"), "ExecStart=/usr/bin/ignored\n").unwrap();

        let text = SystemdUnitText::read(&fragment).unwrap();
        assert_eq!(text.dropins.len(), 2, "only *.conf files are drop-ins");
        assert_eq!(
            text.exec_start(Path::new("/home/a"))
                .as_deref()
                .and_then(systemd_exec_program),
            Some("/usr/bin/two".to_owned()),
            "drop-ins are applied in file-name order"
        );
        assert_eq!(
            text.health_endpoint(Path::new("/home/a"), Path::new("/nonexistent"))
                .unwrap(),
            "127.0.0.1:47903".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn launchd_enabled_is_unknown_without_print_disabled() {
        let status = launchd_service_status("state = running\n", None);
        assert!(status.active);
        assert_eq!(
            status.enabled, None,
            "enabled is unknown unless print-disabled was read"
        );

        let disabled = "disabled services = {\n\t\"dev.h4rn.h4rnd\" => true\n}\n";
        assert_eq!(
            launchd_service_status("state = running\n", Some(disabled)).enabled,
            Some(false)
        );
        let enabled = "disabled services = {\n\t\"dev.h4rn.h4rnd\" => false\n}\n";
        assert_eq!(
            launchd_service_status("state = running\n", Some(enabled)).enabled,
            Some(true)
        );
        // Not listed in a successfully read table means enabled.
        assert_eq!(
            launchd_service_status("state = running\n", Some("disabled services = {\n}\n")).enabled,
            Some(true)
        );
        // Newer launchctl versions spell the state instead of using a bool.
        assert!(parse_launchd_disabled(
            "\"dev.h4rn.h4rnd\" => disabled\n",
            LAUNCHD_LABEL
        ));
    }

    #[test]
    fn launchd_environment_names_the_service_bind() {
        let plist = render_launchd_plist(
            Path::new("/usr/local/bin/h4rnd"),
            Path::new("/Users/a/data"),
            Path::new("/Users/a"),
        );
        assert_eq!(
            launchd_health_endpoint(&plist).unwrap(),
            "127.0.0.1:47800".parse::<SocketAddr>().unwrap()
        );
        let plist = plist.replace(
            "<string>127.0.0.1:47800</string>",
            "<string>127.0.0.1:47904</string>",
        );
        assert_eq!(
            launchd_health_endpoint(&plist).unwrap(),
            "127.0.0.1:47904".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn status_block_is_stable() {
        let block = format_status(
            ServicePlatform::Systemd,
            Path::new("/home/a/.config/systemd/user/h4rnd.service"),
            ServiceStatus {
                installed: true,
                active: true,
                enabled: Some(false),
            },
        );
        assert_eq!(
            block,
            "h4rnd service (systemd)\n  installed: yes — /home/a/.config/systemd/user/h4rnd.service\n  active:    yes\n  enabled:   no\n"
        );
    }

    #[test]
    fn install_rewrites_a_missing_or_changed_unit_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h4rnd.service");
        assert!(should_write_file(&path, b"one").unwrap());
        std::fs::write(&path, "one").unwrap();
        assert!(!should_write_file(&path, b"one").unwrap());
        assert!(should_write_file(&path, b"two").unwrap());
    }

    #[test]
    fn build_and_temp_paths_are_rejected() {
        let root = Path::new("/home/a/src/h4rn");
        assert!(is_build_or_temp_path(
            &root.join("target/debug/h4rnd"),
            Path::new("/tmp")
        ));
        assert!(is_build_or_temp_path(
            &root.join("target/release/h4rnd"),
            Path::new("/tmp")
        ));
        assert!(is_build_or_temp_path(
            Path::new("/home/a/.cache/h4rn-target/impl-22/debug/h4rnd"),
            Path::new("/tmp")
        ));
        assert!(is_build_or_temp_path(
            Path::new("/tmp/h4rnd"),
            Path::new("/tmp")
        ));
        assert!(!is_build_or_temp_path(
            &root.join("bin/h4rnd"),
            Path::new("/tmp")
        ));
        assert!(!is_build_or_temp_path(
            Path::new("/usr/local/bin/h4rnd"),
            Path::new("/tmp")
        ));
        // Cargo's bin directory is where a real install lives.
        assert!(!is_build_or_temp_path(
            Path::new("/home/a/.cargo/bin/h4rnd"),
            Path::new("/tmp")
        ));
        // `release` only counts directly under a Cargo target directory.
        assert!(!is_build_or_temp_path(
            Path::new("/home/a/release/h4rnd"),
            Path::new("/tmp")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_temp_dir_is_still_a_temp_dir() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        symlink(&real, &link).unwrap();

        // The path goes through the link; the temp root is the real directory.
        assert!(is_build_or_temp_path(&link.join("h4rnd"), &real));
        // And the other way round.
        assert!(is_build_or_temp_path(&real.join("h4rnd"), &link));
        // A path outside both is not a temp path.
        assert!(!is_build_or_temp_path(&dir.path().join("bin/h4rnd"), &real));
    }

    #[test]
    fn a_windows_task_recovery_step_names_the_binary_it_restores() {
        let command = "-NoProfile -NonInteractive -ExecutionPolicy Bypass -Command \
             $b = 'C:\\Program Files\\h4rn\\h4rnd.exe'; \
             $p = 'C:\\Program Files\\h4rn\\h4rnd.previous.exe'; \
             $o = 'C:\\Program Files\\h4rn\\h4rnd.old.exe'; \
             $d = 'C:\\Users\\a\\AppData\\Local\\h4rn'; \
             if (-not (Test-Path -LiteralPath $b)) { \
               if (Test-Path -LiteralPath $o) { \
                 Move-Item -LiteralPath $o -Destination $b -Force \
               } elseif (Test-Path -LiteralPath $p) { \
                 Copy-Item -LiteralPath $p -Destination $b -Force \
               } \
             }; & $b --data-dir $d serve; exit $LASTEXITCODE";
        assert_eq!(
            inline_task_binary(command),
            Some(PathBuf::from(r"C:\Program Files\h4rn\h4rnd.exe"))
        );
        // PowerShell doubles a quote in a single-quoted literal.
        assert_eq!(
            inline_task_binary("$b = 'C:\\it''s\\h4rnd.exe'; $p = ''"),
            Some(PathBuf::from(r"C:\it's\h4rnd.exe"))
        );
        assert_eq!(
            inline_task_binary("-NoProfile -Command Write-Host hi"),
            None
        );
        assert_eq!(
            windows_installed_binary(&format!("powershell.exe\t{command}\n")),
            InstalledService::Runs(PathBuf::from(r"C:\Program Files\h4rn\h4rnd.exe"))
        );

        // A task from an older install runs the binary directly.
        assert_eq!(
            windows_installed_binary("C:\\h4rn\\h4rnd.exe\t--data-dir C:\\data serve\n"),
            InstalledService::Runs(PathBuf::from(r"C:\h4rn\h4rnd.exe"))
        );
        assert_eq!(windows_installed_binary(""), InstalledService::NotInstalled);
    }

    #[test]
    fn the_shipped_windows_install_script_registers_the_recovery_launcher() {
        assert!(WINDOWS_INSTALL_SCRIPT.contains("$format = '$b = {0}"));
        // Recovery belongs to the launcher so it also works with an older image
        // that does not understand the daemon's `--recover-only` flag.
        let launcher = &WINDOWS_INSTALL_SCRIPT[WINDOWS_INSTALL_SCRIPT.find("$format = ").unwrap()
            ..WINDOWS_INSTALL_SCRIPT.find("$values = ").unwrap()];
        assert!(!launcher.contains("--recover-only"));
        // The launcher takes the updater's own lock file, waiting bounded.
        assert!(WINDOWS_INSTALL_SCRIPT.contains(r#".update" + $binaryExt + ".lock"#));
        assert!(WINDOWS_INSTALL_SCRIPT.contains("AddSeconds(30)"));
        assert!(
            WINDOWS_INSTALL_SCRIPT
                .contains(r#"[IO.File]::Open($l, "OpenOrCreate", "ReadWrite", "None")"#)
        );
        // The lock is taken before any recovery step runs.
        assert!(
            launcher.find("[IO.File]::Open").unwrap()
                < launcher.find("Test-Path -LiteralPath $o").unwrap()
        );
        // A moved-aside binary is renamed back; a rollback copy is copied to a
        // staging file and then renamed, never copied straight over h4rnd.exe.
        assert!(
            WINDOWS_INSTALL_SCRIPT.contains("Move-Item -LiteralPath $o -Destination $b -Force")
        );
        assert!(
            WINDOWS_INSTALL_SCRIPT.contains("Move-Item -LiteralPath $x -Destination $b -Force")
        );
        assert!(
            WINDOWS_INSTALL_SCRIPT.contains("Copy-Item -LiteralPath $p -Destination $t -Force")
        );
        assert!(
            WINDOWS_INSTALL_SCRIPT.contains("Move-Item -LiteralPath $t -Destination $b -Force")
        );
        // The branch order matches startup recovery: interrupted update, then
        // interrupted rollback, then the rollback copy.
        assert!(
            launcher.find("Test-Path -LiteralPath $o").unwrap()
                < launcher.find("Test-Path -LiteralPath $x").unwrap()
        );
        assert!(
            launcher.find("Test-Path -LiteralPath $x").unwrap()
                < launcher.find("Test-Path -LiteralPath $p").unwrap()
        );
        // Its staging name matches the updater's (`is_staging_name`).
        assert!(WINDOWS_INSTALL_SCRIPT.contains(r#"$f + ".update-" + $PID + "-0-""#));
        // The lock is released before the daemon starts.
        assert!(
            WINDOWS_INSTALL_SCRIPT.find("$lock.Dispose()").unwrap()
                < WINDOWS_INSTALL_SCRIPT
                    .find("& $b --data-dir $d serve")
                    .unwrap()
        );
        assert!(WINDOWS_INSTALL_SCRIPT.contains("& $b --data-dir $d serve"));
        // A rewrite before an update must not bounce the running daemon.
        assert!(WINDOWS_INSTALL_SCRIPT.contains("$NoStart"));
        // The step is inline, so there is no script file to leave behind.
        assert!(!WINDOWS_INSTALL_SCRIPT.contains("h4rnd-launch"));
        // Both scripts migrate the machine-wide task older releases used.
        let install = WINDOWS_INSTALL_SCRIPT.to_ascii_lowercase();
        assert!(install.contains("$legacytaskname"));
        assert!(install.contains("unregister-scheduledtask"));
        let uninstall = WINDOWS_UNINSTALL_SCRIPT.to_ascii_lowercase();
        assert!(uninstall.contains("$legacytaskname"));
        assert!(uninstall.contains("unregister-scheduledtask"));
    }

    #[test]
    fn the_restart_decision_only_restarts_the_replaced_binary() {
        let current = Path::new("/opt/h4rn/h4rnd");
        assert_eq!(
            restart_decision(&InstalledService::NotInstalled, current),
            RestartDecision::NotInstalled
        );
        assert_eq!(
            restart_decision(&InstalledService::Unknown, current),
            RestartDecision::Unknown
        );
        assert_eq!(
            restart_decision(&InstalledService::Runs(current.to_path_buf()), current),
            RestartDecision::Restart
        );
        assert_eq!(
            restart_decision(
                &InstalledService::Runs(PathBuf::from("/usr/local/bin/h4rnd")),
                current
            ),
            RestartDecision::OtherInstall(PathBuf::from("/usr/local/bin/h4rnd"))
        );
    }

    #[test]
    fn same_binary_follows_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("h4rnd");
        std::fs::write(&real, b"bin").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let link = dir.path().join("h4rnd-link");
            symlink(&real, &link).unwrap();
            assert!(same_binary(&real, &link));
        }
        assert!(same_binary(&real, &real));
        assert!(!same_binary(&real, &dir.path().join("missing")));
    }

    /// A runner that answers systemctl commands from a fixed table.
    fn systemd_runner(
        calls: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
        exec_start: std::result::Result<&'static str, &'static str>,
        environment: std::result::Result<&'static str, &'static str>,
    ) -> impl Fn(&CommandSpec) -> Result<CommandOutput> {
        move |spec: &CommandSpec| {
            calls.borrow_mut().push(spec.display());
            if spec.args.iter().any(|arg| arg == "--property=ExecStart") {
                return match exec_start {
                    Ok(stdout) => Ok(CommandOutput {
                        success: true,
                        stdout: stdout.as_bytes().to_vec(),
                    }),
                    Err(error) => Err(anyhow::anyhow!("{error}")),
                };
            }
            if spec.args.iter().any(|arg| arg == "show-environment") {
                return match environment {
                    Ok(stdout) => Ok(CommandOutput {
                        success: true,
                        stdout: stdout.as_bytes().to_vec(),
                    }),
                    Err(error) => Err(anyhow::anyhow!("{error}")),
                };
            }
            Ok(CommandOutput::default())
        }
    }

    #[test]
    fn a_service_bind_argument_wins_over_the_unit_and_manager_environment() {
        let dir = tempfile::tempdir().unwrap();
        let fragment = dir.path().join("h4rnd.service");
        std::fs::write(
            &fragment,
            "ExecStart=/opt/h4rnd/h4rnd serve\nEnvironment=H4RN_BIND=127.0.0.1:48002\n",
        )
        .unwrap();
        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let run = systemd_runner(
            calls.clone(),
            Ok(
                "{ path=/opt/h4rnd/h4rnd ; argv[]=/opt/h4rnd/h4rnd serve --bind 127.0.0.1:48001 ; ignore_errors=no }",
            ),
            Ok("H4RN_BIND=127.0.0.1:48003\nPATH=/usr/bin\n"),
        );

        match systemd_target_from(&fragment, Path::new("/home/a"), &run) {
            RegisteredTarget::Runs { binary, health } => {
                assert_eq!(binary, PathBuf::from("/opt/h4rnd/h4rnd"));
                assert_eq!(health, "127.0.0.1:48001".parse::<SocketAddr>().unwrap());
            }
            other => panic!("expected a resolved service, got {other:?}"),
        }
        assert!(
            !calls
                .borrow()
                .iter()
                .any(|call| call.contains("show-environment")),
            "the manager environment is irrelevant when the command line names a bind"
        );
    }

    #[test]
    fn an_environment_file_names_the_service_bind() {
        let dir = tempfile::tempdir().unwrap();
        let fragment = dir.path().join("h4rnd.service");
        let environment = dir.path().join("bind.env");
        std::fs::write(
            &environment,
            "# bind for this host\nH4RN_BIND='127.0.0.1:48004'\n",
        )
        .unwrap();
        std::fs::write(
            &fragment,
            format!(
                "ExecStart=/opt/h4rnd/h4rnd serve\n\
                 Environment=H4RN_BIND=127.0.0.1:48002\n\
                 EnvironmentFile={}\n",
                environment.display()
            ),
        )
        .unwrap();
        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        // systemctl is unavailable, so the unit and its environment files are
        // the source of truth.
        let run = systemd_runner(calls, Err("systemctl not found"), Err("no manager"));

        match systemd_target_from(&fragment, Path::new("/home/a"), &run) {
            RegisteredTarget::Runs { health, .. } => {
                assert_eq!(
                    health,
                    "127.0.0.1:48004".parse::<SocketAddr>().unwrap(),
                    "EnvironmentFile overrides Environment=, as systemd does"
                );
            }
            other => panic!("expected a resolved service, got {other:?}"),
        }
    }

    #[test]
    fn a_dropin_can_reset_and_override_the_units_exec_start() {
        let dir = tempfile::tempdir().unwrap();
        let fragment = dir.path().join("h4rnd.service");
        std::fs::write(&fragment, "ExecStart=/usr/bin/base serve\n").unwrap();
        let dropins = dir.path().join("h4rnd.service.d");
        std::fs::create_dir(&dropins).unwrap();
        std::fs::write(dropins.join("10-reset.conf"), "ExecStart=\n").unwrap();
        std::fs::write(
            dropins.join("20-replace.conf"),
            "ExecStart=/opt/other/h4rnd serve\nEnvironment=H4RN_BIND=127.0.0.1:48005\n",
        )
        .unwrap();

        // No systemctl: the files are read, honouring the empty reset.
        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let run = systemd_runner(calls, Err("systemctl not found"), Err("no manager"));
        match systemd_target_from(&fragment, Path::new("/home/a"), &run) {
            RegisteredTarget::Runs { binary, health } => {
                assert_eq!(binary, PathBuf::from("/opt/other/h4rnd"));
                assert_eq!(health, "127.0.0.1:48005".parse::<SocketAddr>().unwrap());
            }
            other => panic!("expected a resolved service, got {other:?}"),
        }
    }

    #[test]
    fn an_unreadable_user_manager_environment_leaves_the_service_unverified() {
        let dir = tempfile::tempdir().unwrap();
        let fragment = dir.path().join("h4rnd.service");
        std::fs::write(&fragment, "ExecStart=/opt/h4rnd/h4rnd serve\n").unwrap();
        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let run = systemd_runner(
            calls,
            Ok("{ path=/opt/h4rnd/h4rnd ; argv[]=/opt/h4rnd/h4rnd serve ; ignore_errors=no }"),
            Err("Failed to connect to bus"),
        );
        match systemd_target_from(&fragment, Path::new("/home/a"), &run) {
            RegisteredTarget::Unverified { reason } => {
                assert!(reason.contains("show-environment"), "{reason}");
            }
            other => panic!("an unreadable source must fail closed, got {other:?}"),
        }
    }

    #[test]
    fn the_windows_prepare_step_rewrites_the_action_without_starting_it() {
        let script = Path::new(r"C:\Users\a\AppData\Local\Temp\h4rn-service-1\install.ps1");
        let binary = Path::new(r"C:\Program Files\h4rn\h4rnd.exe");
        let data = Path::new(r"C:\Users\a\AppData\Local\h4rn");
        let install = windows_install_command(script, binary, data, "h4rn-alice");
        assert!(!install.args.iter().any(|arg| arg == "-NoStart"));
        let prepare = windows_prepare_command(script, binary, data, "h4rn-alice");
        assert_eq!(prepare.args.last().map(String::as_str), Some("-NoStart"));
        assert_eq!(
            &prepare.args[..install.args.len()],
            &install.args[..],
            "the prepare step is the install action plus -NoStart"
        );
    }

    #[test]
    fn a_launchd_bind_argument_wins_over_its_environment() {
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("dev.h4rn.h4rnd.plist");
        std::fs::write(
            &plist,
            format!(
                "<dict>\n\
                 <key>ProgramArguments</key>\n<array>\n\
                 <string>/opt/h4rnd/h4rnd</string>\n\
                 <string>serve</string>\n\
                 <string>--bind</string>\n\
                 <string>127.0.0.1:48007</string>\n\
                 </array>\n\
                 <key>EnvironmentVariables</key>\n<dict>\n\
                 <key>H4RN_BIND</key>\n<string>127.0.0.1:48008</string>\n\
                 </dict>\n</dict>\n{}",
                crate::service::LAUNCHD_LABEL
            ),
        )
        .unwrap();
        match launchd_target_from(&plist) {
            RegisteredTarget::Runs { binary, health } => {
                assert_eq!(binary, PathBuf::from("/opt/h4rnd/h4rnd"));
                assert_eq!(health, "127.0.0.1:48007".parse::<SocketAddr>().unwrap());
            }
            other => panic!("expected a resolved service, got {other:?}"),
        }
    }

    #[test]
    fn a_windows_action_bind_wins_over_the_account_environment() {
        let action = windows_task_action(
            "C:\\h4rn\\h4rnd.exe\t--bind 127.0.0.1:48011 --data-dir C:\\d serve\n",
        )
        .expect("the action names its binary");
        assert_eq!(
            bind_argument(action.arguments.split_whitespace()),
            Some("127.0.0.1:48011".to_owned())
        );
        assert_eq!(
            parse_windows_bind_env("User=127.0.0.1:48012\n"),
            Some("127.0.0.1:48012".to_owned())
        );
        assert_eq!(
            parse_windows_bind_env("Machine=127.0.0.1:48013\n"),
            Some("127.0.0.1:48013".to_owned())
        );
        assert_eq!(parse_windows_bind_env("Unset\n"), None);
        assert_eq!(
            bind_argument(
                ["--bind=127.0.0.1:48014", "--bind", "127.0.0.1:48015"].map(String::from)
            ),
            Some("127.0.0.1:48015".to_owned()),
            "the last flag wins"
        );
        assert_eq!(bind_argument(["serve"].map(String::from)), None);

        // The account environment is the next source when the action has no
        // `--bind`, and an unreadable one leaves the target unverified.
        let launcher = TaskAction {
            execute: "powershell.exe".to_owned(),
            arguments: "-NoProfile -Command $b = 'C:\\h4rn\\h4rnd.exe'; & $b serve".to_owned(),
        };
        match windows_target_from_action(&launcher, || Ok(Some("127.0.0.1:48016".to_owned()))) {
            RegisteredTarget::Runs { binary, health } => {
                assert_eq!(binary, PathBuf::from(r"C:\h4rn\h4rnd.exe"));
                assert_eq!(health, "127.0.0.1:48016".parse::<SocketAddr>().unwrap());
            }
            other => panic!("expected a resolved service, got {other:?}"),
        }
        match windows_target_from_action(&launcher, || Err("access denied".to_owned())) {
            RegisteredTarget::Unverified { reason } => {
                assert!(reason.contains("access denied"), "{reason}");
            }
            other => panic!("an unreadable environment must fail closed, got {other:?}"),
        }
        // A `--bind` in the action is used without consulting the environment.
        let direct = TaskAction {
            execute: r"C:\h4rn\h4rnd.exe".to_owned(),
            arguments: "--bind 127.0.0.1:48017 serve".to_owned(),
        };
        match windows_target_from_action(&direct, || panic!("must not read the environment")) {
            RegisteredTarget::Runs { health, .. } => {
                assert_eq!(health, "127.0.0.1:48017".parse::<SocketAddr>().unwrap());
            }
            other => panic!("expected a resolved service, got {other:?}"),
        }
    }
}
