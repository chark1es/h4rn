//! Process-group plumbing for the Claude Code adapter (ADR-021).
//!
//! Every agent process is spawned into its own process group so `interrupt`
//! and `shutdown` reach the tools it started, not just the CLI. On Unix the
//! group is created with `process_group(0)` and signalled with `killpg`. On
//! Windows the child gets `CREATE_NEW_PROCESS_GROUP` and is terminated with
//! `taskkill /T`, which walks the descendant tree; a full job object is the
//! next step when the daemon owns the supervisor.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// The signal to deliver to an agent process group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupSignal {
    /// Ask the running turn to stop (Unix `SIGINT`).
    Interrupt,
    /// Ask the process to exit cleanly (Unix `SIGTERM`).
    Terminate,
    /// Force the process tree down (Unix `SIGKILL`).
    Kill,
}

/// Sends a signal to the process group led by `pid`.
///
/// Failures are best effort: the process may already be gone, and shutdown
/// must never fail because a signal raced the exit.
pub fn signal_group(pid: u32, signal: GroupSignal) {
    #[cfg(unix)]
    {
        let signal = match signal {
            GroupSignal::Interrupt => libc::SIGINT,
            GroupSignal::Terminate => libc::SIGTERM,
            GroupSignal::Kill => libc::SIGKILL,
        };
        if pid == 0 {
            return;
        }
        // SAFETY: killpg is async-signal-safe and takes an integer group id.
        // A stale group id can only signal processes that reused it, which is
        // the same risk every supervisor takes; the child is reaped first.
        let result = unsafe { libc::killpg(pid as libc::pid_t, signal) };
        if result != 0 {
            // Fall back to the pid itself if the group is already gone.
            // SAFETY: as above.
            unsafe {
                libc::kill(pid as libc::pid_t, signal);
            }
        }
    }

    #[cfg(windows)]
    {
        let mut command = std::process::Command::new("taskkill");
        command.args(["/PID", &pid.to_string(), "/T"]);
        if matches!(signal, GroupSignal::Kill) {
            command.arg("/F");
        }
        let _ = command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = (pid, signal);
    }
}

const STDERR_RING_BYTES: usize = 8 * 1024;

/// A bounded ring of stderr lines for error reporting.
///
/// Claude Code occasionally writes diagnostics to stderr even in stream-json
/// mode. Keeping the tail lets the daemon explain a crash without buffering
/// an unbounded log in memory (ADR-023).
#[derive(Clone, Debug, Default)]
pub struct StderrRing {
    inner: Arc<Mutex<RingInner>>,
    bytes: Arc<AtomicUsize>,
}

#[derive(Debug, Default)]
struct RingInner {
    lines: VecDeque<String>,
}

impl StderrRing {
    /// Creates an empty ring.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends one line, dropping the oldest lines to stay under the cap.
    pub fn push(&self, line: &str) {
        let line = line.trim_end();
        if line.is_empty() {
            return;
        }
        let line = if line.len() > STDERR_RING_BYTES {
            let mut start = line.len() - STDERR_RING_BYTES;
            while start < line.len() && !line.is_char_boundary(start) {
                start += 1;
            }
            line[start..].to_string()
        } else {
            line.to_string()
        };
        let mut inner = lock(&self.inner);
        self.bytes.fetch_add(line.len() + 1, Ordering::Relaxed);
        inner.lines.push_back(line);
        while self.bytes.load(Ordering::Relaxed) > STDERR_RING_BYTES {
            let Some(front) = inner.lines.pop_front() else {
                self.bytes.store(0, Ordering::Relaxed);
                break;
            };
            self.bytes.fetch_sub(front.len() + 1, Ordering::Relaxed);
        }
    }

    /// Returns the buffered tail, newest last.
    pub fn snapshot(&self) -> String {
        lock(&self.inner)
            .lines
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stderr_ring_stays_bounded() {
        let ring = StderrRing::new();
        for index in 0..2_000 {
            ring.push(&format!("line {index} with some padding to reach the cap"));
        }
        let snapshot = ring.snapshot();
        assert!(snapshot.len() <= STDERR_RING_BYTES + 64);
        assert!(snapshot.ends_with("line 1999 with some padding to reach the cap"));
    }
}
