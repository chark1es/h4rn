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
//! repaired.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};

use crate::config::Config;

/// The systemd user unit name.
pub const UNIT_NAME: &str = "h4rnd.service";
/// The launchd job label.
pub const LAUNCHD_LABEL: &str = "dev.h4rn.h4rnd";
/// The Windows Task Scheduler task name.
pub const WINDOWS_TASK_NAME: &str = "h4rn";

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
            let output = if command_available("systemctl") {
                Some(run_capture_allow_failure(&systemd_status_command())?)
            } else {
                None
            };
            let mut status = output
                .as_ref()
                .map(|output| parse_systemd_show(&String::from_utf8_lossy(&output.stdout)))
                .unwrap_or_default();
            status.installed |= path.exists();
            (ServicePlatform::Systemd, path, status)
        }
        ServicePlatform::Launchd => {
            let path = launchd_plist_path()?;
            let output = if command_available("launchctl") {
                Some(run_capture_allow_failure(&launchd_status_command(
                    current_uid()?,
                ))?)
            } else {
                None
            };
            let mut status = output
                .as_ref()
                .filter(|output| output.status.success())
                .map(|output| ServiceStatus {
                    installed: true,
                    active: parse_launchd_print(&String::from_utf8_lossy(&output.stdout)).active,
                    enabled: Some(true),
                })
                .unwrap_or_default();
            status.installed |= path.exists();
            (ServicePlatform::Launchd, path, status)
        }
        ServicePlatform::Windows => {
            let output = if command_available("schtasks") {
                Some(run_capture_allow_failure(&windows_status_command())?)
            } else {
                None
            };
            let status = output
                .as_ref()
                .filter(|output| output.status.success())
                .map(|output| parse_schtasks(&String::from_utf8_lossy(&output.stdout)))
                .unwrap_or_default();
            (
                ServicePlatform::Windows,
                PathBuf::from(WINDOWS_TASK_NAME),
                status,
            )
        }
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
            for command in windows_restart_commands() {
                run_checked(&command)?;
            }
            println!("Restarted scheduled task {WINDOWS_TASK_NAME}.");
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
            println!(
                "Task Scheduler runs {WINDOWS_TASK_NAME} without a console, so h4rnd has no log file."
            );
            println!("Run `h4rnd serve` in a terminal to watch logs, or open Task Scheduler →");
            println!("{WINDOWS_TASK_NAME} → History for start and exit events.");
            Ok(())
        }
    }
}

/// Whether the service is installed. Used by `h4rnd update` to decide whether
/// it should restart the daemon after replacing the binary.
pub fn is_installed() -> Result<bool> {
    Ok(match ServicePlatform::current() {
        ServicePlatform::Systemd => systemd_unit_path()?.exists(),
        ServicePlatform::Launchd => launchd_plist_path()?.exists(),
        ServicePlatform::Windows => {
            if !command_available("schtasks") {
                false
            } else {
                run_capture_allow_failure(&windows_status_command())?
                    .status
                    .success()
            }
        }
    })
}

// --- Linux -----------------------------------------------------------------

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
            rendered.push_str(&systemd_quote(&binary.to_string_lossy()));
            rendered.push_str(" serve");
        } else if line.starts_with("Environment=H4RN_DATA_DIR=") {
            rendered.push_str("Environment=\"H4RN_DATA_DIR=");
            rendered.push_str(&systemd_escape(&data_dir.to_string_lossy()));
            rendered.push('"');
        } else {
            rendered.push_str(line);
        }
        rendered.push('\n');
    }
    rendered
}

/// Quotes and escapes one argument for a systemd unit value.
fn systemd_quote(value: &str) -> String {
    format!("\"{}\"", systemd_escape(value))
}

