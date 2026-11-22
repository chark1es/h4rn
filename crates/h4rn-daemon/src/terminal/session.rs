//! One PTY session: process, screen model, output fan-out and per-viewer flow
//! control (ADR-007).
//!
//! A [`Session`] owns the PTY master, the child process, the headless VT grid
//! and every attached viewer. The reader thread is the only producer of output;
//! it feeds the grid and broadcasts numbered chunks. Each viewer has an
//! independent credit window, so a slow phone never blocks a desktop viewer or
//! the PTY itself. A viewer that falls outside the broadcast ring is resynced
//! from a fresh snapshot instead of replaying history.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use chrono::Utc;
use h4rn_protocol::{
    BinaryFrame, BinaryKind, CREDIT_MAX_BYTES, CREDIT_MAX_CHUNKS, HostId, TerminalExit,
    TerminalExitNotice, TerminalId, TerminalInfo, ThreadId,
};
use h4rn_vt::{AlacrittyVt, TerminalModel};
use portable_pty::{Child, MasterPty, PtySize};
use tokio::sync::{Notify, broadcast, mpsc};

use crate::server::Outbound;

/// Capacity of the per-session broadcast ring.
///
/// The ring is deliberately larger than the credit window: a viewer pauses on
/// credit, not on the ring, so a normal slow reader never resyncs. Only a viewer
/// that stops acknowledging until the ring wraps falls back to a snapshot.
pub const OUTPUT_RING_CAPACITY: usize = 256;

/// Largest output chunk emitted to a viewer.
pub const OUTPUT_CHUNK_BYTES: usize = 64 * 1024;

/// How long the shell is given to act on the closing hangup.
///
/// The wait ends as soon as the shell exits; this bound only applies to a
/// shell that ignores SIGHUP.
const HANGUP_GRACE: Duration = Duration::from_secs(2);

/// How long a job's TERM handler may take before its group is killed.
const TERM_GRACE: Duration = Duration::from_millis(150);

/// One viewer of one terminal.
pub type ViewerId = u64;

/// One numbered output chunk.
#[derive(Debug)]
pub struct OutputChunk {
    /// Per-terminal sequence, starting at 1.
    pub seq: u64,
    /// Raw PTY bytes.
    pub data: Vec<u8>,
}

/// A consistent view of the screen at one sequence.
pub struct Snapshot {
    /// ANSI that recreates the screen and scrollback.
    pub ansi: String,
    /// Last chunk sequence contained in `ansi`.
    pub seq: u64,
    /// Effective columns.
    pub cols: u16,
    /// Effective rows.
    pub rows: u16,
}

/// How output is framed for one attached viewer.
pub struct Viewer {
    /// Connection outbound queue.
    pub tx: mpsc::Sender<Outbound>,
    /// Outstanding credit.
    pub credit: Mutex<Credit>,
    /// Wakes the viewer when an ack frees credit or the viewer is detached.
    pub notify: Notify,
    /// Set when the viewer is detached.
    pub closed: AtomicBool,
}

impl Viewer {
    /// Creates a viewer with a full credit window.
    pub fn new(tx: mpsc::Sender<Outbound>) -> Self {
        Self {
            tx,
            credit: Mutex::new(Credit::default()),
            notify: Notify::new(),
            closed: AtomicBool::new(false),
        }
    }

    /// Marks the viewer closed and wakes its task.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// Resets the window after an attach snapshot.
    ///
    /// The snapshot replaces all output, so it becomes the latest position
    /// this viewer has, and the viewer starts parsing raw output at the next
    /// chunk. A reply may name a later chunk only once `seq`'s successor has
    /// also arrived, because a query that started in `seq` is invisible to a
    /// parser that only renders the snapshot.
    pub fn reset_credit(&self, seq: u64) {
        let mut credit = lock(&self.credit);
        credit.outstanding.clear();
        credit.bytes = 0;
        credit.snapshot_at(seq);
        self.notify.notify_waiters();
    }

    /// Clears the window before a resync snapshot is handed over.
    ///
    /// The sent bound and the reply bound stay put until the snapshot really
    /// reaches the outbound channel (see [`Viewer::record_snapshot_sent`]).
    pub fn begin_resync(&self) {
        let mut credit = lock(&self.credit);
        credit.outstanding.clear();
        credit.bytes = 0;
    }

    /// Records that a resync snapshot reached the outbound channel.
    pub fn record_snapshot_sent(&self, seq: u64) {
        let mut credit = lock(&self.credit);
        credit.snapshot_at(seq);
    }

    /// Records a sent chunk so a later ack can free its credit.
    ///
    /// The sent bound advances only when the frame has been handed to the
    /// outbound channel, so a reply cannot name output the viewer never got.
    /// The receiver can acknowledge the frame before the sender gets here (the
    /// handoff and the ack are on different tasks), so a chunk at or below the
    /// acked position is already accounted for and must not be recorded again.
    pub fn record_sent(&self, seq: u64, bytes: usize) {
        let mut credit = lock(&self.credit);
        credit.sent_through = credit.sent_through.max(seq);
        if seq <= credit.acked_through {
            return;
        }
        credit.outstanding.push_back((seq, bytes));
        credit.bytes += bytes;
    }

    /// The earliest output position whose answer this viewer may name.
    ///
    /// The viewer must have parsed the raw bytes of the chunk before the one
    /// it answers, so a query that started in the previous chunk is part of
    /// its numbering. A viewer that attached at position zero is the
    /// exception: no earlier chunk exists, so its first chunk is answerable.
    pub fn answer_from(&self) -> u64 {
        lock(&self.credit).answer_from
    }

    /// The latest output position actually sent to this viewer.
    ///
    /// Replies are validated against this exact value, not against credit: a
    /// viewer may only answer a query from output it was sent.
    pub fn sent_through(&self) -> u64 {
        lock(&self.credit).sent_through
    }

