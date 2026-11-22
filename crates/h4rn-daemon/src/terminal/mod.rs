//! Daemon-owned terminals: PTY processes, headless VT grid and viewers (ADR-007).
//!
//! One [`TerminalManager`] owns every PTY on this host. Opening a terminal
//! validates the working directory, spawns the requested shell through
//! `portable-pty`, starts a reader thread that feeds the `h4rn-vt` grid and
//! broadcasts numbered output chunks. Viewers attach independently; each has
//! its own credit window and its own requested viewport, while the PTY itself
//! runs at the smallest attached viewport so a phone never sees wrapped output.
//!
//! A terminal outlives its viewers: detaching (closing the socket) never kills
//! the process. Only `TerminalClose` or daemon shutdown does.

pub mod audit;
pub mod session;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use h4rn_protocol::{
    DeviceInfo, HostId, ProtoError, ProtoErrorCode, TerminalId, TerminalInfo, ThreadId,
};
use h4rn_vt::{MAX_COLUMNS, MAX_ROWS, MIN_COLUMNS, MIN_ROWS};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tokio::sync::broadcast;

pub use audit::{AuditEntry, AuditSink, FileAudit, TracingAudit};
pub use session::{
    OUTPUT_CHUNK_BYTES, OUTPUT_RING_CAPACITY, OutputChunk, Snapshot, Viewer, ViewerId, run_viewer,
};

use crate::server::Outbound;
use session::{Session, lock};

/// Everything the daemon needs to spawn one terminal.
#[derive(Clone, Debug)]
pub struct OpenSpec {
    /// Owning thread, when opened from one.
    pub thread_id: Option<ThreadId>,
    /// Host that should run the PTY.
    pub host: HostId,
    /// Absolute working directory.
    pub cwd: String,
    /// Explicit shell executable, or the daemon default.
    pub shell: Option<String>,
    /// Extra shell arguments.
    pub args: Vec<String>,
    /// Initial columns.
    pub cols: u16,
    /// Initial rows.
    pub rows: u16,
}

/// A terminal plus the viewer state a freshly attached connection needs.
pub struct Attached {
    /// Current terminal metadata.
    pub terminal: TerminalInfo,
    /// ANSI snapshot at `snapshot.seq`.
    pub snapshot: Snapshot,
    /// Live output subscription.
    pub output: broadcast::Receiver<Arc<OutputChunk>>,
    /// Flow-control handle for this viewer.
    pub viewer: Arc<Viewer>,
    /// Session the viewer belongs to.
    pub session: Arc<Session>,
}

/// Terminal failures that map onto [`ProtoError`].
#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    /// The terminal id is unknown (or was closed).
    #[error("terminal {0} was not found")]
    NotFound(TerminalId),
    /// The request was well formed but cannot be served.
    #[error("{0}")]
    Invalid(String),
    /// The process or PTY could not be started.
    #[error("could not start terminal: {0}")]
    Spawn(String),
    /// Input could not be written to the PTY.
    #[error("could not write terminal input: {0}")]
    Input(String),
}

impl TerminalError {
    /// Maps onto the wire error classes.
    pub fn into_proto(self) -> ProtoError {
        match self {
            Self::NotFound(id) => ProtoError::new(
                ProtoErrorCode::NotFound,
                format!("terminal {id} does not exist on this daemon"),
                "List terminals and attach to one that is still open",
            ),
            Self::Invalid(message) => ProtoError::new(
                ProtoErrorCode::InvalidRequest,
                message,
                "Send an absolute working directory that exists on the host",
            ),
            Self::Spawn(message) => ProtoError::new(
                ProtoErrorCode::Internal,
                format!("the shell could not be started: {message}"),
                "Check the shell path and working directory, then try again",
            ),
            Self::Input(message) => ProtoError::new(
                ProtoErrorCode::Conflict,
                format!("the terminal is no longer accepting input: {message}"),
                "The process exited; open a new terminal",
            ),
        }
    }
}

/// Owns every PTY on this daemon.
pub struct TerminalManager {
    host: HostId,
    audit: Arc<dyn AuditSink>,
    sessions: Mutex<HashMap<TerminalId, Arc<Session>>>,
    stream_ids: Mutex<HashMap<u32, TerminalId>>,
    next_stream: AtomicU64,
    next_viewer: AtomicU64,
}

impl std::fmt::Debug for TerminalManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalManager")
            .field("host", &self.host)
            .field("terminals", &self.terminal_count())
            .finish_non_exhaustive()
    }
}

impl TerminalManager {
    /// Creates an empty manager for `host`, recording opens in `audit`.
    pub fn new(host: HostId, audit: Arc<dyn AuditSink>) -> Self {
        Self {
            host,
            audit,
            sessions: Mutex::new(HashMap::new()),
            stream_ids: Mutex::new(HashMap::new()),
            next_stream: AtomicU64::new(0),
            next_viewer: AtomicU64::new(0),
        }
    }

    /// Host every terminal belongs to.
    pub fn host(&self) -> &HostId {
        &self.host
    }