fn systemd_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' | '\\' => {
                escaped.push('\\');
                escaped.push(ch);
            }
            // systemd expands % specifiers and $ variables even in quoted
            // values; doubling them keeps a literal percent or dollar.
            '%' | '$' => {
                escaped.push(ch);
                escaped.push(ch);
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

fn install_windows(binary: &Path, data_dir: &Path) -> Result<()> {
    ensure_available("powershell")?;
    let script = write_windows_script("install-scheduled-task.ps1", WINDOWS_INSTALL_SCRIPT)?;
    let result = run_checked(&windows_install_command(&script, binary, data_dir));
    let _ = std::fs::remove_file(&script);
    result?;
    println!("Installed and started scheduled task {WINDOWS_TASK_NAME}.");
    println!("Data directory: {}", data_dir.display());
    Ok(())
}

fn uninstall_windows() -> Result<()> {
    ensure_available("schtasks")?;
    if !run_capture_allow_failure(&windows_status_command())?
        .status
        .success()
    {
        println!("Scheduled task {WINDOWS_TASK_NAME} is not installed.");
        return Ok(());
    }
    ensure_available("powershell")?;
    let script = write_windows_script("uninstall-scheduled-task.ps1", WINDOWS_UNINSTALL_SCRIPT)?;
    let result = run_checked(&windows_uninstall_command(&script));
    let _ = std::fs::remove_file(&script);
    result?;
    println!(
        "Stopped and removed scheduled task {WINDOWS_TASK_NAME}. The data directory was left untouched."
    );
    Ok(())
}

fn write_windows_script(name: &str, contents: &str) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join("h4rn-service");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(name);
    std::fs::write(&path, contents).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

/// The command that runs the shipped install script with explicit paths.
pub fn windows_install_command(script: &Path, binary: &Path, data_dir: &Path) -> CommandSpec {
    CommandSpec::new(
        "powershell",
        [
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
            WINDOWS_TASK_NAME.to_owned(),
        ],
    )
}

/// The command that runs the shipped uninstall script.
pub fn windows_uninstall_command(script: &Path) -> CommandSpec {
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
            WINDOWS_TASK_NAME.to_owned(),
        ],
    )
}

/// The Task Scheduler restart commands.
pub fn windows_restart_commands() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("schtasks", ["/End", "/TN", WINDOWS_TASK_NAME]),
        CommandSpec::new("schtasks", ["/Run", "/TN", WINDOWS_TASK_NAME]),
    ]
}

/// The Task Scheduler status query.
pub fn windows_status_command() -> CommandSpec {
    CommandSpec::new(
        "schtasks",
        ["/Query", "/TN", WINDOWS_TASK_NAME, "/FO", "LIST", "/V"],
    )
}

/// Parses English `schtasks /Query /FO LIST /V` output.
pub fn parse_schtasks(output: &str) -> ServiceStatus {
    let mut status = ServiceStatus {
        installed: true,
        ..ServiceStatus::default()
    };
    for line in output.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("Status") {
            status.active = value.trim().eq_ignore_ascii_case("Running");
        }
    }
    status
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

/// Whether a binary path looks like a build or temporary directory.
pub fn is_build_or_temp_path(path: &Path, temp_root: &Path) -> bool {
    if path.starts_with(temp_root) {
        return true;
    }
    let components: Vec<String> = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_ascii_lowercase())
        .collect();
    let mut seen_target = false;
    for component in &components {
        if component == ".cargo" {
            return true;
        }
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

fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
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
        assert!(
            rendered.contains("Environment=\"H4RN_DATA_DIR=/home/a/100%% $$data/\\\"quoted\\\"\"")
        );
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
    fn windows_commands_run_the_shipped_scripts_without_a_shell_string() {
        let script = Path::new(r"C:\Temp\h4rn-service\install-scheduled-task.ps1");
        let install = windows_install_command(
            script,
            Path::new(r"C:\Program Files\h4rn\h4rnd.exe"),
            Path::new(r"C:\Users\a\AppData\Local\h4rn"),
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
                r"C:\Temp\h4rn-service\install-scheduled-task.ps1",
                "-Binary",
                r"C:\Program Files\h4rn\h4rnd.exe",
                "-DataDir",
                r"C:\Users\a\AppData\Local\h4rn",
                "-TaskName",
                "h4rn",
            ]
        );
        let uninstall = windows_uninstall_command(script);
        assert_eq!(uninstall.program, "powershell");
        assert_eq!(
            uninstall.args,
            vec![
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                r"C:\Temp\h4rn-service\install-scheduled-task.ps1",
                "-TaskName",
                "h4rn",
            ]
        );
        assert_eq!(
            windows_restart_commands()[1],
            CommandSpec::new("schtasks", ["/Run", "/TN", "h4rn"])
        );
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
        let status = parse_schtasks("TaskName: \\h4rn\nStatus: Running\n");
        assert!(status.installed && status.active);
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
    }
}
