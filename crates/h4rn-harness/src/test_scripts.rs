//! Test-only helpers shared by the harness fixtures.
//!
//! The adapter tests stand a shell script in for a vendor CLI or ACP agent.
//! Writing one and spawning it immediately is racy on a busy machine: when
//! another test thread forks while the write handle is open, the child inherits
//! it and `exec` fails with `ETXTBSY` ("Text file busy", os error 26). Several
//! adapters report that failure as "not installed" rather than an error, so the
//! test can pass or fail for a reason the code cannot fix. These helpers write
//! the script through a rename (the path is never open for writing here) and
//! retry the spawn while a stale handle from elsewhere still makes it busy.

use std::path::Path;

/// Attempts before a path that stays `Text file busy` is a real failure.
const SPAWN_ATTEMPTS: u32 = 50;

/// `ETXTBSY`, the error a spawn gets while the file is open for writing.
#[cfg(unix)]
const ETXTBSY: i32 = 26;

/// Writes `body` to `path` as an executable script a test can spawn.
///
/// A harmless probe is installed and spawned first, retrying `ETXTBSY`; the
/// real body is then renamed into place. The probe runs instead of the caller's
/// script so the test keeps ownership of that script's side effects, and the
/// rename means the path is never itself held open for writing here.
#[cfg(unix)]
pub(crate) fn write_script(path: &Path, body: &str) {
    install(path, "#!/bin/sh\nexit 0\n");
    spawn_retrying(path);
    install(path, body);
}

/// Installs `body` at `path` with the executable bit set, by rename.
#[cfg(unix)]
fn install(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;

    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("script");
    let staged = path.with_file_name(format!(".{name}.staged"));
    std::fs::write(&staged, body).expect("write the test script");
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
        .expect("mark the test script executable");
    std::fs::rename(&staged, path).expect("install the test script");
}

/// Runs `path` once, retrying while it is `Text file busy`.
///
/// The probe is killed as soon as it is running: it is only a proof that `exec`
/// works for this path right now.
#[cfg(unix)]
fn spawn_retrying(path: &Path) {
    for attempt in 0..SPAWN_ATTEMPTS {
        match std::process::Command::new(path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            Err(error) if error.raw_os_error() == Some(ETXTBSY) => {
                std::thread::sleep(std::time::Duration::from_millis(10 * (attempt as u64 + 1)));
            }
            Err(error) => panic!("could not spawn {}: {error}", path.display()),
        }
    }
    panic!(
        "{} stayed `Text file busy` after {SPAWN_ATTEMPTS} attempts",
        path.display()
    );
}

/// Writing executable scripts is a unix test concern; other platforms either do
/// not run these tests or write none.
#[cfg(not(unix))]
#[allow(dead_code)]
pub(crate) fn write_script(_path: &Path, _body: &str) {}