    /// Allocates a viewer id for one WebSocket connection.
    pub fn next_viewer_id(&self) -> ViewerId {
        self.next_viewer.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Number of live and exited-but-open terminals.
    pub fn terminal_count(&self) -> usize {
        lock(&self.sessions).len()
    }

    /// Opens a PTY and starts feeding it.
    pub async fn open(
        &self,
        spec: OpenSpec,
        device: &DeviceInfo,
    ) -> Result<TerminalInfo, TerminalError> {
        if spec.host != self.host {
            return Err(TerminalError::Invalid(format!(
                "host {} is not served by this daemon ({})",
                spec.host, self.host
            )));
        }
        let cwd = validate_cwd(&spec.cwd)?;
        let shell = match spec.shell {
            Some(shell) if !shell.trim().is_empty() => shell,
            _ => default_shell(),
        };
        let cols = clamp_cols(spec.cols);
        let rows = clamp_rows(spec.rows);

        let spawned = spawn_shell(&shell, &spec.args, &cwd, cols, rows).await?;
        let stream_id = session::next_stream_id(&self.next_stream);
        let id = TerminalId::new_uuid_v7();
        let session = Arc::new(Session::new(
            id.clone(),
            stream_id,
            spec.thread_id.clone(),
            self.host.clone(),
            cwd.clone(),
            shell.clone(),
            cols,
            rows,
            spawned.master,
            spawned.child,
            spawned.writer,
        ));
        session.start_reader(spawned.reader);

        {
            let mut sessions = lock(&self.sessions);
            let mut streams = lock(&self.stream_ids);
            streams.insert(stream_id, id.clone());
            sessions.insert(id.clone(), Arc::clone(&session));
        }

        self.audit.record(&AuditEntry {
            at: Utc::now(),
            terminal_id: id.to_string(),
            thread_id: spec.thread_id.as_ref().map(ToString::to_string),
            device_id: device.device_id.to_string(),
            device_name: device.name.clone(),
            device_platform: device.platform.clone(),
            host: self.host.to_string(),
            cwd,
            shell,
            cols,
            rows,
        });

        Ok(session.info())
    }

    /// Attaches a viewer and returns the snapshot it should render first.
    pub fn attach(
        &self,
        terminal_id: &TerminalId,
        viewer_id: ViewerId,
        tx: tokio::sync::mpsc::Sender<Outbound>,
        cols: Option<u16>,
        rows: Option<u16>,
    ) -> Result<Attached, TerminalError> {
        let session = self.session(terminal_id)?;
        let current = session.info();
        let cols = clamp_cols(cols.unwrap_or(current.cols));
        let rows = clamp_rows(rows.unwrap_or(current.rows));

        let output = session.subscribe();
        let viewer = Arc::new(Viewer::new(tx));
        session.add_viewer(viewer_id, cols, rows, Arc::clone(&viewer));
        let snapshot = session.snapshot();
        // The snapshot replaces everything older, so the window starts empty.
        viewer.reset_credit();

        Ok(Attached {
            terminal: session.info(),
            snapshot,
            output,
            viewer,
            session,
        })
    }

    /// Resizes one viewer and applies the smallest attached viewport.
    pub fn resize(
        &self,
        terminal_id: &TerminalId,
        viewer_id: ViewerId,
        cols: u16,
        rows: u16,
    ) -> Result<(), TerminalError> {
        let session = self.session(terminal_id)?;
        if !session.has_viewer(viewer_id) {
            return Err(TerminalError::Invalid(
                "this connection is not attached to the terminal".into(),
            ));
        }
        session.resize_viewer(viewer_id, clamp_cols(cols), clamp_rows(rows));
        Ok(())
    }

    /// Writes raw input to a terminal selected by its binary stream id, on
    /// behalf of one connection.
    ///
    /// The connection must have an attached viewer on that terminal. A stream
    /// id alone is not a capability: ids are sequential and guessable, so a
    /// connection that never attached (or is not allowed to) is refused with
    /// the same error as an unknown stream.
    ///
    /// A marked `reply` is xterm's answer to a terminal query. At most one
    /// attached viewer may answer, so replies from the others are dropped; the
    /// role passes to another viewer when the owner detaches or stops
    /// receiving output.
    pub fn input_by_stream(
        &self,
        stream_id: u32,
        viewer_id: ViewerId,
        data: &[u8],
        reply: bool,
    ) -> Result<(), TerminalError> {
        let terminal_id = lock(&self.stream_ids).get(&stream_id).cloned();
        let not_found = || TerminalError::NotFound(TerminalId::new(format!("stream {stream_id}")));
        let terminal_id = terminal_id.ok_or_else(not_found)?;
        let session = self.session(&terminal_id)?;
        session
            .write_viewer_input(viewer_id, data, reply)
            .map_err(|error| match error {
                session::ViewerInputError::NotAttached => not_found(),
                session::ViewerInputError::Io(error) => TerminalError::Input(error.to_string()),
            })
    }

    /// Frees viewer credit through `up_to_chunk`.
    pub fn ack(
        &self,
        terminal_id: &TerminalId,
        viewer_id: ViewerId,
        up_to_chunk: u64,
    ) -> Result<(), TerminalError> {
        let session = self.session(terminal_id)?;
        session.ack_viewer(viewer_id, up_to_chunk);
        Ok(())
    }

    /// Lists terminals, optionally filtered to one thread.
    pub fn list(&self, thread_id: Option<&ThreadId>) -> Vec<TerminalInfo> {
        let sessions = lock(&self.sessions);
        let mut terminals: Vec<TerminalInfo> = sessions
            .values()
            .filter(|session| match thread_id {
                Some(thread) => session.info().thread_id.as_ref() == Some(thread),
                None => true,
            })
            .map(|session| session.info())
            .collect();
        terminals.sort_by(|left, right| left.terminal_id.cmp(&right.terminal_id));
        terminals
    }

    /// Closes one terminal: viewers are closed, then the shell is hung up and
    /// given time to forward the hangup to its jobs, terminated with a grace
    /// period, and finally killed with its process group. Windows has no
    /// POSIX signals; the ConPTY ends when the PTY is dropped, which closes
    /// the console processes attached to it.
    pub async fn close(&self, terminal_id: &TerminalId) -> Result<(), TerminalError> {
        let session = {
            let mut sessions = lock(&self.sessions);
            sessions.remove(terminal_id)
        };
        let Some(session) = session else {
            return Err(TerminalError::NotFound(terminal_id.clone()));
        };
        {
            let mut streams = lock(&self.stream_ids);
            streams.retain(|_, id| id != terminal_id);
        }
        session.close_viewers();
        session.kill_with_grace().await;
        Ok(())
    }

    /// Removes every viewer belonging to one connection (socket closed).
    pub fn detach_viewer(&self, viewer_id: ViewerId) {
        let sessions: Vec<Arc<Session>> = lock(&self.sessions).values().cloned().collect();
        for session in sessions {
            session.remove_viewer(viewer_id);
        }
    }

    /// Kills every terminal process group (daemon shutdown).
    pub async fn shutdown(&self) {
        let sessions: Vec<Arc<Session>> = {
            let mut sessions = lock(&self.sessions);
            sessions.drain().map(|(_, session)| session).collect()
        };
        lock(&self.stream_ids).clear();
        for session in sessions {
            session.close_viewers();
            session.kill_with_grace().await;
        }
    }

    fn session(&self, terminal_id: &TerminalId) -> Result<Arc<Session>, TerminalError> {
        lock(&self.sessions)
            .get(terminal_id)
            .cloned()
            .ok_or_else(|| TerminalError::NotFound(terminal_id.clone()))
    }
}

struct SpawnedPty {
    master: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    reader: Box<dyn std::io::Read + Send>,
    writer: Box<dyn std::io::Write + Send>,
}

async fn spawn_shell(
    shell: &str,
    args: &[String],
    cwd: &str,
    cols: u16,
    rows: u16,
) -> Result<SpawnedPty, TerminalError> {
    let shell = shell.to_owned();
    let args = args.to_vec();
    let cwd = cwd.to_owned();
    tokio::task::spawn_blocking(move || {
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = native_pty_system()
            .openpty(size)
            .map_err(|error| TerminalError::Spawn(error.to_string()))?;

        let mut command = CommandBuilder::new(&shell);
        for arg in &args {
            command.arg(arg);
        }
        command.cwd(&cwd);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");

        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| TerminalError::Spawn(error.to_string()))?;
        // The slave handle must close in the parent so the master sees EOF
        // when the child exits.
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| TerminalError::Spawn(error.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| TerminalError::Spawn(error.to_string()))?;

        Ok(SpawnedPty {
            master: pair.master,
            child,
            reader,
            writer,
        })
    })
    .await
    .map_err(|error| TerminalError::Spawn(error.to_string()))?
}

