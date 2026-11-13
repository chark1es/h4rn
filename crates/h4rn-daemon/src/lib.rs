//! The per-host h4rn daemon.
//!
//! The daemon owns every behaviour: harness sessions, the local event log,
//! MCP hub, terminal server, attachments and the WebSocket endpoint that all
//! clients speak (ADR-001). Phase 1a ships the crate boundary and process
//! skeleton; later phases wire the core, harness and protocol crates together.

/// Daemon application version.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Starts the daemon.
///
/// Phase 1a only initializes logging and reports the version. The real daemon
/// boot sequence (config, SQLite cache, listeners, supervisors) arrives in the
/// next phase.
pub fn run() -> anyhow::Result<()> {
    tracing::info!(
        version = version(),
        "h4rn daemon skeleton started; no listeners are configured yet"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_not_empty() {
        assert!(!super::version().is_empty());
    }
}