    /// Frees credit acknowledged by `up_to`.
    pub fn ack(&self, up_to: u64) {
        let mut credit = lock(&self.credit);
        // The ack can arrive before the corresponding `record_sent`, so the
        // position is remembered and a late record for it is skipped.
        credit.acked_through = credit.acked_through.max(up_to);
        while let Some((seq, bytes)) = credit.outstanding.front().copied() {
            if seq > credit.acked_through {
                break;
            }
            credit.outstanding.pop_front();
            credit.bytes = credit.bytes.saturating_sub(bytes);
        }
        drop(credit);
        self.notify.notify_waiters();
    }

    /// Waits until `bytes` fit in the credit window.
    ///
    /// Returns `false` when the viewer was detached while waiting.
    pub async fn wait_for_credit(&self, bytes: usize) -> bool {
        loop {
            if self.closed.load(Ordering::SeqCst) {
                return false;
            }
            // `enable` registers interest before the credit check, so an ack
            // that lands between the check and the await is not lost.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let credit = lock(&self.credit);
                if credit.outstanding.len() < CREDIT_MAX_CHUNKS as usize
                    && credit.bytes + bytes <= CREDIT_MAX_BYTES as usize
                {
                    return true;
                }
            }
            notified.await;
        }
    }
}

/// Outstanding chunk credit for one viewer.
#[derive(Debug)]
pub struct Credit {
    outstanding: VecDeque<(u64, usize)>,
    bytes: usize,
    /// Latest output position sent to this viewer.
    sent_through: u64,
    /// Earliest output position this viewer may answer (see
    /// [`Viewer::answer_from`]).
    answer_from: u64,
    /// Highest position the viewer acknowledged.
    ///
    /// An ack can overtake the send that records the same chunk, so this also
    /// tells a late record that the chunk is already accounted for.
    acked_through: u64,
}

impl Default for Credit {
    fn default() -> Self {
        Self {
            outstanding: VecDeque::new(),
            bytes: 0,
            sent_through: 0,
            answer_from: 1,
            acked_through: 0,
        }
    }
}

impl Credit {
    /// Applies a snapshot at `seq`: it becomes the latest position, and the
    /// parser restarts at the next raw chunk.
    fn snapshot_at(&mut self, seq: u64) {
        self.sent_through = self.sent_through.max(seq);
        // The snapshot carries position `seq` as rendered state, not as raw
        // bytes. To number answers the way an earlier viewer does, this one
        // needs both `seq + 1` and the chunk before the one it answers.
        self.answer_from = if seq == 0 { 1 } else { seq + 2 };
    }
}

/// One attached viewer with its requested viewport.
#[derive(Clone)]
pub struct ViewerEntry {
    /// Flow-control state.
    pub viewer: Arc<Viewer>,
    /// Requested columns.
    pub cols: u16,
    /// Requested rows.
    pub rows: u16,
    /// Whether this client names the output position on every marked reply.
    ///
    /// A client that does not is an older build. While one is attached the
    /// terminal uses the legacy single-owner rule for every reply; only when
    /// every attached viewer declares positions is each answer decided by its
    /// `(position, ordinal)` through [`AnsweredQueries`].
    pub reply_positions: bool,
    /// Whether this viewer owns every reply while a legacy client is attached.
    ///
    /// The first viewer to send a marked reply takes the role; it passes to
    /// another viewer only when it detaches.
    pub legacy_owner: bool,
}

/// A PTY session.
pub struct Session {
    id: TerminalId,
    stream_id: u32,
    thread_id: Option<ThreadId>,
    host: HostId,
    cwd: String,
    shell: String,
    screen: Mutex<Screen>,
    output: broadcast::Sender<Arc<OutputChunk>>,
    viewers: Mutex<HashMap<ViewerId, ViewerEntry>>,
    answers: Mutex<AnsweredQueries>,
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    pty: Mutex<Box<dyn MasterPty + Send>>,
    /// The spawned shell's own pid, for the initial hangup.
    ///
    /// `process_group` comes from `tcgetpgrp`, which reports whatever holds
    /// the terminal in the foreground; during startup that can be a job the
    /// shell already started, so the hangup must not use it.
    shell_pid: Option<i32>,
    process_group: Option<i32>,
    exit: Mutex<Option<TerminalExit>>,
    /// Status captured when `wait_for_shell_exit` reaped the child.
    ///
    /// `finish` runs on the reader thread and may reach `wait` after that, so
    /// the captured status is reused instead of reporting the failed second
    /// wait as `-1`.
    reaped_status: Mutex<Option<portable_pty::ExitStatus>>,
    alive: AtomicBool,
}

struct Screen {
    vt: AlacrittyVt,
    seq: u64,
}