fn validate_cwd(cwd: &str) -> Result<String, TerminalError> {
    let path = PathBuf::from(cwd);
    if cwd.trim().is_empty() || !path.is_absolute() {
        return Err(TerminalError::Invalid(
            "the terminal working directory must be an absolute path".into(),
        ));
    }
    match std::fs::metadata(&path) {
        Ok(metadata) if metadata.is_dir() => Ok(cwd.to_owned()),
        Ok(_) => Err(TerminalError::Invalid(format!(
            "the terminal working directory {cwd} is not a directory"
        ))),
        Err(error) => Err(TerminalError::Invalid(format!(
            "the terminal working directory {cwd} is not available: {error}"
        ))),
    }
}

/// The daemon user's login shell, or the platform fallback.
pub fn default_shell() -> String {
    #[cfg(windows)]
    {
        std::env::var("ComSpec")
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "pwsh".to_owned())
    }
    #[cfg(not(windows))]
    {
        std::env::var("SHELL")
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "/bin/sh".to_owned())
    }
}

fn clamp_cols(cols: u16) -> u16 {
    cols.clamp(MIN_COLUMNS, MAX_COLUMNS)
}

fn clamp_rows(rows: u16) -> u16 {
    rows.clamp(MIN_ROWS, MAX_ROWS)
}
