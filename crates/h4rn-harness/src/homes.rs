//! The state directory each harness reads, resolved the way the harness does.
//!
//! Claude Code and Codex both honour an environment override
//! (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`): a `~` is expanded, a relative value is
//! resolved against the process's working directory, and an empty value means
//! the default. The daemon reads the same directories the launched harness
//! writes, so both sides ask this one function instead of each deciding on its
//! own.

use std::path::{Path, PathBuf};

use h4rn_protocol::HarnessKind;

/// Why a harness home could not be resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HomeError {
    /// The environment variable that was rejected.
    pub variable: &'static str,
    /// Plain-language reason, safe to show to the person.
    pub message: String,
}

impl std::fmt::Display for HomeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.variable, self.message)
    }
}

impl std::error::Error for HomeError {}

/// The environment variable that overrides `kind`'s state directory.
pub fn home_variable(kind: HarnessKind) -> Option<&'static str> {
    match kind {
        HarnessKind::ClaudeCode => Some("CLAUDE_CONFIG_DIR"),
        HarnessKind::Codex => Some("CODEX_HOME"),
        _ => None,
    }
}

/// The directory a harness keeps its conversations in.
///
/// `home` is the user's home directory, used when no override is set. A
/// harness this daemon does not read transcripts from has no state home.
pub fn harness_home(kind: HarnessKind, home: &Path) -> Result<Option<PathBuf>, HomeError> {
    let Some(variable) = home_variable(kind) else {
        return Ok(None);
    };
    let cwd = std::env::current_dir().ok();
    Ok(Some(resolve_home(
        kind,
        home,
        std::env::var_os(variable).as_deref(),
        cwd.as_deref(),
    )?))
}

/// [`harness_home`] with the override and working directory supplied.
///
/// Tests use this so they never mutate the process environment.
pub fn resolve_home(
    kind: HarnessKind,
    home: &Path,
    override_value: Option<&std::ffi::OsStr>,
    cwd: Option<&Path>,
) -> Result<PathBuf, HomeError> {
    let variable = home_variable(kind).expect("a harness with a state home");
    let default = home.join(match kind {
        HarnessKind::Codex => ".codex",
        _ => ".claude",
    });
    let Some(value) = override_value else {
        return Ok(default);
    };
    let value = value.to_string_lossy();
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(default);
    }
    let expanded = expand_home(trimmed, home);
    if expanded.is_absolute() {
        return Ok(normalize(&expanded));
    }
    let Some(cwd) = cwd else {
        return Err(HomeError {
            variable,
            message: "a relative path cannot be resolved without a working directory".into(),
        });
    };
    Ok(normalize(&cwd.join(expanded)))
}

/// Expands a leading `~` or `~/` against `home`.
fn expand_home(value: &str, home: &Path) -> PathBuf {
    if value == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    {
        return home.join(rest);
    }
    PathBuf::from(value)
}

/// Resolves `.` and `..` lexically so a path compares the way it reads.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    out.push(component);
                }
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn an_unset_override_reads_the_default_directory() {
        let home = Path::new("/home/user");
        assert_eq!(
            resolve_home(HarnessKind::Codex, home, None, None).expect("home"),
            home.join(".codex")
        );
        assert_eq!(
            resolve_home(HarnessKind::ClaudeCode, home, Some(OsStr::new("  ")), None)
                .expect("home"),
            home.join(".claude")
        );
    }

    #[test]
    fn a_tilde_and_a_relative_override_resolve_like_the_harness() {
        let home = Path::new("/home/user");
        let cwd = Path::new("/work/app");
        assert_eq!(
            resolve_home(
                HarnessKind::ClaudeCode,
                home,
                Some(OsStr::new("~/claude-alt")),
                Some(cwd)
            )
            .expect("home"),
            home.join("claude-alt")
        );
        assert_eq!(
            resolve_home(
                HarnessKind::ClaudeCode,
                home,
                Some(OsStr::new("relative/claude")),
                Some(cwd)
            )
            .expect("home"),
            cwd.join("relative/claude")
        );
        assert_eq!(
            resolve_home(
                HarnessKind::Codex,
                home,
                Some(OsStr::new("/srv/codex/../codex-a")),
                Some(cwd)
            )
            .expect("home"),
            PathBuf::from("/srv/codex-a")
        );
    }

    #[test]
    fn a_relative_override_without_a_working_directory_is_reported() {
        let error = resolve_home(
            HarnessKind::Codex,
            Path::new("/home/user"),
            Some(OsStr::new("relative")),
            None,
        )
        .expect_err("invalid override");
        assert_eq!(error.variable, "CODEX_HOME");
        assert!(error.message.contains("relative"));
    }

    #[test]
    fn a_harness_without_a_state_home_reports_none() {
        assert_eq!(
            harness_home(HarnessKind::Opencode, Path::new("/home/user")),
            Ok(None)
        );
    }
}
