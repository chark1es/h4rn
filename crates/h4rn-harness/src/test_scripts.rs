//! Test-only helpers shared by the harness fixtures.
//!
//! The adapter tests stand a shell script in for a vendor CLI or ACP agent.
//! Writing one and spawning it immediately is racy on a busy machine: when
//! another test thread forks while the write handle is open, the child inherits
//! it and `exec` fails with `ETXTBSY` ("Text file busy", os error 26). Several
//! adapters report that failure as "not installed" rather than an error, so the
//! test can pass or fail for a reason the code cannot fix. The body is therefore
//! written by a short-lived child process, never by the test process itself, and
//! installed by rename. `ETXTBSY` needs an open write descriptor *in the process
//! that forks*, so a handle held by this helper's child cannot make the caller's
//! own spawns fail, no matter how many test threads fork meanwhile.

use std::path::Path;

/// `ETXTBSY`, the error a spawn gets while the file is open for writing.
#[cfg(unix)]
const ETXTBSY: i32 = 26;

/// Writes `body` to `path` as an executable script a test can spawn.
///
/// The test process never opens the script for writing: a short-lived `/bin/sh`
/// does, then exits, and the finished file is renamed into place. A fork in
/// another test thread can then inherit nothing that makes `path` busy.
#[cfg(unix)]
pub(crate) fn write_script(path: &Path, body: &str) {
    install(path, body);
}

/// Installs `body` at `path` with the executable bit set.
#[cfg(unix)]
fn install(path: &Path, body: &str) {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("script");
    let staged = path.with_file_name(format!(".{name}.staged"));
    // The write descriptor lives only in this child. The test process holds the
    // pipe to its stdin, which a fork may inherit harmlessly, never a handle on
    // the script itself.
    let mut writer = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("cat > \"$1\"")
        .arg("h4rn-test-script")
        .arg(&staged)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("start the test script writer");
    {
        let mut stdin = writer.stdin.take().expect("the writer's stdin");
        stdin
            .write_all(body.as_bytes())
            .expect("write the test script body");
    }
    let status = writer.wait().expect("the test script writer exiting");
    assert!(status.success(), "the test script writer failed: {status}");
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
        .expect("mark the test script executable");
    std::fs::rename(&staged, path).expect("install the test script");
}

/// Writing executable scripts is a unix test concern; other platforms either do
/// not run these tests or write none.
#[cfg(not(unix))]
#[allow(dead_code)]
pub(crate) fn write_script(_path: &Path, _body: &str) {}

/// Measures the fixture under the churn that made it flaky.
///
/// DeepSeek's stress run wrote a fresh script and spawned it while other
/// threads kept forking `/bin/sh`. That is the exact sequence repeated here: at
/// least 20,000 install-and-spawn rounds with a forker running alongside. Zero
/// `ETXTBSY` is the result the fixture guarantees; the same sequence with the
/// old in-process write failed 254 of those 20,000 rounds (264 in a second
/// run). Run with
/// `cargo test -p h4rn-harness --lib test_scripts -- --ignored`.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Rounds in the stress run.
    const RUNS: usize = 20_000;
    /// Distinct script paths, so each install replaces one the fixture uses
    /// again (the shape a rotating fixture has).
    const PATHS: usize = 8;

    #[test]
    #[ignore = "20,000 install-and-spawn rounds; run with --ignored to measure it"]
    fn a_script_spawned_under_fork_churn_is_never_text_file_busy() {
        let dir = std::env::temp_dir().join(format!(
            "h4rn-script-stress-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("stress dir");
        let stop = Arc::new(AtomicBool::new(false));
        let churn = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let _ = Command::new("/bin/sh")
                        .arg("-c")
                        .arg("exit 0")
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                }
            })
        };

        let mut busy = 0usize;
        for round in 0..RUNS {
            let path = dir.join(format!("script-{}.sh", round % PATHS));
            write_script(&path, "#!/bin/sh\nexit 0\n");
            match Command::new(&path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
            {
                Ok(_) => {}
                Err(error) if error.raw_os_error() == Some(ETXTBSY) => busy += 1,
                Err(error) => panic!("round {round}: could not spawn the script: {error}"),
            }
        }
        stop.store(true, Ordering::SeqCst);
        churn.join().expect("the forker");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            busy, 0,
            "{busy} of {RUNS} script spawns failed with `Text file busy`"
        );
        println!("{RUNS} rounds, {busy} ETXTBSY");
    }
}