impl Session {
    /// Builds a session around a spawned PTY.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: TerminalId,
        stream_id: u32,
        thread_id: Option<ThreadId>,
        host: HostId,
        cwd: String,
        shell: String,
        cols: u16,
        rows: u16,
        master: Box<dyn MasterPty + Send>,
        child: Box<dyn Child + Send + Sync>,
        writer: Box<dyn Write + Send>,
    ) -> Self {
        #[cfg(unix)]
        let process_group = master.process_group_leader();
        #[cfg(not(unix))]
        let process_group = None;
        // The child is the shell itself when it is spawned; `tcgetpgrp` above
        // can already point at a startup job in the foreground.
        let shell_pid = child.process_id().map(|pid| pid as i32);
        let (output, _) = broadcast::channel(OUTPUT_RING_CAPACITY);
        Self {
            id,
            stream_id,
            thread_id,
            host,
            cwd,
            shell,
            screen: Mutex::new(Screen {
                vt: AlacrittyVt::new(cols, rows),
                seq: 0,
            }),
            output,
            viewers: Mutex::new(HashMap::new()),
            answers: Mutex::new(AnsweredQueries::default()),
            writer: Mutex::new(Some(writer)),
            child: Mutex::new(child),
            pty: Mutex::new(master),
            shell_pid,
            process_group,
            exit: Mutex::new(None),
            reaped_status: Mutex::new(None),
            alive: AtomicBool::new(true),
        }
    }

    /// Terminal id.
    pub fn id(&self) -> &TerminalId {
        &self.id
    }

    /// Binary stream id used in the frame header.
    pub fn stream_id(&self) -> u32 {
        self.stream_id
    }

    /// Subscribes to live output.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<OutputChunk>> {
        self.output.subscribe()
    }

    /// A consistent ANSI snapshot and the sequence it replaces.
    pub fn snapshot(&self) -> Snapshot {
        let screen = lock(&self.screen);
        Snapshot {
            ansi: screen.vt.snapshot(),
            seq: screen.seq,
            cols: screen.vt.cols(),
            rows: screen.vt.rows(),
        }
    }

    /// Current metadata.
    pub fn info(&self) -> TerminalInfo {
        let screen = lock(&self.screen);
        let viewers = lock(&self.viewers).len() as u32;
        TerminalInfo {
            terminal_id: self.id.clone(),
            stream_id: self.stream_id,
            thread_id: self.thread_id.clone(),
            host: self.host.clone(),
            cwd: self.cwd.clone(),
            shell: self.shell.clone(),
            cols: screen.vt.cols(),
            rows: screen.vt.rows(),
            alive: self.alive.load(Ordering::SeqCst),
            viewers,
            exit: lock(&self.exit).clone(),
        }
    }

    /// Receives PTY bytes, updates the grid and fans the chunk out.
    fn push_output(&self, data: &[u8]) {
        let chunk = {
            let mut screen = lock(&self.screen);
            screen.vt.feed(data);
            screen.seq += 1;
            Arc::new(OutputChunk {
                seq: screen.seq,
                data: data.to_vec(),
            })
        };
        // `Err` only means no viewer is attached right now; the grid already
        // has the bytes, so the next attach gets them in a snapshot.
        let _ = self.output.send(chunk);
    }

    /// Spawns the blocking reader thread for this session.
    pub fn start_reader(self: &Arc<Self>, mut reader: Box<dyn Read + Send>) {
        let session = Arc::clone(self);
        let name = format!("h4rn-pty-{}", session.stream_id);
        let spawned = std::thread::Builder::new().name(name).spawn(move || {
            let mut buffer = vec![0u8; OUTPUT_CHUNK_BYTES];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => session.push_output(&buffer[..read]),
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            session.finish();
        });
        if let Err(error) = spawned {
            tracing::warn!(terminal_id = %self.id, %error, "could not start PTY reader");
            self.finish();
        }
    }

    /// Reaps the child, records the exit status and tells attached viewers.
    fn finish(&self) {
        if !self.alive.swap(false, Ordering::SeqCst) {
            return;
        }
        let status = {
            // `wait_for_shell_exit` may already have reaped the child while
            // closing the terminal; a second `wait` would then fail and lose
            // the real status.
            let captured = lock(&self.reaped_status).take();
            match captured {
                Some(status) => Some(status),
                None => {
                    let mut child = lock(&self.child);
                    child.wait().ok()
                }
            }
        };
        let exit = match status {
            Some(status) => {
                let signal = status.signal().map(str::to_owned);
                TerminalExit {
                    status: if signal.is_some() {
                        -1
                    } else {
                        status.exit_code() as i32
                    },
                    signal,
                    at: Utc::now(),
                }
            }
            None => TerminalExit {
                status: -1,
                signal: None,
                at: Utc::now(),
            },
        };
        *lock(&self.exit) = Some(exit.clone());
        let notice = TerminalExitNotice {
            terminal_id: self.id.clone(),
            thread_id: self.thread_id.clone(),
            exit,
        };
        let frame =
            serde_json::to_string(&h4rn_protocol::ServerFrame::TerminalEvent { event: notice })
                .unwrap_or_default();
        let viewers = lock(&self.viewers);
        for entry in viewers.values() {
            // The viewer task may have ended; a failed send only means no one is
            // listening, not that the session failed.
            let _ = entry.viewer.tx.try_send(Outbound::Text(frame.clone()));
        }
    }

    /// Adds a viewer and recomputes the effective size.
    ///
    /// Attaching twice on one connection replaces the old viewer entry; the
    /// previous task is closed so it cannot outlive its successor. A client
    /// that does not declare positioned replies keeps the terminal in the
    /// legacy single-owner mode for as long as it is attached.
    pub fn add_viewer(
        &self,
        id: ViewerId,
        cols: u16,
        rows: u16,
        reply_positions: bool,
        viewer: Arc<Viewer>,
    ) {
        let previous = {
            let mut viewers = lock(&self.viewers);
            if !reply_positions {
                // The reply owner is chosen afresh while a legacy client is
                // present; an earlier owner's role must not outlive it.
                clear_legacy_owners(&mut viewers);
            }
            viewers.insert(
                id,
                ViewerEntry {
                    viewer,
                    cols,
                    rows,
                    reply_positions,
                    legacy_owner: false,
                },
            )
        };
        if let Some(previous) = previous {
            previous.viewer.close();
            if (previous.cols, previous.rows) != (cols, rows) {
                self.apply_smallest_size();
            }
        } else {
            self.apply_smallest_size();
        }
    }

    /// Removes one viewer and recomputes the effective size.
    pub fn remove_viewer(&self, id: ViewerId) {
        self.remove_viewer_matching(id, None);
    }

    /// Removes `id` only when it still maps to `viewer`.
    ///
    /// A run task that was replaced by a newer attach must not remove its
    /// successor's viewer entry when it eventually exits.
    pub fn remove_viewer_if(&self, id: ViewerId, viewer: &Arc<Viewer>) {
        self.remove_viewer_matching(id, Some(viewer));
    }

    fn remove_viewer_matching(&self, id: ViewerId, expected: Option<&Arc<Viewer>>) {
        let removed = {
            let mut viewers = lock(&self.viewers);
            let matches = match viewers.get(&id) {
                Some(entry) => expected.is_none_or(|viewer| Arc::ptr_eq(&entry.viewer, viewer)),
                None => false,
            };
            if !matches {
                return;
            }
            // Dropping the entry also drops the legacy owner for replies with
            // no position, so another old client can take over.
            let removed = viewers.remove(&id);
            if removed.as_ref().is_some_and(|entry| !entry.reply_positions) {
                clear_legacy_owners(&mut viewers);
            }
            removed
        };
        let Some(removed) = removed else {
            return;
        };
        removed.viewer.close();
        self.apply_smallest_size();
    }

    /// Closes every viewer (daemon shutdown or terminal close).
    pub fn close_viewers(&self) {
        let viewers = std::mem::take(&mut *lock(&self.viewers));
        for entry in viewers.into_values() {
            entry.viewer.close();
        }
    }

    /// Updates one viewer's requested viewport.
    pub fn resize_viewer(&self, id: ViewerId, cols: u16, rows: u16) -> bool {
        let mut changed = false;
        {
            let mut viewers = lock(&self.viewers);
            if let Some(entry) = viewers.get_mut(&id)
                && (entry.cols != cols || entry.rows != rows)
            {
                entry.cols = cols;
                entry.rows = rows;
                changed = true;
            }
        }
        if changed {
            self.apply_smallest_size();
        }
        changed
    }

    /// Applies the smallest attached viewport to the PTY and the grid.
    fn apply_smallest_size(&self) {
        let target = {
            let viewers = lock(&self.viewers);
            let cols = viewers.values().map(|entry| entry.cols).min();
            let rows = viewers.values().map(|entry| entry.rows).min();
            cols.zip(rows)
        };
        let Some((cols, rows)) = target else {
            return;
        };
        self.resize(cols, rows);
    }

    /// Resizes the PTY and the screen model.
    pub fn resize(&self, cols: u16, rows: u16) {
        let (cols, rows) = {
            let mut screen = lock(&self.screen);
            screen.vt.resize(cols, rows);
            (screen.vt.cols(), screen.vt.rows())
        };
        let pty = lock(&self.pty);
        if let Err(error) = pty.resize(PtySize {
            cols,
            rows,
            pixel_width: 0,
            pixel_height: 0,
        }) {
            tracing::warn!(terminal_id = %self.id, %error, "could not resize PTY");
        }
    }

    /// Writes one attached viewer's input to the PTY.
    ///
    /// Viewers share one PTY, so a query must be answered once. A marked reply
    /// that names the output position (and the answer's ordinal within it) is
    /// written by the first viewer to send it, from any viewer; an exact
    /// duplicate, or an answer for a chunk the record has forgotten, is
    /// dropped. A stalled viewer therefore never blocks another viewer's
    /// answer. A position the daemon never sent to that viewer is ignored, so
    /// a bogus claim cannot suppress a real answer.
    ///
    /// The acceptance decision and the PTY write happen under the same writer
    /// lock: two connections cannot be accepted in one order and have their
    /// bytes reach the shell in another.
    pub fn write_viewer_input(
        &self,
        viewer_id: ViewerId,
        data: &[u8],
        reply: QueryReply,
    ) -> Result<(), ViewerInputError> {
        write_input_locked(
            &self.writer,
            &self.viewers,
            &self.answers,
            viewer_id,
            data,
            reply,
        )
    }

    /// Whether a viewer is attached.
    pub fn has_viewer(&self, id: ViewerId) -> bool {
        lock(&self.viewers).contains_key(&id)
    }

    /// Whether the child is still running.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Hangs up, waits for the shell to exit, then terminates with a grace
    /// period so shells and their jobs can save state.
    ///
    /// The order is: SIGHUP to the shell's own pid, wait until the shell exits
    /// (bounded, so a shell that ignores the hangup cannot delay shutdown
    /// forever), SIGTERM to the shell's process group, a grace period for TERM
    /// handlers to clean up, then `child.kill()` and SIGKILL for the group. The
    /// hangup targets the spawned child rather than the cached foreground
    /// group: a shell that starts a job while it boots puts that job in the
    /// foreground, and the job would receive the hangup instead of the shell.
    ///
    /// Waiting for the shell rather than a fixed delay is what makes the
    /// hangup reliable under load: an interactive shell forwards SIGHUP to its
    /// jobs before it exits, and a loaded machine can take longer than any
    /// fixed pause. The wait only runs when the platform actually delivered a
    /// hangup, so closing a terminal on Windows still takes no two seconds.
    /// Windows has no POSIX sessions or signals: the ConPTY closes when the PTY
    /// is dropped, which ends the console processes attached to it, and the
    /// SIGHUP/SIGTERM steps are no-ops there.
    pub async fn kill_with_grace(&self) {
        // Hang up first and wait, so the shell can run its hangup path and
        // its jobs hear SIGHUP; only then fall back to TERM and KILL for the
        // shell's group.
        let hung_up = hangup_pid(self.shell_pid, self.process_group).is_some_and(signal_shell);
        if hung_up {
            self.wait_for_shell_exit(HANGUP_GRACE).await;
        }
        if let Some(pgid) = self.process_group {
            signal_group(pgid, Signal::Term);
        }
        // Keep the grace period after TERM so a job's cleanup handler can run
        // before the group is killed.
        tokio::time::sleep(TERM_GRACE).await;
        {
            let mut child = lock(&self.child);
            let _ = child.kill();
        }
        if let Some(pgid) = self.process_group {
            signal_group(pgid, Signal::Kill);
        }
    }

    /// Waits until the shell exits, or `limit` passes.
    ///
    /// `try_wait` reaps the shell when it has exited; the status is kept so
    /// `finish` can still report it when the reader thread notices the close.
    async fn wait_for_shell_exit(&self, limit: Duration) {
        let deadline = Instant::now() + limit;
        loop {
            let status = {
                let mut child = lock(&self.child);
                match child.try_wait() {
                    Ok(Some(status)) => Some(Some(status)),
                    Err(_) => Some(None),
                    Ok(None) => None,
                }
            };
            match status {
                Some(Some(status)) => {
                    *lock(&self.reaped_status) = Some(status);
                    return;
                }
                Some(None) => return,
                None => {}
            }
            if Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

/// Writes one viewer's input under the writer lock.
///
/// The lock covers the acceptance decision and the write, so two connections
/// cannot be accepted in one order and have their bytes reach the shell in
/// another.
fn write_input_locked(
    writer: &Mutex<Option<Box<dyn Write + Send>>>,
    viewers: &Mutex<HashMap<ViewerId, ViewerEntry>>,
    answers: &Mutex<AnsweredQueries>,
    viewer_id: ViewerId,
    data: &[u8],
    reply: QueryReply,
) -> Result<(), ViewerInputError> {
    let mut writer = lock(writer);
    let Some(writer) = writer.as_mut() else {
        return Err(ViewerInputError::Io(std::io::Error::other(
            "terminal input is closed",
        )));
    };
    let viewer = lock(viewers)
        .get(&viewer_id)
        .map(|entry| Arc::clone(&entry.viewer));
    let Some(viewer) = viewer else {
        return Err(ViewerInputError::NotAttached);
    };
    if reply != QueryReply::No && !accepts_reply(viewers, answers, viewer_id, &viewer, reply) {
        return Ok(());
    }
    writer.write_all(data).map_err(ViewerInputError::Io)?;
    writer.flush().map_err(ViewerInputError::Io)?;
    Ok(())
}

/// Decides whether one marked reply may be written.
///
/// The caller holds the writer lock, so the decision is not reordered against
/// another connection's write.
fn accepts_reply(
    viewers: &Mutex<HashMap<ViewerId, ViewerEntry>>,
    answers: &Mutex<AnsweredQueries>,
    viewer_id: ViewerId,
    viewer: &Viewer,
    reply: QueryReply,
) -> bool {
    let latest = viewer.sent_through();
    let answer_from = viewer.answer_from();
    let mut viewers = lock(viewers);
    // Mixed versions are decided when each client attaches, not by whichever
    // reply happens to arrive first. While any attached viewer cannot name a
    // position, every reply follows the legacy single-owner rule: the first
    // viewer to send a marked reply owns them all, and its positioned replies
    // are still validated and deduplicated. Any arrival order therefore writes
    // one answer, never two.
    if viewers.values().any(|entry| !entry.reply_positions) {
        let owner = viewers
            .iter()
            .find(|(_, entry)| entry.legacy_owner)
            .map(|(id, _)| *id);
        match owner {
            Some(owner) if owner != viewer_id => return false,
            Some(_) => {}
            None => match viewers.get_mut(&viewer_id) {
                Some(entry) => entry.legacy_owner = true,
                None => return false,
            },
        }
        return match reply {
            QueryReply::No | QueryReply::Unpositioned => true,
            QueryReply::At { position, ordinal } => {
                accept_position(answers, position, ordinal, latest, answer_from)
            }
        };
    }
    match reply {
        QueryReply::No => true,
        // Every attached viewer declared positions, so an unpositioned reply
        // breaks that contract and names no answer: writing it could duplicate
        // a query another viewer already answered.
        QueryReply::Unpositioned => false,
        QueryReply::At { position, ordinal } => {
            accept_position(answers, position, ordinal, latest, answer_from)
        }
    }
}

/// Clears every legacy reply owner.
fn clear_legacy_owners(viewers: &mut HashMap<ViewerId, ViewerEntry>) {
    for entry in viewers.values_mut() {
        entry.legacy_owner = false;
    }
}

/// Records an answer for `(position, ordinal)` if it is new to this viewer.
fn accept_position(
    answers: &Mutex<AnsweredQueries>,
    position: u64,
    ordinal: u64,
    latest: u64,
    answer_from: u64,
) -> bool {
    // A viewer cannot answer a query from output it was never sent, nor from
    // a chunk whose predecessor it never parsed: an answer's ordinal is counted
    // from the start of the chunk, so a viewer that attached mid-query would
    // number the same answers differently. Drop the reply without advancing
    // the record, so it cannot suppress the answer of a viewer that really saw
    // the query.
    if position == 0 || position > latest || position < answer_from {
        return false;
    }
    lock(answers).accept(position, ordinal)
}

/// Which query a reply answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryReply {
    /// Typed input, not a reply.
    No,
    /// A reply from a client that does not name the output position.
    Unpositioned,
    /// A reply to one answer of the queries in an output chunk.
    At {
        /// Output chunk that contained the query (its end, if split).
        position: u64,
        /// The answer's index among the answers expected for that chunk.
        ordinal: u64,
    },
}

/// Number of recent chunks whose answers are tracked individually.
const ANSWERED_CHUNKS: usize = 64;

/// Number of answer ordinals tracked per recent chunk.
const ANSWERED_ORDINALS_PER_CHUNK: usize = 1024;

/// The answers already written on one terminal.
///
/// One chunk can carry many queries, so a single high-water position cannot
/// say which of its answers were written. Recent chunks keep a bounded bitset
/// of their answer ordinals; chunks older than that collapse into a high-water
/// mark and their answers are treated as duplicates. Memory stays constant no
/// matter how many queries the program asks.
#[derive(Debug, Default)]
struct AnsweredQueries {
    /// Every chunk at or below this position is old; its answers are dropped.
    through: u64,
    /// Recent chunks, oldest first, with the ordinals already written.
    recent: BTreeMap<u64, ChunkAnswers>,
}

#[derive(Debug, Default)]
struct ChunkAnswers {
    /// One bit per ordinal below [`ANSWERED_ORDINALS_PER_CHUNK`].
    bits: [u64; ANSWERED_ORDINALS_PER_CHUNK / 64],
    /// Highest ordinal at or above the bitset that was written.
    ///
    /// Real batches fit the bitset; this keeps one exact answer for the rare
    /// larger chunk without growing memory.
    overflow: u64,
}

impl ChunkAnswers {
    fn accept(&mut self, ordinal: u64) -> bool {
        let Ok(index) = usize::try_from(ordinal) else {
            return false;
        };
        if index < ANSWERED_ORDINALS_PER_CHUNK {
            let word = &mut self.bits[index / 64];
            let bit = 1_u64 << (index % 64);
            if *word & bit != 0 {
                return false;
            }
            *word |= bit;
            return true;
        }
        if ordinal <= self.overflow {
            return false;
        }
        self.overflow = ordinal;
        true
    }
}

impl AnsweredQueries {
    /// Records an answer, returning whether it is new.
    fn accept(&mut self, position: u64, ordinal: u64) -> bool {
        if position <= self.through {
            return false;
        }
        if !self.recent.contains_key(&position) {
            while self.recent.len() >= ANSWERED_CHUNKS {
                let Some((oldest, _)) = self.recent.pop_first() else {
                    break;
                };
                self.through = self.through.max(oldest);
            }
        }
        self.recent.entry(position).or_default().accept(ordinal)
    }

    /// Whether `(position, ordinal)` is already recorded.
    #[cfg(test)]
    fn answered(&self, position: u64, ordinal: u64) -> bool {
        if position <= self.through {
            return true;
        }
        let Some(chunk) = self.recent.get(&position) else {
            return false;
        };
        let Ok(index) = usize::try_from(ordinal) else {
            return false;
        };
        if index < ANSWERED_ORDINALS_PER_CHUNK {
            chunk.bits[index / 64] & (1_u64 << (index % 64)) != 0
        } else {
            ordinal <= chunk.overflow
        }
    }
}

/// Why a viewer's input was not written.
#[derive(Debug)]
pub enum ViewerInputError {
    /// The viewer is not attached to this terminal (any more).
    NotAttached,
    /// The PTY could not be written.
    Io(std::io::Error),
}

#[derive(Clone, Copy)]
enum Signal {
    Term,
    Kill,
}

/// Which pid the initial hangup targets.
///
/// Always the spawned shell itself. `process_group` is only a fallback for a
/// platform where the child has no pid to report (where the signal is a no-op
/// anyway); it is never preferred, because `tcgetpgrp` can name a job the
/// shell started while it was booting.
fn hangup_pid(shell_pid: Option<i32>, process_group: Option<i32>) -> Option<i32> {
    shell_pid.or(process_group)
}

/// Sends SIGHUP to the shell process itself.
///
/// `pid` is the spawned child's pid, not the cached foreground process group:
/// a shell that starts a job while it boots would otherwise see the hangup go
/// to that job. Returns whether a hangup was actually delivered, so Windows
/// (and a pid that already exited) can skip the close wait.
fn signal_shell(pid: i32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid, libc::SIGHUP) == 0 }
    }
    #[cfg(windows)]
    {
        let _ = pid;
        false
    }
}

