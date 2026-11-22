//! Reading a skill from a local folder, a pinned git commit or a harness
//! directory.
//!
//! Git is invoked as a subprocess instead of pulling a full git library into
//! the daemon. The commit is resolved to a full id and recorded with the
//! version, so a sync or reinstall cannot silently follow a moving branch.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use h4rn_protocol::SkillSource;

use crate::error::SkillError;
use crate::model::{SkillBundle, read_bundle};

/// How long a git source may take before the clone is abandoned.
///
/// A black-holed network is indistinguishable from a slow one until a deadline
/// fires, so skill import never waits on git longer than this. The clone runs
/// while the daemon's serial request loop waits, so the budget is deliberately
/// short: an unreachable remote should fail promptly, not hold the connection.
pub const GIT_LOAD_TIMEOUT: Duration = Duration::from_secs(20);

/// Git options that turn a stalled transfer into a fast failure.
///
/// `http.lowSpeedTime=15` aborts when the transfer stays below
/// `http.lowSpeedLimit=1` byte/s for 15 seconds, which covers a peer that
/// accepts the connection and then goes silent.
const GIT_FAST_FAIL_ARGS: &[&str] = &["-c", "http.lowSpeedLimit=1", "-c", "http.lowSpeedTime=15"];

/// A program that answers any credential prompt with nothing.
///
/// `git` invokes the askpass helper through the shell; `true` exists on every
/// Unix, and `cmd` exists on every Windows host.
#[cfg(windows)]
const NOOP_ASKPASS: &str = "cmd";
#[cfg(not(windows))]
const NOOP_ASKPASS: &str = "true";

/// A bundle plus the canonical source that should be recorded.
#[derive(Clone, Debug, PartialEq)]
pub struct LoadedSource {
    /// Parsed bundle.
    pub bundle: SkillBundle,
    /// Source with parent paths and commits resolved where possible.
    pub source: SkillSource,
}

/// Loads one source into a bundle.
pub fn load_source(source: &SkillSource) -> Result<LoadedSource, SkillError> {
    match source {
        SkillSource::LocalFolder { path } => {
            let root = PathBuf::from(path);
            let bundle = read_bundle(&root)?;
            Ok(LoadedSource {
                bundle,
                source: SkillSource::LocalFolder {
                    path: absolute(&root),
                },
            })
        }
        SkillSource::HarnessDir { harness, path } => {
            let root = PathBuf::from(path);
            let bundle = read_bundle(&root)?;
            Ok(LoadedSource {
                bundle,
                source: SkillSource::HarnessDir {
                    harness: *harness,
                    path: absolute(&root),
                },
            })
        }
        SkillSource::Git {
            url,
            commit,
            subdir,
        } => load_git(url, commit, subdir.as_deref()),
        SkillSource::Connect { .. } => Err(SkillError::InvalidSource(
            "a connect version is applied by sync, not read as a local source".to_owned(),
        )),
    }
}

/// Loads one source into a bundle without blocking an async worker on git.
///
/// Local folders are read exactly as [`load_source`] reads them; a git source is
/// cloned and checked out off the async worker and abandoned after
/// [`GIT_LOAD_TIMEOUT`], so an unreachable remote cannot wedge the daemon.
pub async fn load_source_async(source: &SkillSource) -> Result<LoadedSource, SkillError> {
    match source {
        SkillSource::Git {
            url,
            commit,
            subdir,
        } => load_git_with_timeout(url, commit, subdir.as_deref(), GIT_LOAD_TIMEOUT).await,
        other => load_source(other),
    }
}

fn load_git(url: &str, commit: &str, subdir: Option<&str>) -> Result<LoadedSource, SkillError> {
    if commit.trim().is_empty() {
        return Err(SkillError::InvalidSource(
            "a git source must be pinned to a commit".to_owned(),
        ));
    }
    let directory = std::env::temp_dir().join(format!("h4rn-skill-git-{}", uuid::Uuid::now_v7()));
    let result = (|| {
        run_git(
            None,
            &[
                "clone",
                "--quiet",
                "--no-checkout",
                url,
                &directory.display().to_string(),
            ],
        )?;
        run_git(Some(&directory), &["checkout", "--quiet", commit])?;
        let head = run_git(Some(&directory), &["rev-parse", "HEAD"])?;
        let head = head.trim().to_owned();
        if !head.starts_with(commit) && !commit.starts_with(&head) {
            return Err(SkillError::Git(format!(
                "checked out {head}, expected {commit}"
            )));
        }
        let root = match subdir {
            Some(subdir) => {
                let subdir = crate::model::normalize_rel_path(subdir)?;
                directory.join(subdir.replace('/', std::path::MAIN_SEPARATOR_STR))
            }
            None => directory.clone(),
        };
        let bundle = read_bundle(&root)?;
        Ok(LoadedSource {
            bundle,
            source: SkillSource::Git {
                url: url.to_owned(),
                commit: head,
                subdir: subdir.map(str::to_owned),
            },
        })
    })();
    let _ = std::fs::remove_dir_all(&directory);
    result
}

