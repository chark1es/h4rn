//! Terminal (PTY) wire types (ADR-007, T3 Code §4).
//!
//! A terminal is a daemon-owned PTY with a headless VT grid. JSON frames open,
//! attach, resize, acknowledge and close a terminal; PTY output and input travel
//! in binary frames using the header in [`crate::binary`].
//!
//! Sequence rules:
//! - every output chunk from one terminal carries a monotonically increasing
//!   `seq`, starting at 1;
//! - `Attach` returns a snapshot plus `start_seq`, the last sequence the snapshot
//!   already contains. A client discards chunks with `seq <= start_seq` and
//!   applies everything newer;
//! - a resync snapshot (sent as [`crate::BinaryKind::PtySnapshot`]) carries the
//!   sequence it replaces in its binary header, so it composes with the same
//!   rule.
//!
//! Backpressure is per viewer: at most [`CREDIT_MAX_CHUNKS`] chunks and
//! [`CREDIT_MAX_BYTES`] bytes may be outstanding between the daemon and one
//! viewer. A viewer that falls outside the window is resynced from a snapshot
//! rather than slowing the other viewers or the PTY.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::ids::{HostId, TerminalId, ThreadId};

/// Maximum number of chunks a viewer may leave unacknowledged.
pub const CREDIT_MAX_CHUNKS: u32 = 8;

/// Maximum number of bytes a viewer may leave unacknowledged (64 KiB).
pub const CREDIT_MAX_BYTES: u32 = 64 * 1024;

/// The state of one daemon-owned terminal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct TerminalInfo {
    /// Stable terminal id for this daemon process.
    pub terminal_id: TerminalId,
    /// Numeric id carried in the binary frame header for this terminal.
    #[ts(type = "number")]
    pub stream_id: u32,
    /// Owning thread, when the terminal was opened from one.
    pub thread_id: Option<ThreadId>,
    /// Host that runs the PTY.
    pub host: HostId,
    /// Working directory the shell started in.
    pub cwd: String,
    /// Executable that was spawned, for display in the pane header.
    pub shell: String,
    /// Effective column count (smallest attached viewer).
    pub cols: u16,
    /// Effective row count (smallest attached viewer).
    pub rows: u16,
    /// Whether the child process is still running.
    pub alive: bool,
    /// Number of viewers currently attached.
    pub viewers: u32,
    /// Exit status once the process ended.
    pub exit: Option<TerminalExit>,
}

/// How a terminal process ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct TerminalExit {
    /// Process exit code, or `-1` when it was killed by a signal.
    pub status: i32,
    /// Signal name on Unix, when the process was signalled.
    pub signal: Option<String>,
    /// When the process was reaped.
    pub at: DateTime<Utc>,
}

/// An ephemeral notice that a terminal exited.
///
/// Terminal exit is not part of the durable thread event log: a fresh client
/// learns the status from [`TerminalInfo::exit`] when it lists or attaches.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct TerminalExitNotice {
    /// Terminal that exited.
    pub terminal_id: TerminalId,
    /// Owning thread, when known.
    pub thread_id: Option<ThreadId>,
    /// Status and signal.
    pub exit: TerminalExit,
}

/// A change in which attached viewer answers terminal queries.
///
/// Every xterm answers device-attribute, cursor-position and colour queries on
/// its own. With two viewers attached that gives the shell duplicate replies,
/// so the daemon elects one responder (the most recently attached viewer) and
/// tells the others to drop their automatic answers. When the responder
/// detaches, the next viewer is promoted and notified.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct TerminalResponderNotice {
    /// Terminal whose responder changed.
    pub terminal_id: TerminalId,
    /// Whether this viewer should answer queries from now on.
    pub answers_queries: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_serialises_ids_transparently() {
        let info = TerminalInfo {
            terminal_id: TerminalId::new("term_1"),
            stream_id: 7,
            thread_id: Some(ThreadId::new("th_1")),
            host: HostId::new("host_atlas"),
            cwd: "/home/you/code/h4rn".into(),
            shell: "/bin/bash".into(),
            cols: 120,
            rows: 32,
            alive: true,
            viewers: 1,
            exit: None,
        };
        let value = serde_json::to_value(&info).unwrap();
        assert_eq!(value["terminal_id"], "term_1");
        assert_eq!(value["thread_id"], "th_1");
        assert_eq!(value["cols"], 120);
    }

    #[test]
    fn exit_notice_carries_a_status() {
        let notice = TerminalExitNotice {
            terminal_id: TerminalId::new("term_1"),
            thread_id: None,
            exit: TerminalExit {
                status: 1,
                signal: None,
                at: Utc::now(),
            },
        };
        let json = serde_json::to_string(&notice).unwrap();
        assert!(json.contains("\"status\":1"));
    }

    #[test]
    fn credit_window_matches_the_research() {
        assert_eq!(CREDIT_MAX_CHUNKS, 8);
        assert_eq!(CREDIT_MAX_BYTES, 65_536);
    }
}
