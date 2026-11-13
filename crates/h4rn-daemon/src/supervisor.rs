//! Process supervision for harness children (ADR-021).
//!
//! Adapters spawn harness processes into their own process group and register
//! that group here. On daemon shutdown the supervisor sends `SIGTERM` to every
//! registered group, waits briefly, then sends `SIGKILL`; on Windows it uses
//! `taskkill /T /F`. The daemon also installs a graceful `SIGTERM`/`SIGINT`
//! handler so `systemctl stop` drains sessions rather than orphaning them.

use std::collections::BTreeSet;

/// Registry of harness process groups owned by this daemon.
#[derive(Debug, Default)]
pub struct ProcessSupervisor {
    groups: std::sync::Mutex<BTreeSet<u32>>,
}

impl ProcessSupervisor {
    /// Creates an empty supervisor.
    pub fn new() -> Self {
        Self::default()
    }

    /// Tracks a harness process group id.
    ///
    /// The adapter must have called `setsid`/`setpgid` so the whole tree can be
    /// signalled at once; tracking a bare child pid risks leaving grandchildren
    /// behind.
    pub fn track_process_group(&self, pgid: u32) {
        if pgid == 0 {
            return;
        }
        let mut groups = self.lock();
        groups.insert(pgid);
    }

    /// Stops tracking a group that exited cleanly.
    pub fn untrack_process_group(&self, pgid: u32) {
        let mut groups = self.lock();
        groups.remove(&pgid);
    }

    /// Groups currently tracked.
    pub fn tracked(&self) -> Vec<u32> {
        self.lock().iter().copied().collect()
    }

    /// Terminates every tracked group: `SIGTERM`, a short grace, then `SIGKILL`.
    pub async fn shutdown(&self) {
        let groups = self.tracked();
        if groups.is_empty() {
            return;
        }
        for group in &groups {
            signal_group(*group, Signal::Term);
        }
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        for group in &groups {
            signal_group(*group, Signal::Kill);
        }
        let mut pending = self.lock();
        pending.clear();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeSet<u32>> {
        self.groups
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[derive(Clone, Copy)]
enum Signal {
    Term,
    Kill,
}

fn signal_group(pgid: u32, signal: Signal) {
    #[cfg(unix)]
    {
        let flag = match signal {
            Signal::Term => "-TERM",
            Signal::Kill => "-KILL",
        };
        // `kill -TERM -<pgid>` signals the whole group; `sh` handles the
        // negative argument without a `--` portability question.
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("kill {flag} -- -{pgid}"))
            .status();
        if let Err(error) = status {
            tracing::warn!(pgid, %error, "could not signal harness process group");
        }
    }

    #[cfg(windows)]
    {
        let _ = signal;
        let status = std::process::Command::new("taskkill")
            .args(["/PID", &pgid.to_string(), "/T", "/F"])
            .status();
        if let Err(error) = status {
            tracing::warn!(pgid, %error, "could not terminate harness process tree");
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = (pgid, signal);
    }
}

/// Waits for `SIGTERM` or `SIGINT` (Ctrl-C) on Unix, Ctrl-C elsewhere.
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(signal) => signal,
            Err(error) => {
                tracing::warn!(%error, "could not listen for SIGTERM; Ctrl-C still works");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = term.recv() => tracing::info!("received SIGTERM, shutting down"),
            _ = tokio::signal::ctrl_c() => tracing::info!("received SIGINT, shutting down"),
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("received Ctrl-C, shutting down");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracking_is_idempotent_and_ignores_zero() {
        let supervisor = ProcessSupervisor::new();
        supervisor.track_process_group(0);
        supervisor.track_process_group(42);
        supervisor.track_process_group(42);
        assert_eq!(supervisor.tracked(), vec![42]);
        supervisor.untrack_process_group(42);
        assert!(supervisor.tracked().is_empty());
    }

    #[tokio::test]
    async fn shutdown_with_no_groups_is_a_no_op() {
        ProcessSupervisor::new().shutdown().await;
    }
}
