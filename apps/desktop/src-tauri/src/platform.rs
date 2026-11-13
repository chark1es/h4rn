//! Windows and WSL probing, kept out of the rest of the shell.
//!
//! The shell only needs three OS facts: what this machine is called, which WSL
//! distributions exist (ADR-019), and how to reach a daemon inside one of
//! them. Everything product-shaped stays in the daemon.

#[cfg(windows)]
use std::process::Output;

/// Hidden console window flag for child processes on Windows.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The machine name used as a daemon endpoint label.
pub fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "This machine".to_owned())
}

/// Lists installed WSL distributions on Windows.
///
/// `wsl.exe -l -q` writes UTF-16LE on most builds and UTF-8 on a few, so the
/// decoder sniffs NUL bytes. Docker's internal distributions are filtered out:
/// they are not user hosts and do not run `h4rnd`.
#[cfg(windows)]
pub fn wsl_distros() -> Vec<String> {
    let Some(output) = run_hidden("wsl.exe", &["-l", "-q"]) else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }

    decode_wsl_output(&output.stdout)
        .lines()
        .map(|line| {
            line.trim_matches(|character: char| character == '\0' || character.is_whitespace())
        })
        .filter(|line| !line.is_empty())
        .filter(|line| !line.to_ascii_lowercase().starts_with("docker-desktop"))
        .map(str::to_owned)
        .collect()
}

/// No WSL outside Windows.
#[cfg(not(windows))]
pub fn wsl_distros() -> Vec<String> {
    Vec::new()
}

/// Runs `wsl.exe -d <distro> -- <args>` and returns trimmed stdout.
///
/// Returns `None` when WSL is unavailable, the command is missing in the
/// distribution, or it exits unsuccessfully.
#[cfg(windows)]
pub fn wsl_output(distro: &str, args: &[&str]) -> Option<String> {
    let mut wsl_args = vec!["-d", distro, "--"];
    wsl_args.extend_from_slice(args);
    let output = run_hidden("wsl.exe", &wsl_args)?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// Starts `h4rnd serve` inside a WSL distribution.
///
/// WSL2's localhost forwarding makes the loopback listener reachable from
/// Windows. The child is deliberately not tracked for shutdown: it belongs to
/// the distribution's host, not to this shell, and outlives the window (the
/// daemon is what keeps threads alive across app restarts).
#[cfg(windows)]
pub fn start_wsl_daemon(distro: &str, port: u16) -> bool {
    use std::os::windows::process::CommandExt;

    let bind = format!("127.0.0.1:{port}");
    std::process::Command::new("wsl.exe")
        .args(["-d", distro, "--", "h4rnd", "serve", "--bind", &bind])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .is_ok()
}

#[cfg(windows)]
fn run_hidden(program: &str, args: &[&str]) -> Option<Output> {
    use std::os::windows::process::CommandExt;

    std::process::Command::new(program)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_output_is_decoded() {
        let text = "Ubuntu\r\nDebian\r\n";
        let bytes: Vec<u8> = text
            .encode_utf16()
            .flat_map(|unit| unit.to_le_bytes())
            .collect();
        assert_eq!(decode_wsl_output(&bytes), text);
        assert_eq!(decode_wsl_output(b"Ubuntu\n"), "Ubuntu\n");
    }

    #[test]
    fn hostname_is_never_empty() {
        assert!(!hostname().is_empty());
    }
}
