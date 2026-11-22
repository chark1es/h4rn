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
use portable_pty::{Child, ChildKiller, MasterPty, PtySize};
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
    /// chunk. A query that started in a chunk the snapshot already folded in
    /// is invisible to this parser, so it never asks for that query's answer;
    /// its own answers name the chunk where their query started, which every
    /// other viewer that parsed that chunk also names.
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
    /// A chunk at or below the acked position was already sent and freed, so
    /// recording it again would consume credit that no future ack frees.
    pub fn record_sent(&self, seq: u64, bytes: usize) {
        let mut credit = lock(&self.credit);
        credit.sent_through = credit.sent_through.max(seq);
        if seq <= credit.acked_through {
            return;
        }
        credit.outstanding.push_back((seq, bytes));
        credit.bytes += bytes;
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
        // An ack can name output that was never sent: a stale or bogus
        // acknowledgement must not mark later chunks free, because nothing
        // would then account for their credit. Clamp it to the sent bound.
        let up_to = up_to.min(credit.sent_through);
        // Remember the position so a late record of the same chunk is skipped
        // instead of holding credit that no future ack would free.
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
#[derive(Debug, Default)]
pub struct Credit {
    outstanding: VecDeque<(u64, usize)>,
    bytes: usize,
    /// Latest output position sent to this viewer.
    sent_through: u64,
    /// Highest position the viewer acknowledged.
    ///
    /// An ack can overtake the send that records the same chunk, so this also
    /// tells a late record that the chunk is already accounted for.
    acked_through: u64,
}

impl Credit {
    /// Applies a snapshot at `seq`: it becomes the latest position, and the
    /// parser restarts at the next raw chunk.
    fn snapshot_at(&mut self, seq: u64) {
        self.sent_through = self.sent_through.max(seq);
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
    /// Whether this client names the output position on every marked reply
    /// with protocol 2 numbering: the chunk where the query **starts**.
    ///
    /// A client that does not declare it is legacy (an older build, or one
    /// that still numbers by the chunk where the query ended). While one is
    /// attached the terminal uses the legacy single-owner rule for every
    /// reply; only when every attached viewer declares positions is each
    /// answer decided by its `(position, ordinal)` through [`AnsweredQueries`].
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
    /// An independent killer for the shell.
    ///
    /// `finish` can hold the child lock inside `wait()` until the shell
    /// exits; the close path must still end the shell. The cloned killer owns
    /// its own process handle (on Windows) or pid, so it works while that lock
    /// is busy.
    killer: Mutex<Option<Box<dyn ChildKiller + Send + Sync>>>,
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
        // Keep a handle that can end the shell without taking the child lock,
        // which the reader thread may hold in `wait()` until the shell exits.
        let killer = child.clone_killer();
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
            killer: Mutex::new(Some(killer)),
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
            // the real status. It stores that status while still holding the
            // child lock, so taking both locks in the same order (child, then
            // captured status) makes the captured status visible here and
            // closes the race where the child is reaped between the two steps.
            let mut child = lock(&self.child);
            let captured = lock(&self.reaped_status).take();
            match captured {
                Some(status) => Some(status),
                None => child.wait().ok(),
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
            let previous = viewers.insert(
                id,
                ViewerEntry {
                    viewer,
                    cols,
                    rows,
                    reply_positions,
                    legacy_owner: false,
                },
            );
            // Attaching a legacy client must not take the reply role away from
            // an owner that is still attached: a viewer may attach while an
            // answer is in flight. The role moves only when its owner is gone
            // (including a replaced viewer) or legacy mode ends.
            let owner_replaced = previous.as_ref().is_some_and(|entry| entry.legacy_owner);
            let any_legacy = viewers.values().any(|entry| !entry.reply_positions);
            if owner_replaced || !any_legacy {
                clear_legacy_owners(&mut viewers);
            }
            previous
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
            // Dropping the entry may drop the legacy owner for replies with
            // no position. The role passes on only when its owner leaves or
            // when the last legacy client does; a legacy viewer attaching or
            // detaching elsewhere must not move it while a query is in flight.
            let removed = viewers.remove(&id);
            if let Some(entry) = &removed {
                let owner_gone = entry.legacy_owner;
                let legacy_gone = !viewers.values().any(|entry| !entry.reply_positions);
                if owner_gone || legacy_gone {
                    clear_legacy_owners(&mut viewers);
                }
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
    /// that names the output chunk where its query started (and the query's
    /// index in that chunk) is written by the first viewer to send it, from
    /// any viewer; an exact duplicate, or an answer for a chunk the record has
    /// forgotten, is dropped. The identity is a property of the output stream,
    /// so a viewer that stalled for a while names answers exactly as one that
    /// kept up. A position the daemon never sent to that viewer is ignored, so
    /// a bogus claim cannot suppress a real answer.
    ///
    /// The acceptance decision, the PTY write and the record update happen
    /// under the same writer lock: two connections cannot be accepted in one
    /// order and have their bytes reach the shell in another, and a write that
    /// fails records nothing.
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
        // The reader thread may be holding the child in `wait()` after a PTY
        // EOF; waiting for that lock here would outlast the close deadline.
        // The cloned killer owns its own process handle, so it still ends the
        // shell (the only way on Windows), and the pid signals below need no
        // handle at all.
        match try_lock(&self.child) {
            Some(mut child) => {
                // A handle kill that fails (the child was already reaped, or
                // Windows refused it) gets the same fallback as a busy lock.
                if child.kill().is_err() {
                    kill_through_handle(&self.killer);
                    signal_pid(self.shell_pid, Signal::Kill);
                }
            }
            None => {
                // On Unix the handle kill is SIGHUP, which a shell may catch
                // and ignore; the SIGKILL by pid below is the hard stop there.
                // On Windows the cloned handle is the TerminateProcess call.
                kill_through_handle(&self.killer);
                signal_pid(self.shell_pid, Signal::Kill);
            }
        }
        if let Some(pgid) = self.process_group {
            signal_group(pgid, Signal::Kill);
        }
    }

    /// Waits until the shell exits, or `limit` passes.
    ///
    /// `try_wait` reaps the shell when it has exited; the status is stored
    /// while the child lock is still held so `finish` can never see the child
    /// as reaped without also seeing its status. Both take the child lock
    /// before the captured-status lock.
    ///
    /// The child lock is only ever tried, never waited for: after a PTY EOF
    /// the reader thread holds it in `wait()` until the shell exits, and a
    /// blocking lock here would let that shell hold up the whole close. A
    /// shell that is busy being reaped is simply still running as far as the
    /// deadline is concerned, so the caller moves on to TERM and KILL.
    async fn wait_for_shell_exit(&self, limit: Duration) {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(mut child) = try_lock(&self.child) {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        *lock(&self.reaped_status) = Some(status);
                        return;
                    }
                    Err(_) => return,
                    Ok(None) => {}
                }
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
/// The lock covers the acceptance decision, the write and the record update,
/// so two connections cannot be accepted in one order and have their bytes
/// reach the shell in another. The record changes only after the bytes are
/// written: a failed write must leave no answer claimed and no legacy owner,
/// or the shell never sees the answer and no viewer may send it again.
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
    let accepted = if reply == QueryReply::No {
        None
    } else {
        match accepts_reply(viewers, answers, viewer_id, &viewer, reply) {
            Some(accepted) => Some(accepted),
            None => return Ok(()),
        }
    };
    writer.write_all(data).map_err(ViewerInputError::Io)?;
    writer.flush().map_err(ViewerInputError::Io)?;
    if let Some(accepted) = accepted {
        commit_reply(viewers, answers, accepted);
    }
    Ok(())
}

/// A reply the arbitration accepted, committed once it is written.
struct AcceptedReply {
    /// Viewer whose reply it is.
    viewer: ViewerId,
    /// Whether writing it makes this viewer the legacy reply owner.
    legacy_owner: bool,
    /// The `(position, ordinal)` of the answer, when it names one.
    answer: Option<(u64, u64)>,
}

/// Records an accepted reply after its bytes reached the shell.
fn commit_reply(
    viewers: &Mutex<HashMap<ViewerId, ViewerEntry>>,
    answers: &Mutex<AnsweredQueries>,
    accepted: AcceptedReply,
) {
    if let Some((position, ordinal)) = accepted.answer {
        lock(answers).accept(position, ordinal);
    }
    if accepted.legacy_owner
        && let Some(entry) = lock(viewers).get_mut(&accepted.viewer)
    {
        entry.legacy_owner = true;
    }
}

/// Decides whether one marked reply may be written.
///
/// The caller holds the writer lock, so the decision is not reordered against
/// another connection's write. Nothing is recorded here: a reply is only
/// committed by [`commit_reply`] once its bytes are written.
fn accepts_reply(
    viewers: &Mutex<HashMap<ViewerId, ViewerEntry>>,
    answers: &Mutex<AnsweredQueries>,
    viewer_id: ViewerId,
    viewer: &Viewer,
    reply: QueryReply,
) -> Option<AcceptedReply> {
    let latest = viewer.sent_through();
    let viewers = lock(viewers);
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
        if let Some(owner) = owner
            && owner != viewer_id
        {
            return None;
        }
        let answer = match reply {
            QueryReply::No | QueryReply::Unpositioned => None,
            QueryReply::At { position, ordinal } => {
                if !accept_position(answers, position, ordinal, latest) {
                    return None;
                }
                Some((position, ordinal))
            }
        };
        // Only a reply that passed validation and is actually written takes
        // the role. A bogus claim must not suppress the old client's next
        // valid reply, and ownership is not reset by a legacy viewer merely
        // attaching or detaching.
        return Some(AcceptedReply {
            viewer: viewer_id,
            legacy_owner: owner.is_none(),
            answer,
        });
    }
    match reply {
        QueryReply::No => None,
        // Every attached viewer declared positions, so an unpositioned reply
        // breaks that contract and names no answer: writing it could duplicate
        // a query another viewer already answered.
        QueryReply::Unpositioned => None,
        QueryReply::At { position, ordinal } => {
            if !accept_position(answers, position, ordinal, latest) {
                return None;
            }
            Some(AcceptedReply {
                viewer: viewer_id,
                legacy_owner: false,
                answer: Some((position, ordinal)),
            })
        }
    }
}

/// Clears every legacy reply owner.
fn clear_legacy_owners(viewers: &mut HashMap<ViewerId, ViewerEntry>) {
    for entry in viewers.values_mut() {
        entry.legacy_owner = false;
    }
}

/// Records an answer for `(position, ordinal)` if it is new.
///
/// The identity comes from the output stream, not from the viewer: `position`
/// is the chunk that carried the query's first byte and `ordinal` counts
/// queries that start in that chunk, so every viewer that parsed that chunk
/// names the same answer the same way. A viewer that attached with the
/// query's first byte already folded into its snapshot never saw the query
/// and cannot answer it. A reply naming output this viewer was never sent is
/// still ignored, so a bogus claim cannot suppress a real answer. A rejected
/// reply is dropped without advancing the record, so it cannot suppress the
/// answer of a viewer that really saw the query.
fn accept_position(
    answers: &Mutex<AnsweredQueries>,
    position: u64,
    ordinal: u64,
    latest: u64,
) -> bool {
    if position == 0 || position > latest {
        return false;
    }
    // Checked here and recorded after the write (`commit_reply`); the writer
    // lock makes the two steps one decision.
    !lock(answers).is_answered(position, ordinal)
}

/// Which query a reply answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryReply {
    /// Typed input, not a reply.
    No,
    /// A reply from a client that does not name the output position.
    Unpositioned,
    /// A reply to one query in an output chunk.
    At {
        /// Output chunk that carried the query's first byte.
        position: u64,
        /// The query's index among the queries that start in that chunk.
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
            // The incoming position competes for retirement too. A late reply
            // for a chunk older than everything tracked must not push the
            // newest tracked chunk out; retiring the incoming one treats its
            // answers as forgotten, exactly like a chunk that aged out.
            if self.recent.len() >= ANSWERED_CHUNKS
                && let Some((oldest, _)) = self.recent.first_key_value()
                && position <= *oldest
            {
                self.through = self.through.max(position);
                return false;
            }
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
    fn is_answered(&self, position: u64, ordinal: u64) -> bool {
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

/// Signals one process directly, without the child handle.
///
/// The close path uses this when the reader thread holds the child lock: the
/// signal must go out on schedule, not after the shell decides to exit.
fn signal_pid(pid: Option<i32>, signal: Signal) {
    #[cfg(unix)]
    {
        let signal = match signal {
            Signal::Term => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
        };
        // Ignore ESRCH for a shell that already exited.
        if let Some(pid) = pid {
            let _ = unsafe { libc::kill(pid, signal) };
        }
    }
    #[cfg(windows)]
    {
        let _ = (pid, signal);
    }
}

/// Kills the shell through the cloned killer, without the child lock.
///
/// On Windows the cloned killer holds a duplicated process handle and calls
/// `TerminateProcess`, which is what ends a ConPTY shell when `finish` holds
/// the child in `wait()`. It never waits for the child lock, so a busy reader
/// thread cannot delay it. Returns whether a killer was available.
fn kill_through_handle(killer: &Mutex<Option<Box<dyn ChildKiller + Send + Sync>>>) -> bool {
    match lock(killer).as_mut() {
        Some(killer) => killer.kill().is_ok(),
        None => false,
    }
}

/// Locks a mutex, recovering from a panic in another thread.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Tries to lock a mutex, recovering from a panic in another thread.
///
/// `None` means another thread holds it; the caller must not block, because
/// the holder may be waiting on something this path is about to signal.
fn try_lock<T>(mutex: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    match mutex.try_lock() {
        Ok(guard) => Some(guard),
        Err(std::sync::TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
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
    async fn an_ack_beyond_the_sent_bound_cannot_open_the_window() {
        let (viewer, _rx) = viewer();
        viewer.record_sent(1, 1024);
        // A stale or bogus ack names output the daemon never sent.
        viewer.ack(u64::MAX);
        let window = u64::from(CREDIT_MAX_CHUNKS);
        for seq in 2..=window + 1 {
            viewer.record_sent(seq, 1024);
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(50), viewer.wait_for_credit(1))
                .await
                .is_err(),
            "chunks sent after a bogus ack must still consume credit"
        );
        viewer.ack(window + 1);
        assert!(viewer.wait_for_credit(1).await);

        // A resync clears the window; chunks after it must still count.
        viewer.begin_resync();
        viewer.record_snapshot_sent(window + 10);
        viewer.record_sent(window + 11, CREDIT_MAX_BYTES as usize);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), viewer.wait_for_credit(1))
                .await
                .is_err(),
            "a chunk after a resync must still consume credit"
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
    fn a_snapshot_advances_the_sent_bound_but_not_a_floor() {
        let (viewer, _rx) = viewer();
        viewer.reset_credit(0);
        assert_eq!(viewer.sent_through(), 0);
        viewer.reset_credit(5);
        assert_eq!(
            viewer.sent_through(),
            5,
            "a snapshot at 5 is the latest position the viewer received"
        );
        viewer.record_sent(6, 1);
        assert_eq!(
            viewer.sent_through(),
            6,
            "the first raw chunk after the snapshot is sendable and answerable"
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

        #[cfg(windows)]
        fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
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

    /// A child that reaps and then keeps the `try_wait` caller (and the child
    /// lock) busy long enough for the test to start `finish` in between.
    #[derive(Debug)]
    struct SlowReapChild {
        reaping: Option<std::sync::mpsc::Sender<()>>,
        status: u32,
    }

    impl portable_pty::ChildKiller for SlowReapChild {
        fn kill(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
            Box::new(SlowReapChild {
                reaping: None,
                status: self.status,
            })
        }
    }

    impl portable_pty::Child for SlowReapChild {
        fn try_wait(&mut self) -> std::io::Result<Option<portable_pty::ExitStatus>> {
            if let Some(reaping) = self.reaping.take() {
                let _ = reaping.send(());
                // Finish starts while this call still holds the child lock;
                // without the captured status it would then wait again.
                std::thread::sleep(Duration::from_millis(150));
            }
            Ok(Some(portable_pty::ExitStatus::with_exit_code(self.status)))
        }

        fn wait(&mut self) -> std::io::Result<portable_pty::ExitStatus> {
            Err(std::io::Error::other("the child was already reaped"))
        }

        fn process_id(&self) -> Option<u32> {
            None
        }

        #[cfg(windows)]
        fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
            None
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn finish_sees_the_status_while_the_close_wait_is_still_reaping() {
        let pty = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open pty");
        let writer = pty.master.take_writer().expect("pty writer");
        let (reaping_tx, reaping_rx) = std::sync::mpsc::channel();
        let session = Arc::new(Session::new(
            TerminalId::new("term_interleaved"),
            1,
            None,
            HostId::new("host_test"),
            "/tmp".into(),
            "/bin/sh".into(),
            80,
            24,
            pty.master,
            Box::new(SlowReapChild {
                reaping: Some(reaping_tx),
                status: 9,
            }),
            writer,
        ));

        let waiting = tokio::spawn({
            let session = Arc::clone(&session);
            async move {
                session.wait_for_shell_exit(Duration::from_secs(2)).await;
            }
        });
        reaping_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the close wait must reach try_wait");

        // The reader thread's exit report now runs while the close wait still
        // holds the child lock. It must block until the status is stored and
        // then report it, not race ahead and lose it to a failed second wait.
        session.finish();
        waiting.await.expect("close wait joins");

        let exit = session.info().exit.expect("exit status");
        assert_eq!(
            exit.status, 9,
            "finish must see the status reaped by the close wait"
        );
    }
    /// A child that closes its stdio (so the master sees EOF) and then keeps
    /// running while ignoring SIGHUP and SIGTERM.
    ///
    /// The EOF makes the reader thread call `finish`, which holds the child
    /// lock inside `wait()` until the process ends. Closing the terminal must
    /// still send TERM and then KILL on schedule instead of waiting for a lock
    /// that the ignored hangup can never release.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_does_not_wait_for_a_shell_that_holds_the_child_lock() {
        use std::time::Instant;

        let pty = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open pty");
        let mut command = portable_pty::CommandBuilder::new("/bin/sh");
        command.args(["-c", "trap '' HUP TERM; exec 0<&- 1>&- 2>&-; exec sleep 30"]);
        let child = pty.slave.spawn_command(command).expect("spawn child");
        // The parent's slave handle must close so the master sees EOF when the
        // child closes its own stdio (as `spawn_shell` does in production).
        drop(pty.slave);
        let reader = pty.master.try_clone_reader().expect("pty reader");
        let writer = pty.master.take_writer().expect("pty writer");
        let session = Arc::new(Session::new(
            TerminalId::new("term_hup_ignored"),
            1,
            None,
            HostId::new("host_test"),
            "/tmp".into(),
            "/bin/sh".into(),
            80,
            24,
            pty.master,
            child,
            writer,
        ));
        session.start_reader(reader);

        // Wait until the reader thread's `finish` holds the child in `wait`
        // and can no longer release it before the close path runs.
        let deadline = Instant::now() + Duration::from_secs(5);
        while session.child.try_lock().is_ok() {
            assert!(
                Instant::now() < deadline,
                "the PTY EOF must let the reader thread reach the child wait"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // The hangup is ignored, so the wait lasts its whole bound; the TERM
        // grace follows. The child lock must add nothing on top. A blocking
        // lock would never be released here, because only this path kills the
        // shell, so a watchdog ends it shortly after the deadline and the
        // wall-clock assertion below fails instead of hanging the test.
        //
        // The watchdog waits on a completion signal rather than sleeping a
        // fixed time, and it only signals a pid while the session still says
        // the shell is alive: once the close reaps the child, its pid may be
        // reused, and killing it then would hit an unrelated process.
        let bound = HANGUP_GRACE + TERM_GRACE + Duration::from_millis(500);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let watchdog = {
            let session = Arc::clone(&session);
            std::thread::spawn(move || {
                if done_rx
                    .recv_timeout(bound + Duration::from_millis(500))
                    .is_ok()
                {
                    return false;
                }
                if !session.is_alive() {
                    return false;
                }
                if let Some(pid) = session.shell_pid {
                    // SAFETY: the session still reports the shell as alive, so
                    // the pid cannot have been reaped and reused; ESRCH after
                    // an exit that races this check is ignored.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
                true
            })
        };

        let started = Instant::now();
        session.kill_with_grace().await;
        let elapsed = started.elapsed();
        let _ = done_tx.send(());
        let fired = watchdog.join().expect("watchdog joins");
        assert!(!fired, "the close path must finish without the watchdog");
        assert!(
            elapsed < bound,
            "close must not wait for the child lock the reader thread holds (took {elapsed:?})"
        );

        // The pid/group signals reached the shell, so the reader thread's
        // `wait` returns and the exit report is written.
        let deadline = Instant::now() + Duration::from_secs(5);
        while session.info().exit.is_none() {
            assert!(Instant::now() < deadline, "the shell must be killed");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Closing a console on Windows ends it even when the reader thread's
    /// `finish` holds the child in `wait()` and the handle kill cannot run.
    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_ends_a_windows_console() {
        use std::time::Instant;

        let pty = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open pty");
        let mut command = portable_pty::CommandBuilder::new("ping.exe");
        command.args(["-n", "60", "127.0.0.1"]);
        let child = pty.slave.spawn_command(command).expect("spawn child");
        drop(pty.slave);
        let reader = pty.master.try_clone_reader().expect("pty reader");
        let writer = pty.master.take_writer().expect("pty writer");
        let session = Arc::new(Session::new(
            TerminalId::new("term_windows_close"),
            1,
            None,
            HostId::new("host_test"),
            "C:\\".into(),
            "cmd.exe".into(),
            80,
            24,
            pty.master,
            child,
            writer,
        ));
        session.start_reader(reader);

        // Hold the child handle the way `finish` does inside `wait()`, so the
        // handle kill cannot run: only the cloned killer can end the console.
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = {
            let session = Arc::clone(&session);
            std::thread::spawn(move || {
                let _held = lock(&session.child);
                let _ = release_rx.recv_timeout(Duration::from_secs(10));
            })
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while session.child.try_lock().is_ok() {
            assert!(Instant::now() < deadline, "the child lock must be held");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // The close path has no hangup wait on Windows; it must end the
        // process through the cloned handle without waiting for the child.
        let started = Instant::now();
        tokio::time::timeout(Duration::from_secs(5), session.kill_with_grace())
            .await
            .expect("close must not wait for the child handle");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "closing the console must not block"
        );

        let _ = release_tx.send(());
        holder.join().expect("the lock holder joins");

        // ConPTY keeps the console's output pipe open until the pseudoconsole
        // is closed, so the reader never reaches EOF here and no exit notice is
        // written. The child handle is what proves the cloned killer ended the
        // console process, which is the close contract on Windows.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let exited = {
                let mut child = lock(&session.child);
                child.try_wait().ok().flatten()
            };
            if exited.is_some() {
                break;
            }
            assert!(Instant::now() < deadline, "the console must be terminated");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
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

    #[test]
    fn a_late_reply_never_evicts_a_newer_chunk() {
        let mut answers = AnsweredQueries::default();
        // Fill the window with chunks 100..=163.
        for position in 100..100 + ANSWERED_CHUNKS as u64 {
            assert!(answers.accept(position, 0));
        }
        // A reply for chunk 99 arrives late. It is the oldest candidate, so it
        // is the one retired; the tracked chunks stay.
        assert!(
            !answers.accept(99, 0),
            "the incoming older chunk is the one that ages out"
        );
        assert!(answers.is_answered(100, 0), "chunk 100 stays recorded");
        assert!(
            !answers.accept(100, 0),
            "a still-tracked chunk's answer is not written twice"
        );
        assert!(
            answers.accept(100, 1),
            "chunk 100's other answers are still tracked"
        );
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
            // Every step whose failure would leave a writer parked on the gate
            // is recorded, not asserted yet: the gate is released and both
            // writers are joined before anything is checked.
            let started = started_rx.recv_timeout(Duration::from_secs(5));

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
            let second_started = second_started_rx.recv_timeout(Duration::from_secs(5));
            if started.is_ok() && second_started.is_ok() {
                std::thread::sleep(Duration::from_millis(100));
            }
            // Observe only; the writers are released and joined before any of
            // this is asserted, so a failure cannot leave a thread parked.
            let second_wrote = !lock(&written).is_empty();
            let waiting_recorded = lock(&answers).is_answered(4, 0);

            let _ = release_tx.send(());
            let first = first.join();
            let second = second.join();

            started.expect("the first write must start");
            second_started.expect("the second write must start");
            first.expect("first thread").expect("first write");
            second.expect("second thread").expect("second write");

            assert!(!second_wrote, "the second write must wait for the first");
            assert!(
                !waiting_recorded,
                "a reply still waiting for the writer must not enter the answered record"
            );
        });

        assert_eq!(&*lock(&written), b"34", "writes keep the accepted order");
        assert!(
            lock(&answers).is_answered(4, 0),
            "the released reply is recorded once it is written"
        );
    }

    /// A writer whose first write fails, then works.
    struct FailOnceWriter {
        failed: bool,
        written: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for FailOnceWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if !self.failed {
                self.failed = true;
                return Err(std::io::Error::other("the PTY write failed"));
            }
            lock(&self.written).extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn fail_once_writer(written: &Arc<Mutex<Vec<u8>>>) -> Mutex<Option<Box<dyn Write + Send>>> {
        Mutex::new(Some(Box::new(FailOnceWriter {
            failed: false,
            written: Arc::clone(written),
        })))
    }

    #[test]
    fn a_failed_write_does_not_record_the_answer() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let writer = fail_once_writer(&written);
        let (first_viewer, _first_rx) = attached_viewer(10);
        let (second_viewer, _second_rx) = attached_viewer(10);
        let (viewers, answers) = input_state(&[(1, first_viewer), (2, second_viewer)]);

        write_input_locked(
            &writer,
            &viewers,
            &answers,
            1,
            b"iFIRST",
            QueryReply::At {
                position: 3,
                ordinal: 0,
            },
        )
        .expect_err("the failing write is reported");
        assert!(
            !lock(&answers).is_answered(3, 0),
            "a failed write must not claim the answer"
        );

        write_input_locked(
            &writer,
            &viewers,
            &answers,
            2,
            b"iSECOND",
            QueryReply::At {
                position: 3,
                ordinal: 0,
            },
        )
        .expect("the other viewer's answer is written");
        assert_eq!(
            &*lock(&written),
            b"iSECOND",
            "the failed reply never reached the shell; the valid one did"
        );
        assert!(lock(&answers).is_answered(3, 0));
    }

    #[test]
    fn a_failed_write_does_not_take_the_legacy_owner() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let writer = fail_once_writer(&written);
        let (old_viewer, _old_rx) = attached_viewer(10);
        let (new_viewer, _new_rx) = attached_viewer(10);
        let (viewers, answers) = input_state(&[(1, old_viewer), (2, new_viewer)]);
        lock(&viewers)
            .get_mut(&1)
            .expect("the old viewer")
            .reply_positions = false;

        write_input_locked(
            &writer,
            &viewers,
            &answers,
            1,
            b"iOLD",
            QueryReply::Unpositioned,
        )
        .expect_err("the failing write is reported");
        assert!(
            !lock(&viewers).get(&1).expect("the old viewer").legacy_owner,
            "a failed write must not take the legacy owner role"
        );

        // The old viewer's valid answer would be dropped if the failed one had
        // taken ownership, leaving the query unanswered.
        write_input_locked(
            &writer,
            &viewers,
            &answers,
            2,
            b"iNEW",
            QueryReply::At {
                position: 3,
                ordinal: 0,
            },
        )
        .expect("the positioned viewer's answer is written");
        assert_eq!(&*lock(&written), b"iNEW");
        assert!(lock(&viewers).get(&2).expect("the new viewer").legacy_owner);
    }

    /// A writer that records every byte written to it.
    struct RecordingWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for RecordingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            lock(&self.0).extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn recording_writer(written: &Arc<Mutex<Vec<u8>>>) -> Mutex<Option<Box<dyn Write + Send>>> {
        Mutex::new(Some(Box::new(RecordingWriter(Arc::clone(written)))))
    }

    /// A viewer entry that attached at `snapshot` and was then sent `sent`.
    fn viewer_since_snapshot(snapshot: u64, sent: u64) -> (Arc<Viewer>, mpsc::Receiver<Outbound>) {
        let (viewer, rx) = viewer();
        viewer.reset_credit(snapshot);
        for seq in snapshot + 1..=sent {
            viewer.record_sent(seq, 1);
        }
        (viewer, rx)
    }

    /// A reply naming the chunk where its query started.
    fn at(position: u64, ordinal: u64) -> QueryReply {
        QueryReply::At { position, ordinal }
    }

    #[test]
    fn one_query_is_written_once_whatever_order_two_viewers_answer_in() {
        // Chunk 5 ends inside a query; chunk 6 finishes it and asks a second.
        // One viewer saw chunk 5 raw and the other attached with it folded
        // into its snapshot, so only the early viewer can name the first query.
        // The second query keeps the same identity for both.
        let replies: [(ViewerId, &[u8], QueryReply); 3] = [
            (2, b"Y", at(6, 0)),
            (1, b"X", at(5, 0)),
            (1, b"Y", at(6, 0)),
        ];
        for late_first in [false, true] {
            let written = Arc::new(Mutex::new(Vec::new()));
            let writer = recording_writer(&written);
            // The early viewer saw chunk 5 raw; the late one attached with it
            // in its snapshot. Both have chunk 6 by the time they answer, so
            // only the identities decide what is a duplicate.
            let (early, _early_rx) = attached_viewer(6);
            let (late, _late_rx) = viewer_since_snapshot(5, 6);
            let (viewers, answers) = input_state(&[(1, early), (2, late)]);

            // The late viewer's answer arrives either before the early
            // viewer's or after it; both orders must write each query once.
            let order: [usize; 3] = if late_first { [0, 1, 2] } else { [1, 2, 0] };
            for index in order {
                let (viewer, bytes, reply) = replies[index];
                write_input_locked(&writer, &viewers, &answers, viewer, bytes, reply)
                    .expect("the input is written");
            }

            let written = lock(&written);
            assert_eq!(
                written.len(),
                2,
                "one write per query; two viewers cannot duplicate an answer"
            );
            let text = std::str::from_utf8(&written).expect("ascii");
            assert_eq!(
                text,
                if late_first { "YX" } else { "XY" },
                "the first accepted answer to each query is the one written"
            );
        }
    }

    #[test]
    fn a_stalled_viewer_catching_up_cannot_answer_a_query_twice() {
        // The reproduction: chunk 5 ends with `ESC[?12`, chunk 6 is
        // `;2$p ESC[?25$p`. A stalls at 5; B attaches with 5 in its snapshot
        // and answers the chunk-6 query as `(6, 0)`. When A catches up it
        // answers the chunk-5 query as `(5, 0)` and the same chunk-6 query as
        // `(6, 0)` again, which the record must drop.
        let written = Arc::new(Mutex::new(Vec::new()));
        let writer = recording_writer(&written);
        let (stalled, _stalled_rx) = attached_viewer(5);
        let (attached, _attached_rx) = viewer_since_snapshot(5, 6);
        let (viewers, answers) = input_state(&[(1, stalled), (2, attached)]);

        write_input_locked(&writer, &viewers, &answers, 2, b"Y", at(6, 0))
            .expect("the attached viewer's answer is written");
        assert_eq!(&*lock(&written), b"Y");

        // A catches up with chunk 6 before answering.
        lock(&viewers)
            .get(&1)
            .expect("the stalled viewer")
            .viewer
            .record_sent(6, 1);
        write_input_locked(&writer, &viewers, &answers, 1, b"X", at(5, 0))
            .expect("the earlier query is still answered");
        write_input_locked(&writer, &viewers, &answers, 1, b"Y", at(6, 0))
            .expect("a duplicate is dropped without failing the connection");

        assert_eq!(
            &*lock(&written),
            b"YX",
            "every query is answered exactly once, in arrival order"
        );
    }

    #[test]
    fn a_resynced_owner_answers_its_first_chunk_in_mixed_mode() {
        // A legacy client cannot name positions, so the positioned viewer owns
        // replies while it is attached. After a resync its first raw chunk is
        // answerable: the answer's position is the chunk where its query
        // started, which the legacy viewer would not name at all.
        let written = Arc::new(Mutex::new(Vec::new()));
        let writer = recording_writer(&written);
        let (owner, _owner_rx) = viewer_since_snapshot(7, 8);
        let (legacy, _legacy_rx) = attached_viewer(8);
        let (viewers, answers) = input_state(&[(1, owner), (2, legacy)]);
        {
            let mut viewers = lock(&viewers);
            viewers
                .get_mut(&1)
                .expect("the positioned owner")
                .legacy_owner = true;
            viewers
                .get_mut(&2)
                .expect("the legacy viewer")
                .reply_positions = false;
        }

        write_input_locked(&writer, &viewers, &answers, 1, b"iOWN", at(8, 0))
            .expect("the positioned owner's first chunk after a resync is answerable");

        assert_eq!(&*lock(&written), b"iOWN");
        assert!(lock(&answers).is_answered(8, 0));
        assert!(
            lock(&viewers)
                .get(&1)
                .expect("the positioned owner")
                .legacy_owner,
            "the role stays with the viewer that wrote the answer"
        );
    }

    /// A killer that counts how often it was asked to end the shell.
    #[derive(Debug)]
    struct CountingKiller {
        kills: Arc<AtomicU64>,
    }

    impl portable_pty::ChildKiller for CountingKiller {
        fn kill(&mut self) -> std::io::Result<()> {
            self.kills.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(Self {
                kills: Arc::clone(&self.kills),
            })
        }
    }

    /// A child that never exits on its own and never reports a pid.
    #[derive(Debug)]
    struct IdleChild {
        kills: Arc<AtomicU64>,
    }

    impl portable_pty::ChildKiller for IdleChild {
        fn kill(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(CountingKiller {
                kills: Arc::clone(&self.kills),
            })
        }
    }

    impl portable_pty::Child for IdleChild {
        fn try_wait(&mut self) -> std::io::Result<Option<portable_pty::ExitStatus>> {
            Ok(None)
        }

        fn wait(&mut self) -> std::io::Result<portable_pty::ExitStatus> {
            Err(std::io::Error::other("the test child never exits"))
        }

        fn process_id(&self) -> Option<u32> {
            None
        }

        #[cfg(windows)]
        fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
            None
        }
    }

    /// A child whose handle kill fails, as an already reaped one does.
    #[derive(Debug)]
    struct UnkillableChild {
        kills: Arc<AtomicU64>,
    }

    impl portable_pty::ChildKiller for UnkillableChild {
        fn kill(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("the child handle refused the kill"))
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(CountingKiller {
                kills: Arc::clone(&self.kills),
            })
        }
    }

    impl portable_pty::Child for UnkillableChild {
        fn try_wait(&mut self) -> std::io::Result<Option<portable_pty::ExitStatus>> {
            Ok(None)
        }

        fn wait(&mut self) -> std::io::Result<portable_pty::ExitStatus> {
            Err(std::io::Error::other("the test child never exits"))
        }

        fn process_id(&self) -> Option<u32> {
            None
        }

        #[cfg(windows)]
        fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
            None
        }
    }

    #[test]
    fn the_independent_killer_ends_the_shell_without_the_child_lock() {
        let kills = Arc::new(AtomicU64::new(0));
        let killer: Mutex<Option<Box<dyn ChildKiller + Send + Sync>>> =
            Mutex::new(Some(Box::new(CountingKiller {
                kills: Arc::clone(&kills),
            })));

        assert!(kill_through_handle(&killer));
        assert_eq!(kills.load(Ordering::SeqCst), 1);
        let empty: Mutex<Option<Box<dyn ChildKiller + Send + Sync>>> = Mutex::new(None);
        assert!(!kill_through_handle(&empty));
    }

    #[tokio::test]
    async fn close_kills_the_shell_while_the_child_lock_is_busy() {
        use std::time::Instant;

        let pty = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open pty");
        let writer = pty.master.take_writer().expect("pty writer");
        let kills = Arc::new(AtomicU64::new(0));
        let session = Arc::new(Session::new(
            TerminalId::new("term_busy_child"),
            1,
            None,
            HostId::new("host_test"),
            "/tmp".into(),
            "/bin/sh".into(),
            80,
            24,
            pty.master,
            Box::new(IdleChild {
                kills: Arc::clone(&kills),
            }),
            writer,
        ));

        // Stand in for the reader thread: hold the child lock until released,
        // as `finish` does inside `wait()`.
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = {
            let session = Arc::clone(&session);
            std::thread::spawn(move || {
                let _held = lock(&session.child);
                let _ = release_rx.recv_timeout(Duration::from_secs(5));
            })
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while session.child.try_lock().is_ok() {
            assert!(Instant::now() < deadline, "the child lock must be held");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let started = Instant::now();
        session.kill_with_grace().await;
        let elapsed = started.elapsed();
        let _ = release_tx.send(());
        holder.join().expect("the lock holder joins");

        assert_eq!(
            kills.load(Ordering::SeqCst),
            1,
            "the cloned killer ends the shell while `finish` holds the child"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "close must not wait for the child lock (took {elapsed:?})"
        );
    }

    #[tokio::test]
    async fn a_failed_handle_kill_still_ends_the_shell_through_the_killer() {
        let pty = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open pty");
        let writer = pty.master.take_writer().expect("pty writer");
        let kills = Arc::new(AtomicU64::new(0));
        let session = Arc::new(Session::new(
            TerminalId::new("term_unkillable"),
            1,
            None,
            HostId::new("host_test"),
            "/tmp".into(),
            "/bin/sh".into(),
            80,
            24,
            pty.master,
            Box::new(UnkillableChild {
                kills: Arc::clone(&kills),
            }),
            writer,
        ));

        session.kill_with_grace().await;

        assert_eq!(
            kills.load(Ordering::SeqCst),
            1,
            "a handle kill that fails falls back to the independent killer"
        );
    }
}