/// Signals the shell's whole process group.
fn signal_group(pgid: i32, signal: Signal) {
    #[cfg(unix)]
    {
        let signal = match signal {
            Signal::Term => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
        };
        // Negative pid signals the whole group; ignore ESRCH after a clean exit.
        let _ = unsafe { libc::kill(-pgid, signal) };
    }
    #[cfg(windows)]
    {
        let _ = (pgid, signal);
    }
}

/// Locks a mutex, recovering from a panic in another thread.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Encodes one PTY output chunk for a viewer.
pub fn output_frame(stream_id: u32, chunk: &OutputChunk) -> Vec<u8> {
    BinaryFrame {
        kind: BinaryKind::PtyOutput,
        stream_id,
        seq: chunk.seq,
        payload: chunk.data.clone(),
    }
    .encode()
}

/// Encodes a snapshot frame; the header sequence is the sequence it replaces.
pub fn snapshot_frame(stream_id: u32, snapshot: &Snapshot) -> Vec<u8> {
    BinaryFrame {
        kind: BinaryKind::PtySnapshot,
        stream_id,
        seq: snapshot.seq,
        payload: snapshot.ansi.as_bytes().to_vec(),
    }
    .encode()
}

/// Sends one numbered chunk to a viewer and records its credit.
///
/// The outbound slot is reserved first, so the sent bound advances only once
/// the frame is guaranteed to be handed over: a blocked send leaves it
/// unchanged, and recording before the permit is used means an ack cannot
/// overtake the record.
pub async fn send_chunk(
    viewer: &Viewer,
    stream_id: u32,
    chunk: &OutputChunk,
) -> Result<(), mpsc::error::SendError<()>> {
    let permit = viewer.tx.reserve().await?;
    viewer.record_sent(chunk.seq, chunk.data.len());
    permit.send(Outbound::Binary(output_frame(stream_id, chunk)));
    Ok(())
}