fn run_git(cwd: Option<&Path>, args: &[&str]) -> Result<String, SkillError> {
    let mut command = git_command();
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    command.args(args);
    let output = command
        .output()
        .map_err(|error| SkillError::Git(format!("could not run git: {error}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SkillError::Git(format!(
            "git {} failed: {}",
            args.first().copied().unwrap_or(""),
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Builds a git command that never waits on a person or a stalled transfer.
fn git_command() -> Command {
    let mut command = Command::new("git");
    command
        .args(GIT_FAST_FAIL_ARGS)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", NOOP_ASKPASS)
        .env("SSH_ASKPASS", NOOP_ASKPASS);
    if std::env::var_os("GIT_SSH_COMMAND").is_none() {
        command.env(
            "GIT_SSH_COMMAND",
            "ssh -o BatchMode=yes -o ConnectTimeout=10",
        );
    }
    command
}

/// Puts the child in its own process group so a timeout can kill its helpers.
fn own_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
}

/// Kills the process group led by `pid`, including remote helpers.
///
/// A timed-out `git clone` leaves `git-remote-http` (or `ssh`) behind, and the
/// helper is what keeps the connection and the temporary directory alive.
fn kill_process_tree(pid: u32) {
    #[cfg(unix)]
    {
        if pid == 0 {
            return;
        }
        // SAFETY: killpg is async-signal-safe and takes an integer group id.
        // The child leads the group because it was spawned with
        // `process_group(0)`.
        if unsafe { libc::killpg(pid as libc::pid_t, libc::SIGKILL) } != 0 {
            // SAFETY: as above; the group may already be gone.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }

    #[cfg(windows)]
    {
        if pid == 0 {
            return;
        }
        let _ = Command::new("taskkill")
            .arg("/PID")
            .arg(pid.to_string())
            .args(["/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
    }
}

async fn load_git_with_timeout(
    url: &str,
    commit: &str,
    subdir: Option<&str>,
    limit: Duration,
) -> Result<LoadedSource, SkillError> {
    if commit.trim().is_empty() {
        return Err(SkillError::InvalidSource(
            "a git source must be pinned to a commit".to_owned(),
        ));
    }
    let directory = std::env::temp_dir().join(format!("h4rn-skill-git-{}", uuid::Uuid::now_v7()));
    let deadline = tokio::time::Instant::now() + limit;
    let result = clone_and_read(url, commit, subdir, &directory, deadline, limit).await;
    if let Err(error) = tokio::fs::remove_dir_all(&directory).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::debug!(%error, path = %directory.display(), "could not remove a temporary skill clone");
    }
    result
}

async fn clone_and_read(
    url: &str,
    commit: &str,
    subdir: Option<&str>,
    directory: &Path,
    deadline: tokio::time::Instant,
    limit: Duration,
) -> Result<LoadedSource, SkillError> {
    let destination = directory.display().to_string();
    run_git_async(
        None,
        &["clone", "--quiet", "--no-checkout", url, &destination],
        deadline,
        limit,
    )
    .await?;
    run_git_async(
        Some(directory),
        &["checkout", "--quiet", commit],
        deadline,
        limit,
    )
    .await?;
    let head = run_git_async(Some(directory), &["rev-parse", "HEAD"], deadline, limit).await?;
    let head = head.trim().to_owned();
    if !head.starts_with(commit) && !commit.starts_with(&head) {
        return Err(SkillError::Git(format!(
            "checked out {head}, expected {commit}"
        )));
    }
    let root = match subdir {
        Some(subdir) => {
            let subdir = crate::model::normalize_rel_path(subdir)?;
            directory.join(subdir.replace('/', std::path::MAIN_SEPARATOR_STR))
        }
        None => directory.to_path_buf(),
    };
    let bundle = read_bundle(&root)?;
    Ok(LoadedSource {
        bundle,
        source: SkillSource::Git {
            url: url.to_owned(),
            commit: head,
            subdir: subdir.map(str::to_owned),
        },
    })
}

async fn run_git_async(
    cwd: Option<&Path>,
    args: &[&str],
    deadline: tokio::time::Instant,
    limit: Duration,
) -> Result<String, SkillError> {
    let mut command = git_command();
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    command.args(args);
    own_process_group(&mut command);

    let child = tokio::process::Command::from(command)
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| SkillError::Git(format!("could not run git: {error}")))?;
    let pid = child.id().unwrap_or(0);
    let mut waiting = Box::pin(child.wait_with_output());

    let output = tokio::select! {
        output = &mut waiting => output
            .map_err(|error| SkillError::Git(format!("could not run git: {error}")))?,
        _ = tokio::time::sleep_until(deadline) => {
            // Killing only the direct child leaves `git-remote-http`/`ssh`
            // running with the socket and the clone directory open, so take
            // the whole group down and wait for the child to be reaped before
            // the caller removes the temporary directory.
            kill_process_tree(pid);
            let _ = tokio::time::timeout(Duration::from_secs(5), &mut waiting).await;
            return Err(SkillError::Git(format!(
                "git {} did not finish within {}s; the clone could not reach the remote",
                args.first().copied().unwrap_or(""),
                limit.as_secs()
            )));
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SkillError::Git(format!(
            "git {} failed: {}",
            args.first().copied().unwrap_or(""),
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn absolute(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

/// Reads a directory the user selected for import without normalising it away.
pub fn read_import(path: &Path) -> Result<SkillBundle, SkillError> {
    read_bundle(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SKILL_FILE;
    use tokio::io::AsyncReadExt;

    fn make_repo() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run_git(None, &["init", "--quiet", &root.display().to_string()]).unwrap();
        run_git(Some(root), &["config", "user.email", "test@example.test"]).unwrap();
        run_git(Some(root), &["config", "user.name", "Test"]).unwrap();
        std::fs::write(
            root.join(SKILL_FILE),
            "---\nname: demo\ndescription: Demo\n---\n\n# Demo\n",
        )
        .unwrap();
        run_git(Some(root), &["add", "."]).unwrap();
        run_git(Some(root), &["commit", "--quiet", "-m", "one"]).unwrap();
        let head = run_git(Some(root), &["rev-parse", "HEAD"]).unwrap();
        (dir, head.trim().to_owned())
    }

    #[test]
    fn reads_a_local_folder_and_records_its_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("demo");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join(SKILL_FILE),
            "---\nname: demo\ndescription: Demo\n---\n",
        )
        .unwrap();
        let loaded = load_source(&SkillSource::LocalFolder {
            path: root.display().to_string(),
        })
        .unwrap();
        assert_eq!(loaded.bundle.manifest.name, "demo");
        assert!(matches!(loaded.source, SkillSource::LocalFolder { .. }));
    }

    #[test]
    fn pins_a_git_source_to_the_resolved_commit() {
        let (repo, head) = make_repo();
        let loaded = load_source(&SkillSource::Git {
            url: repo.path().display().to_string(),
            commit: head[..12].to_owned(),
            subdir: None,
        })
        .unwrap();
        match loaded.source {
            SkillSource::Git { commit, .. } => assert_eq!(commit, head),
            other => panic!("expected a git source, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn async_loads_keep_local_folders_working_offline() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("demo");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join(SKILL_FILE),
            "---\nname: demo\ndescription: Demo\n---\n",
        )
        .unwrap();
        let loaded = load_source_async(&SkillSource::LocalFolder {
            path: root.display().to_string(),
        })
        .await
        .unwrap();
        assert_eq!(loaded.bundle.manifest.name, "demo");
        assert!(matches!(loaded.source, SkillSource::LocalFolder { .. }));
    }

    #[tokio::test]
    async fn a_black_holed_git_remote_times_out() {
        // Accept the connection, read the request, and never answer it, the way
        // a firewalled or wedged remote behaves.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut stalled = Vec::new();
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request).await;
                stalled.push(stream);
            }
        });

        let url = format!("http://127.0.0.1:{port}/demo.git");
        let started = std::time::Instant::now();
        let error = load_git_with_timeout(
            &url,
            "0123456789abcdef",
            None,
            std::time::Duration::from_millis(500),
        )
        .await
        .unwrap_err();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "the clone waited {:?}",
            started.elapsed()
        );
        assert!(matches!(&error, SkillError::Git(_)));
        assert!(
            error.to_string().contains("could not reach"),
            "unexpected message: {error}"
        );
    }

    /// Every process whose command line mentions `needle`, for leak checks.
    #[cfg(unix)]
    fn processes_mentioning(needle: &str) -> Vec<String> {
        let mut found = Vec::new();
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return found;
        };
        for entry in entries.flatten() {
            if !entry
                .file_name()
                .to_string_lossy()
                .chars()
                .all(|character| character.is_ascii_digit())
            {
                continue;
            }
            let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
                continue;
            };
            let command = String::from_utf8_lossy(&raw).replace('\0', " ");
            if command.contains(needle) {
                found.push(command);
            }
        }
        found
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_timed_out_clone_leaves_no_git_helper_running() {
        // A remote helper (`git-remote-http`) outlives a `kill_on_drop` child
        // unless the whole process group is killed.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut stalled = Vec::new();
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request).await;
                stalled.push(stream);
            }
        });

        let url = format!("http://127.0.0.1:{port}/demo.git");
        let error = load_git_with_timeout(
            &url,
            "0123456789abcdef",
            None,
            std::time::Duration::from_millis(500),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("could not reach"), "{error}");

        // Signal delivery and process teardown are asynchronous; poll briefly.
        let needle = format!(":{port}");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let leftovers = processes_mentioning(&needle);
            if leftovers.is_empty() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "git helpers survived the timeout: {leftovers:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[test]
    fn rejects_an_unpinned_git_source() {
        let error = load_source(&SkillSource::Git {
            url: "https://example.test/s.git".into(),
            commit: " ".into(),
            subdir: None,
        })
        .unwrap_err();
        assert!(matches!(error, SkillError::InvalidSource(_)));
    }
}