/// Runs one viewer task until the terminal closes or the connection drops.
///
/// On a lagged broadcast the viewer drains to the live edge and receives a
/// fresh snapshot, which resets its credit window. This is the "snapshot when
/// it falls too far behind" half of ADR-007.
pub async fn run_viewer(
    session: Arc<Session>,
    viewer_id: ViewerId,
    viewer: Arc<Viewer>,
    mut output: broadcast::Receiver<Arc<OutputChunk>>,
    start_seq: u64,
) {
    let stream_id = session.stream_id();
    let mut last_seq = start_seq;
    loop {
        // Detaching wakes the task through `Viewer::close`; a viewer that was
        // closed before the loop started sees the flag here.
        if viewer.closed.load(Ordering::SeqCst) {
            break;
        }
        // Register for the detach wakeup before the closed check, so a close
        // between the check and `select!` cannot leave this task parked.
        let notified = viewer.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if viewer.closed.load(Ordering::SeqCst) {
            break;
        }
        tokio::select! {
            _ = &mut notified => {
                if viewer.closed.load(Ordering::SeqCst) {
                    break;
                }
            }
            received = output.recv() => {
                match received {
                    Ok(chunk) => {
                        if chunk.seq <= last_seq {
                            continue;
                        }
                        if !viewer.wait_for_credit(chunk.data.len()).await {
                            break;
                        }
                        if send_chunk(&viewer, stream_id, chunk.as_ref()).await.is_err() {
                            break;
                        }
                        last_seq = chunk.seq;
                    }
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        tracing::debug!(
                            terminal_id = %session.id,
                            missed,
                            "terminal viewer fell behind; sending a snapshot"
                        );
                        while let Ok(chunk) = output.try_recv() {
                            last_seq = last_seq.max(chunk.seq);
                        }
                        let snapshot = session.snapshot();
                        viewer.begin_resync();
                        let frame = snapshot_frame(stream_id, &snapshot);
                        if viewer.tx.send(Outbound::Binary(frame)).await.is_err() {
                            break;
                        }
                        // The viewer can only answer queries from raw output
                        // it receives after this snapshot.
                        viewer.record_snapshot_sent(snapshot.seq);
                        last_seq = snapshot.seq;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    session.remove_viewer_if(viewer_id, &viewer);
}

/// Atomic counter helper used by the manager for stream ids.
pub fn next_stream_id(counter: &AtomicU64) -> u32 {
    (counter.fetch_add(1, Ordering::SeqCst) + 1) as u32
}

impl Session {
    /// Last emitted sequence, for diagnostics and tests.
    pub fn last_seq(&self) -> u64 {
        lock(&self.screen).seq
    }

    /// Frees credit for one attached viewer.
    pub fn ack_viewer(&self, viewer_id: ViewerId, up_to: u64) {
        let viewer = lock(&self.viewers)
            .get(&viewer_id)
            .map(|entry| Arc::clone(&entry.viewer));
        if let Some(viewer) = viewer {
            viewer.ack(up_to);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn viewer() -> (Arc<Viewer>, mpsc::Receiver<Outbound>) {
        let (tx, rx) = mpsc::channel(4);
        (Arc::new(Viewer::new(tx)), rx)
    }

    #[tokio::test]
    async fn credit_blocks_at_the_window_then_opens_on_ack() {
        let (viewer, _rx) = viewer();
        for seq in 1..=u64::from(CREDIT_MAX_CHUNKS) {
            viewer.record_sent(seq, 1024);
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(50), viewer.wait_for_credit(1))
                .await
                .is_err(),
            "a full chunk window must block"
        );
        viewer.ack(u64::from(CREDIT_MAX_CHUNKS));
        assert!(viewer.wait_for_credit(1).await);
    }

    #[tokio::test]
    async fn byte_budget_also_closes_the_window() {
        let (viewer, _rx) = viewer();
        viewer.record_sent(1, CREDIT_MAX_BYTES as usize);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), viewer.wait_for_credit(1))
                .await
                .is_err(),
            "a full byte budget must block"
        );
        viewer.ack(1);
        assert!(viewer.wait_for_credit(1).await);
    }

    #[tokio::test]
    async fn an_ack_that_overtakes_the_sent_record_is_not_lost() {
        let (viewer, _rx) = viewer();
        // The receiver can acknowledge each frame before the viewer task gets
        // back from `tx.send` to record it; every chunk is then already free.
        for seq in 1..=u64::from(CREDIT_MAX_CHUNKS) * 3 {
            viewer.ack(seq);
            viewer.record_sent(seq, CREDIT_MAX_BYTES as usize);
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(50), viewer.wait_for_credit(1))
                .await
                .is_ok(),
            "pre-acknowledged chunks must not keep the window closed"
        );
    }

    #[tokio::test]
    async fn closing_a_viewer_releases_a_credit_wait() {
        let (viewer, _rx) = viewer();
        for seq in 1..=u64::from(CREDIT_MAX_CHUNKS) {
            viewer.record_sent(seq, 1024);
        }
        let waiting = Arc::clone(&viewer);
        let task = tokio::spawn(async move { waiting.wait_for_credit(1).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        viewer.close();
        assert!(!task.await.expect("task joins"));
    }

    #[tokio::test]
    async fn a_blocked_send_does_not_advance_the_sent_bound() {
        let (tx, mut rx) = mpsc::channel(1);
        let viewer = Arc::new(Viewer::new(tx.clone()));
        viewer.reset_credit(0);
        // Fill the outbound queue so the chunk cannot be handed over yet.
        tx.send(Outbound::Text("filler".into()))
            .await
            .expect("fill the queue");

        let chunk = Arc::new(OutputChunk {
            seq: 1,
            data: vec![b'x'; 4],
        });
        let sending = tokio::spawn({
            let viewer = Arc::clone(&viewer);
            let chunk = Arc::clone(&chunk);
            async move { send_chunk(&viewer, 1, &chunk).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            viewer.sent_through(),
            0,
            "a frame waiting for the outbound queue has not reached the viewer"
        );

        let first = rx.recv().await.expect("filler");
        assert!(matches!(first, Outbound::Text(_)));
        sending
            .await
            .expect("send task joins")
            .expect("send succeeds");
        assert_eq!(
            viewer.sent_through(),
            1,
            "the bound advances once the frame is handed over"
        );
        let _ = rx.recv().await;
    }

    #[test]
    fn a_viewer_that_attached_late_cannot_number_its_first_chunk() {
        let (viewer, _rx) = viewer();
        viewer.reset_credit(0);
        assert_eq!(viewer.answer_from(), 1, "a fresh viewer may answer chunk 1");
        viewer.reset_credit(5);
        assert_eq!(
            viewer.answer_from(),
            7,
            "a viewer that attached at position 5 first needs chunk 6"
        );
    }

    /// A child that was already reaped by `try_wait` and refuses a second
    /// `wait`, standing in for the close path's race with the reader thread.
    #[derive(Debug)]
    struct ReapedChild;

    impl portable_pty::ChildKiller for ReapedChild {
        fn kill(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
            Box::new(ReapedChild)
        }
    }

    impl portable_pty::Child for ReapedChild {
        fn try_wait(&mut self) -> std::io::Result<Option<portable_pty::ExitStatus>> {
            Ok(Some(portable_pty::ExitStatus::with_exit_code(7)))
        }

        fn wait(&mut self) -> std::io::Result<portable_pty::ExitStatus> {
            Err(std::io::Error::other("the child was already reaped"))
        }

        fn process_id(&self) -> Option<u32> {
            None
        }
    }

    #[tokio::test]
    async fn finish_reuses_a_status_captured_by_the_close_wait() {
        let pty = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open pty");
        let writer = pty.master.take_writer().expect("pty writer");
        let session = Session::new(
            TerminalId::new("term_reaped"),
            1,
            None,
            HostId::new("host_test"),
            "/tmp".into(),
            "/bin/sh".into(),
            80,
            24,
            pty.master,
            Box::new(ReapedChild),
            writer,
        );

        session
            .wait_for_shell_exit(Duration::from_millis(100))
            .await;
        session.finish();
        let exit = session.info().exit.expect("exit status");
        assert_eq!(
            exit.status, 7,
            "the remove path keeps the reaped status instead of reporting -1"
        );
    }
    #[test]
    fn stream_ids_start_at_one_and_increase() {
        let counter = AtomicU64::new(0);
        assert_eq!(next_stream_id(&counter), 1);
        assert_eq!(next_stream_id(&counter), 2);
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn the_hangup_targets_the_shell_not_the_cached_foreground_group() {
        // A booting shell can put a job in the foreground, so the cached group
        // can differ from the shell's pid; the hangup must still go to the
        // shell.
        assert_eq!(hangup_pid(Some(41), Some(42)), Some(41));
        assert_eq!(hangup_pid(Some(41), None), Some(41));
        assert_eq!(hangup_pid(None, Some(42)), Some(42));
        assert_eq!(hangup_pid(None, None), None);
    }

    fn attached_viewer(seq: u64) -> (Arc<Viewer>, mpsc::Receiver<Outbound>) {
        let (viewer, rx) = viewer();
        // A viewer that attached at position zero and was sent chunks through
        // `seq` may answer any of them.
        viewer.record_sent(seq, 1);
        (viewer, rx)
    }

    fn input_state(
        viewers: &[(ViewerId, Arc<Viewer>)],
    ) -> (
        Mutex<HashMap<ViewerId, ViewerEntry>>,
        Mutex<AnsweredQueries>,
    ) {
        let mut map = HashMap::new();
        for (id, viewer) in viewers {
            map.insert(
                *id,
                ViewerEntry {
                    viewer: Arc::clone(viewer),
                    cols: 80,
                    rows: 24,
                    reply_positions: true,
                    legacy_owner: false,
                },
            );
        }
        (Mutex::new(map), Mutex::new(AnsweredQueries::default()))
    }

    #[test]
    fn answers_are_identified_by_position_and_ordinal() {
        let mut answers = AnsweredQueries::default();
        assert!(answers.accept(3, 0), "the first answer in a chunk");
        assert!(!answers.accept(3, 0), "the same answer is a duplicate");
        assert!(answers.accept(3, 1), "another answer in the same chunk");
        assert!(answers.accept(5, 0), "an unanswered older chunk");
        assert!(!answers.accept(5, 0));
    }

    #[test]
    fn answered_memory_stays_bounded() {
        let mut answers = AnsweredQueries::default();
        for position in 1..=ANSWERED_CHUNKS as u64 {
            assert!(answers.accept(position, 0));
        }
        assert_eq!(answers.recent.len(), ANSWERED_CHUNKS);
        // One more chunk pushes the oldest out; its answers become old.
        assert!(answers.accept(ANSWERED_CHUNKS as u64 + 1, 0));
        assert_eq!(answers.recent.len(), ANSWERED_CHUNKS);
        assert!(!answers.accept(1, 0), "the evicted chunk is forgotten");
        assert!(answers.accept(ANSWERED_CHUNKS as u64 + 1, 1));
    }

    /// A writer that holds the first write until the test releases it, so a
    /// second connection's write can be observed waiting.
    struct GatedWriter {
        started: Option<std::sync::mpsc::Sender<()>>,
        release: std::sync::mpsc::Receiver<()>,
        written: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for GatedWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if let Some(started) = self.started.take() {
                let _ = started.send(());
                let _ = self.release.recv();
            }
            lock(&self.written).extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn acceptance_and_write_are_one_critical_section() {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (second_started_tx, second_started_rx) = std::sync::mpsc::channel();
        let written = Arc::new(Mutex::new(Vec::new()));
        let writer: Mutex<Option<Box<dyn Write + Send>>> =
            Mutex::new(Some(Box::new(GatedWriter {
                started: Some(started_tx),
                release: release_rx,
                written: Arc::clone(&written),
            })));
        let (first_viewer, _first_rx) = attached_viewer(10);
        let (second_viewer, _second_rx) = attached_viewer(10);
        let (viewers, answers) = input_state(&[(1, first_viewer), (2, second_viewer)]);

        std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                write_input_locked(
                    &writer,
                    &viewers,
                    &answers,
                    1,
                    b"3",
                    QueryReply::At {
                        position: 3,
                        ordinal: 0,
                    },
                )
            });
            started_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the first write must start");

            let second = scope.spawn(|| {
                let _ = second_started_tx.send(());
                write_input_locked(
                    &writer,
                    &viewers,
                    &answers,
                    2,
                    b"4",
                    QueryReply::At {
                        position: 4,
                        ordinal: 0,
                    },
                )
            });
            second_started_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the second write must start");
            std::thread::sleep(Duration::from_millis(100));
            assert!(
                lock(&written).is_empty(),
                "the second write must wait for the first"
            );
            assert!(
                !lock(&answers).answered(4, 0),
                "a reply still waiting for the writer must not enter the answered record"
            );

            release_tx.send(()).expect("release the first write");
            first.join().expect("first thread").expect("first write");
            second.join().expect("second thread").expect("second write");
        });

        assert_eq!(&*lock(&written), b"34", "writes keep the accepted order");
        assert!(
            lock(&answers).answered(4, 0),
            "the released reply is recorded once it is written"
        );
    }
}
