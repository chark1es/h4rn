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
    ///
    /// A chunk above the last snapshot bound is also recorded as raw: those
    /// are the bytes this viewer's parser saw as output, and only a position
    /// it parsed that way is answerable without asking another viewer.
    pub fn record_sent(&self, seq: u64, bytes: usize) {
        let mut credit = lock(&self.credit);
        credit.sent_through = credit.sent_through.max(seq);
        if seq > credit.snapshot_through {
            credit.record_raw(seq);
        }
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

    /// The last output position folded into a snapshot this viewer was handed.
    ///
    /// Positions at or below it that are not in [`Viewer::raw_contains`] only
    /// reached this viewer as rendered state; see [`Viewer::snapshot_through`].
    pub fn snapshot_through(&self) -> u64 {
        lock(&self.credit).snapshot_through
    }

    /// Whether this viewer was sent `seq` as that chunk's raw bytes.
    ///
    /// A chunk that reached this viewer only inside a snapshot is not raw. A
    /// chunk it parsed raw before a later snapshot stays raw: its parser saw
    /// those bytes, so it can still answer a query that started there.
    /// Positions above the last snapshot bound are raw by definition, because
    /// the only way to receive them is as output.
    pub fn raw_contains(&self, seq: u64) -> bool {
        lock(&self.credit).raw_contains(seq)
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
    /// Last output position folded into a snapshot this viewer was handed.
    ///
    /// A position at or below this and outside [`Credit::raw`] reached the
    /// viewer only as rendered state; see [`Viewer::snapshot_through`].
    snapshot_through: u64,
    /// Inclusive ranges of positions sent as raw output, oldest first.
    ///
    /// A range covers chunks this viewer's parser really saw. A snapshot
    /// replaces the bytes above the last raw position it folds in, but it does
    /// not unsee a chunk the viewer already parsed, so those ranges survive it:
    /// the exact rule is that a position is snapshot-only when it lies in the
    /// gap between the last raw position before each snapshot and that
    /// snapshot's sequence.
    raw: Vec<(u64, u64)>,
    /// Highest position the viewer acknowledged.
    ///
    /// An ack can overtake the send that records the same chunk, so this also
    /// tells a late record that the chunk is already accounted for.
    acked_through: u64,
}

/// How many raw ranges a viewer's credit remembers.
///
/// A stream that keeps up needs one range; each snapshot that skips output
/// starts another. The cap only matters for a viewer that resyncs hundreds of
/// times. Forgetting the oldest range makes those old raw positions look
/// snapshot-only for this viewer: its claim there is given up to another
/// viewer that has the position raw, or accepted when none does. Either way
/// the shared `(position, ordinal)` record still allows one write, so the
/// bound never causes a second answer.
const RAW_RANGES_MAX: usize = 32;

impl Credit {
    /// Applies a snapshot at `seq`: it becomes the latest position, and the
    /// parser restarts at the next raw chunk.
    ///
    /// Raw ranges are left alone: a chunk this viewer parsed before the
    /// snapshot stays parsed, and only the positions the snapshot replaced are
    /// snapshot-only.
    fn snapshot_at(&mut self, seq: u64) {
        self.sent_through = self.sent_through.max(seq);
        self.snapshot_through = self.snapshot_through.max(seq);
    }

    /// Records `seq` as raw output, merging it into the last range.
    fn record_raw(&mut self, seq: u64) {
        if let Some(last) = self.raw.last_mut() {
            if seq <= last.1 {
                return;
            }
            if seq == last.1 + 1 {
                last.1 = seq;
                return;
            }
        }
        if self.raw.len() >= RAW_RANGES_MAX {
            self.raw.remove(0);
        }
        self.raw.push((seq, seq));
    }

    /// Whether `seq` lies in a range this viewer received as raw output.
    fn raw_contains(&self, seq: u64) -> bool {
        self.raw
            .iter()
            .any(|(first, last)| (*first..=*last).contains(&seq))
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
    /// On Windows `finish` can hold the child lock inside `wait()` until the
    /// shell exits, and the close path must still end the shell. The cloned
    /// killer owns its own process handle (on Windows) or pid, so it works
    /// while that lock is busy. On Unix it is the fallback for a pid signal
    /// taken under the reap lock.
    killer: Mutex<Option<Box<dyn ChildKiller + Send + Sync>>>,
    /// The PTY master, dropped by the close path.
    ///
    /// On Windows a killed ConPTY client does not close the pseudoconsole: the
    /// master keeps it open and the reader thread (which holds the session
    /// alive) would block in `read` forever. Dropping the master closes the
    /// console, and the close path drops the writer with it, which releases
    /// the reader so `finish` can report the exit.
    pty: Mutex<Option<Box<dyn MasterPty + Send>>>,
    /// The spawned shell's own pid, for the initial hangup.
    ///
    /// `process_group` comes from `tcgetpgrp`, which reports whatever holds
    /// the terminal in the foreground; during startup that can be a job the
    /// shell already started, so the hangup must not use it.
    shell_pid: Option<i32>,
    process_group: Option<i32>,
    exit: Mutex<Option<TerminalExit>>,
    /// Whether the exit notice was already sent to the viewers.
    ///
    /// The close path may reap and publish the shell before the reader thread
    /// reaches its own reap, so both paths can find an exit to report; this
    /// keeps the notice to exactly one.
    exit_notified: AtomicBool,
    /// Status captured when the shell was reaped.
    ///
    /// `finish` runs on the reader thread and may reach `wait` after another
    /// path already reaped the child, so the captured status is reused instead
    /// of reporting the failed second wait as `-1`.
    reaped_status: Mutex<Option<portable_pty::ExitStatus>>,
    /// Serializes the real reap and every pid-targeted close signal.
    ///
    /// **Lock order: `reap` -> `child` -> `reaped_status` -> `exit`.** A
    /// blocking `waitid` never runs under this lock; only the reap and the
    /// exit publication do, and each pid-targeted signal does too.
    ///
    /// The shell's pid is only freed by a real reap (`wait`/`waitpid`). Every
    /// reap the session performs itself holds this lock and publishes the exit
    /// before releasing it, so a close that holds the lock and sees no
    /// published exit knows the pid is still this session's live process or
    /// its unreaped zombie, never a pid another process reused. (portable-pty's
    /// `child.kill()` polls `try_wait` internally; that call is itself part of
    /// the locked pid step, and no pid signal follows it.) The reader thread
    /// first waits for the shell **without reaping** it (`waitid` with
    /// `WNOWAIT`), so a close that signals during that wait reaches the
    /// zombie, not a freed pid.
    reap: Mutex<()>,
    /// Set while the reader thread waits for the shell without reaping it.
    ///
    /// A test hook: the close test must wait for that pause, which no longer
    /// holds the child lock.
    #[cfg(all(test, unix))]
    awaiting_shell_exit: AtomicBool,
    /// Where the close path sends its signals.
    ///
    /// Production uses [`SystemSignals`]; unit tests substitute a recorder so
    /// a close never signals a real pid.
    signals: Arc<dyn CloseSignals>,
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
            pty: Mutex::new(Some(master)),
            shell_pid,
            process_group,
            exit: Mutex::new(None),
            exit_notified: AtomicBool::new(false),
            reaped_status: Mutex::new(None),
            reap: Mutex::new(()),
            #[cfg(all(test, unix))]
            awaiting_shell_exit: AtomicBool::new(false),
            signals: Arc::new(SystemSignals),
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
        // Wait for the shell to exit **without reaping it**. The zombie keeps
        // its pid, so a close that runs between this wait and the reap below
        // may signal that pid and only ever reaches this session's own
        // process. `wait` frees the pid the moment it returns, which is why
        // the reap itself must be under the lock the close shares.
        #[cfg(unix)]
        self.await_shell_exit_without_reaping();
        // The reap and the exit publication are one step under the reap lock,
        // so no close can observe a freed pid as unpublished.
        match self.reap_and_publish() {
            Some(exit) => self.notify_exit_once(exit),
            None => {
                // A close reaped and published the shell first. It sends the
                // notice; this only covers the window where it has not yet.
                if let Some(exit) = lock(&self.exit).clone() {
                    self.notify_exit_once(exit);
                }
            }
        }
    }

    /// Waits until the shell exits while leaving it reapable (Unix).
    ///
    /// `waitid` with `WNOWAIT` is POSIX and exists on every Unix target h4rn
    /// builds (Linux and macOS), so no `pidfd` fallback is needed: it reports
    /// the exit and keeps the child a zombie until [`Session::reap_and_publish`]
    /// reaps it. `EINTR` is retried; `ECHILD` means nothing of ours is left to
    /// wait for (another path already reaped it, or the pid is not our child),
    /// so the reap step below sorts out the status.
    #[cfg(unix)]
    fn await_shell_exit_without_reaping(&self) {
        let Some(pid) = self
            .shell_pid
            .and_then(|pid| libc::pid_t::try_from(pid).ok())
        else {
            // A spawned Unix child always reports its pid; only test doubles
            // and the no-pid fallback reach the reap without this wait.
            return;
        };
        #[cfg(all(test, unix))]
        self.awaiting_shell_exit.store(true, Ordering::SeqCst);
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        loop {
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOWAIT,
                )
            };
            if result == 0 {
                break;
            }
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break;
        }
        #[cfg(all(test, unix))]
        self.awaiting_shell_exit.store(false, Ordering::SeqCst);
    }

    /// Reaps the shell and publishes its exit under the reap lock.
    ///
    /// Returns the exit it published, or `None` when another path published
    /// one first. Lock order: `reap` -> `child` -> `reaped_status` -> `exit`.
    fn reap_and_publish(&self) -> Option<TerminalExit> {
        let _reap = lock(&self.reap);
        if lock(&self.exit).is_some() {
            return None;
        }
        let status = {
            // A status captured by an earlier reap (the close path on Windows,
            // or a child that refuses a second `wait`) is reused, so the exit
            // report is not lost to a failed wait.
            let mut child = lock(&self.child);
            let captured = lock(&self.reaped_status).take();
            match captured {
                Some(status) => Some(status),
                None => child.wait().ok(),
            }
        };
        let exit = terminal_exit(status);
        *lock(&self.exit) = Some(exit.clone());
        Some(exit)
    }

    /// Reaps the shell if it has already exited, without blocking.
    ///
    /// The caller holds the reap lock and the child lock; the exit is
    /// published before the lock is released, so a later pid-targeted signal
    /// sees the confirmation and skips. Returns the published exit.
    #[cfg(unix)]
    fn reap_exited(&self, child: &mut Box<dyn Child + Send + Sync>) -> Option<TerminalExit> {
        if lock(&self.exit).is_some() {
            return None;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                *lock(&self.reaped_status) = Some(status.clone());
                let exit = terminal_exit(Some(status));
                *lock(&self.exit) = Some(exit.clone());
                Some(exit)
            }
            // Still running, or a wait that could not answer.
            Ok(None) | Err(_) => None,
        }
    }

    /// Sends the exit notice to attached viewers exactly once.
    fn notify_exit_once(&self, exit: TerminalExit) {
        if self.exit_notified.swap(true, Ordering::SeqCst) {
            return;
        }
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
        let Some(pty) = pty.as_ref() else {
            // The close path already dropped the master.
            return;
        };
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
    /// a bogus claim cannot suppress a real answer. A position the viewer only
    /// received inside a snapshot yields to another viewer that received it
    /// raw, but only when that viewer can actually write the answer: with a
    /// legacy client attached, the reply owner is the only one written, and a
    /// non-owner's copy must not leave the query unanswered. A snapshot-only
    /// position is still answerable when nobody else can write it, because
    /// xterm can complete the query with the snapshot's own bytes and the
    /// shell must still get one answer.
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
    /// handlers to clean up, then the handle kill and SIGKILL for the group.
    /// The hangup targets the spawned child rather than the cached foreground
    /// group: a shell that starts a job while it boots puts that job in the
    /// foreground, and the job would receive the hangup instead of the shell.
    ///
    /// Every pid-targeted signal — the opening hangup, the primary
    /// `child.kill()` (a SIGHUP by pid on Unix) and the pid fallbacks — runs
    /// under the [`Session::reap`] lock, after checking that the exit is not
    /// published. The shell's pid is only freed by a reap, and every reap
    /// publishes the exit under that same lock, so while a signal is sent the
    /// pid is still this session's live process or its unreaped zombie. The
    /// reader thread waits for the shell with `waitid(WNOWAIT)` before it
    /// reaps, so the pid cannot be reused while the close might signal it.
    /// The group TERM and KILL still go out on purpose after a reap: they end
    /// the shell's jobs, which can outlive it, and accept the small reuse risk
    /// of the group id (often the shell's own pid) for that.
    ///
    /// Waiting for the shell rather than a fixed delay is what makes the
    /// hangup reliable under load: an interactive shell forwards SIGHUP to its
    /// jobs before it exits, and a loaded machine can take longer than any
    /// fixed pause. The wait only runs when the platform actually delivered a
    /// hangup, so closing a terminal on Windows still takes no two seconds.
    /// Windows has no POSIX sessions or signals: the ConPTY closes when the PTY
    /// is dropped, which ends the console processes attached to it, and the
    /// SIGHUP/SIGTERM steps are no-ops there. Its handle kill owns a process
    /// handle rather than a bare pid, so it is untouched by this rule.
    pub async fn kill_with_grace(&self) {
        // Hang up first and wait, so the shell can run its hangup path and
        // its jobs hear SIGHUP; only then fall back to TERM and KILL for the
        // shell's group.
        let hung_up = hangup_pid(self.shell_pid, self.process_group)
            .and_then(|pid| self.signal_shell_pid(|| self.signals.hangup(pid)))
            .unwrap_or(false);
        if hung_up {
            self.wait_for_shell_exit(HANGUP_GRACE).await;
        }
        if let Some(pgid) = self.process_group {
            self.signals.group(pgid, Signal::Term);
        }
        // Keep the grace period after TERM so a job's cleanup handler can run
        // before the group is killed.
        tokio::time::sleep(TERM_GRACE).await;
        // The cloned killer holds its own process handle on Windows and its
        // own pid on Unix, so the close never waits for the child lock the
        // reader thread may take inside `finish`'s reap.
        self.end_shell();
        if let Some(pgid) = self.process_group {
            self.signals.group(pgid, Signal::Kill);
        }
        // The master must go too. On Windows the killed ConPTY client leaves
        // the pseudoconsole open, so the reader thread would stay in `read`
        // and keep this session alive; closing the master ends the console and
        // lets `finish` report the exit. On Unix it releases a blocked master
        // read after the kill.
        self.close_pty();
    }

    /// Sends one pid-targeted signal while holding the reap lock.
    ///
    /// Returns `None` when the exit is already published: the pid may belong
    /// to another process now, so nothing may be sent to it. Otherwise the
    /// lock is held across the signal, and no reap can free the pid until the
    /// signal has gone out.
    ///
    /// Lock order: `reap` -> `exit`. The caller must not hold the child lock.
    fn signal_shell_pid(&self, signal: impl FnOnce() -> bool) -> Option<bool> {
        #[cfg(unix)]
        {
            let _reap = lock(&self.reap);
            if lock(&self.exit).is_some() {
                return None;
            }
            Some(signal())
        }
        #[cfg(not(unix))]
        {
            // Windows: the handle kill owns a process handle, and the pid
            // signals are no-ops, so there is no pid reuse to guard against.
            Some(signal())
        }
    }

    /// Ends the shell after the grace period.
    ///
    /// On Unix the primary kill and its pid fallback are one critical section
    /// against the reap: no reap can run between them, and a shell that already
    /// exited is reaped and published here instead of signalled.
    fn end_shell(&self) {
        #[cfg(windows)]
        {
            // A handle kill that fails (the child was already reaped, or
            // Windows refused it) gets the same fallback as a busy lock.
            match try_lock(&self.child) {
                Some(mut child) => {
                    if child.kill().is_err() {
                        self.kill_after_busy_handle();
                    }
                }
                None => self.kill_after_busy_handle(),
            }
        }
        #[cfg(unix)]
        {
            let published = {
                let _reap = lock(&self.reap);
                if lock(&self.exit).is_some() {
                    None
                } else {
                    // Reap a shell that has already exited so its pid is not
                    // signalled and its status is kept for the exit report.
                    let mut child = try_lock(&self.child);
                    let reaped = child.as_mut().and_then(|child| self.reap_exited(child));
                    if reaped.is_none() {
                        // Still running (or the lock is busy): the pid is safe
                        // to signal because no reap can run without this lock.
                        //
                        // portable-pty's `child.kill()` is a SIGHUP by pid on
                        // Unix and polls `try_wait` for a moment inside the
                        // call; that poll also runs under this lock, so even a
                        // shell it reaps there cannot be signalled by a later
                        // pid-targeted step. The fallback stays in the same
                        // critical section.
                        let kill_failed = match child.as_mut() {
                            Some(child) => child.kill().is_err(),
                            None => true,
                        };
                        if kill_failed {
                            // On Unix the handle kill is SIGHUP, which a shell
                            // may catch and ignore; the SIGKILL by pid below is
                            // the hard stop there.
                            kill_through_handle(&self.killer);
                            self.signals.pid(self.shell_pid, Signal::Kill);
                        }
                    }
                    reaped
                }
            };
            if let Some(exit) = published {
                self.notify_exit_once(exit);
            }
        }
    }

    /// Ends the shell on Windows when the child handle is busy or refused the
    /// kill.
    ///
    /// The handle kill owns a duplicated process handle and cannot signal an
    /// unrelated process, so it is kept even after a reap. Only Windows uses
    /// this; on Unix the pid-targeted path is guarded by the reap lock.
    #[cfg(windows)]
    fn kill_after_busy_handle(&self) {
        let reaped = lock(&self.reaped_status).is_some() || !self.is_alive();
        kill_through_handle(&self.killer);
        if !reaped {
            self.signals.pid(self.shell_pid, Signal::Kill);
        }
    }

    /// Drops the PTY master and writer, which close the pipes they own.
    ///
    /// On Windows a killed ConPTY client does not close the pseudoconsole by
    /// itself: the master keeps it open and the reader thread, which holds the
    /// session alive, would block in `read` forever. Dropping the master closes
    /// the console, and dropping the writer closes the input pipe the close path
    /// no longer needs; both ends must go for the console host to finish and the
    /// blocked read to return, so `finish` can report the exit. On Unix the drop
    /// releases a blocked master read after the kill.
    fn close_pty(&self) {
        drop(lock(&self.pty).take());
        drop(lock(&self.writer).take());
    }

    /// Waits until the shell exits, or `limit` passes.
    ///
    /// On Unix a shell that already exited is reaped and published under the
    /// reap lock; a running one is only checked with `try_wait`, so the pid is
    /// never freed without the exit becoming visible in the same critical
    /// section. The child lock is only ever tried, never waited for, so a
    /// shell that is busy being reaped is still running as far as the deadline
    /// is concerned and the caller moves on to TERM and KILL.
    async fn wait_for_shell_exit(&self, limit: Duration) {
        let deadline = Instant::now() + limit;
        loop {
            #[cfg(windows)]
            {
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
            }
            #[cfg(unix)]
            {
                let published = {
                    let _reap = lock(&self.reap);
                    if lock(&self.exit).is_some() {
                        return;
                    }
                    match try_lock(&self.child) {
                        Some(mut child) => self.reap_exited(&mut child),
                        None => None,
                    }
                };
                if let Some(exit) = published {
                    self.notify_exit_once(exit);
                    return;
                }
            }
            if Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

/// Builds the published exit record from a reaped status.
fn terminal_exit(status: Option<portable_pty::ExitStatus>) -> TerminalExit {
    match status {
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
    let snapshot = viewer.snapshot_through();
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
                if !accept_position(
                    answers, &viewers, viewer_id, viewer, position, ordinal, latest, snapshot,
                ) {
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
            if !accept_position(
                answers, &viewers, viewer_id, viewer, position, ordinal, latest, snapshot,
            ) {
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

/// Records an answer for `(position, ordinal)` if this viewer may name it.
///
/// The identity comes from the output stream, not from the viewer: `position`
/// is the chunk that carried the query's first byte and `ordinal` counts
/// queries that start in that chunk, so every viewer that parsed that chunk
/// names the same answer the same way. A reply naming output this viewer was
/// never sent is ignored, so a bogus claim cannot suppress a real answer.
///
/// A position inside this viewer's snapshot that it never parsed raw is
/// claimable only when no other attached viewer that could actually write
/// received it raw. The rule is deliberately narrow: only a snapshot-only
/// position gives way, and only to a viewer that has the bytes and can answer
/// under the current ownership rule. A chunk this viewer parsed raw before a
/// later resync stays its own to claim, and when every viewer saw the query
/// only through a snapshot nobody gives way, so the shared record still
/// dedupes the answer to one write and the shell is never left without one.
/// A rejected reply is dropped without advancing the record, so it cannot
/// suppress the answer of a viewer that really saw the query.
#[allow(clippy::too_many_arguments)]
fn accept_position(
    answers: &Mutex<AnsweredQueries>,
    viewers: &HashMap<ViewerId, ViewerEntry>,
    viewer_id: ViewerId,
    viewer: &Viewer,
    position: u64,
    ordinal: u64,
    latest: u64,
    snapshot: u64,
) -> bool {
    if position == 0 || position > latest {
        return false;
    }
    let snapshot_only = position <= snapshot && !viewer.raw_contains(position);
    if snapshot_only
        && viewers.iter().any(|(id, entry)| {
            *id != viewer_id
                && entry.viewer.raw_contains(position)
                && answers_position(id, entry, viewers)
        })
    {
        return false;
    }
    // Checked here and recorded after the write (`commit_reply`); the writer
    // lock makes the two steps one decision.
    !lock(answers).is_answered(position, ordinal)
}

/// Whether a viewer's reply for this position could actually be written.
///
/// Giving way is only right when the other viewer can answer: otherwise the
/// query is dropped twice and the shell never hears back. When no legacy
/// client is attached every attached viewer names positions, so any of them
/// can write. While one is attached, a single viewer owns marked replies once
/// an owner exists, and before that only a viewer that names positions can
/// answer a positioned query: a legacy client's unpositioned reply does not
/// name this `(position, ordinal)` at all.
fn answers_position(
    id: &ViewerId,
    entry: &ViewerEntry,
    viewers: &HashMap<ViewerId, ViewerEntry>,
) -> bool {
    if viewers.values().all(|entry| entry.reply_positions) {
        return true;
    }
    match viewers
        .iter()
        .find(|(_, entry)| entry.legacy_owner)
        .map(|(owner, _)| *owner)
    {
        Some(owner) => *id == owner,
        None => entry.reply_positions,
    }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Signal {
    Term,
    Kill,
}

/// The signals the close path sends.
///
/// The default methods are the real ones; tests substitute a recorder so a
/// close can never signal a real pid while still exercising the same path.
trait CloseSignals: Send + Sync {
    /// Sends SIGHUP to the shell; returns whether it was delivered.
    fn hangup(&self, pid: i32) -> bool {
        signal_shell(pid)
    }

    /// Signals the shell's process group.
    fn group(&self, pgid: i32, signal: Signal) {
        signal_group(pgid, signal);
    }

    /// Signals one process by pid.
    fn pid(&self, pid: Option<i32>, signal: Signal) {
        signal_pid(pid, signal);
    }
}

/// The signals a real close sends.
struct SystemSignals;

impl CloseSignals for SystemSignals {}

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
/// The Unix close path uses this for its pid fallback while it holds the reap
/// lock, so the signal goes out on schedule and cannot race a reap; the child
/// handle is not needed for it. Windows ignores it.
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
    fn a_snapshot_advances_the_sent_bound_and_the_snapshot_bound() {
        let (viewer, _rx) = viewer();
        viewer.reset_credit(0);
        assert_eq!(viewer.sent_through(), 0);
        assert_eq!(viewer.snapshot_through(), 0);
        viewer.reset_credit(5);
        assert_eq!(
            viewer.sent_through(),
            5,
            "a snapshot at 5 is the latest position the viewer received"
        );
        assert_eq!(
            viewer.snapshot_through(),
            5,
            "chunks at or below 5 arrived only inside the snapshot"
        );
        viewer.record_sent(6, 1);
        assert_eq!(
            viewer.sent_through(),
            6,
            "the first raw chunk after the snapshot is sendable and answerable"
        );
        assert_eq!(
            viewer.snapshot_through(),
            5,
            "a raw chunk after the snapshot does not move the snapshot bound"
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
    /// The EOF makes the reader thread call `finish`, which waits for the
    /// process to exit without reaping it. Closing the terminal must still
    /// send TERM and then KILL on schedule instead of waiting for that wait.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_does_not_wait_for_a_shell_that_outlives_the_reader_eof() {
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

        // Wait until the reader thread's `finish` is waiting for the shell
        // without reaping it. That wait must not hold the child lock or the
        // reap lock: otherwise the close could not signal at all.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !session.awaiting_shell_exit.load(Ordering::SeqCst) {
            assert!(
                Instant::now() < deadline,
                "the PTY EOF must let the reader thread reach the shell wait"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // The hangup is ignored, so the wait lasts its whole bound; the TERM
        // grace follows. The reap lock must add nothing on top. A blocking
        // lock would never be released here, because only this path kills the
        // shell, so a watchdog ends it shortly after the deadline and the
        // wall-clock assertion below fails instead of hanging the test.
        //
        // The watchdog waits on a completion signal rather than sleeping a
        // fixed time, and it only signals a pid under the reap lock while the
        // exit is unpublished: once the shell is reaped its pid may be reused,
        // and killing it then would hit an unrelated process.
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
                let _reap = lock(&session.reap);
                if lock(&session.exit).is_some() {
                    return false;
                }
                if let Some(pid) = session.shell_pid {
                    // SAFETY: the reap lock is held and the exit is
                    // unpublished, so the pid is the live shell or its
                    // unreaped zombie; ESRCH after an exit that races this
                    // check is ignored.
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
            "close must not wait for the shell the reader thread is waiting on (took {elapsed:?})"
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

        // The child handle directly proves the cloned killer ended the console
        // process, which is the close contract on Windows; the exit notice
        // also depends on the reader reaching EOF, which the close path
        // arranges by dropping the master.
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

        // The reader must also be released: `finish` runs only after `read`
        // returns, so a leaked blocked reader leaves the session without an
        // exit report and fails here instead of passing on the child alone.
        let deadline = Instant::now() + Duration::from_secs(10);
        while session.info().exit.is_none() {
            assert!(
                Instant::now() < deadline,
                "closing both PTY ends must release the reader so it reports the exit \
                 (alive={}, child lock free={})",
                session.is_alive(),
                session.child.try_lock().is_ok(),
            );
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

    /// A viewer sent `raw` as output and then resynced from a snapshot at
    /// `snapshot`, so the positions between them arrived only inside it.
    fn viewer_sent_then_resynced(
        raw: u64,
        snapshot: u64,
    ) -> (Arc<Viewer>, mpsc::Receiver<Outbound>) {
        let (viewer, rx) = viewer();
        for seq in 1..=raw {
            viewer.record_sent(seq, 1);
        }
        viewer.ack(raw);
        viewer.begin_resync();
        viewer.record_snapshot_sent(snapshot);
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
    fn a_raw_chunk_stays_answerable_after_a_later_resync() {
        // B parsed chunk 5 as raw output, then was resynced at 8, so 6..=8
        // arrived only inside the snapshot. A attached with 7 in its snapshot
        // and never parsed 5 raw. B's answer for 5 is the one that saw the
        // query; A's claim must give way to it instead of dropping both.
        let written = Arc::new(Mutex::new(Vec::new()));
        let writer = recording_writer(&written);
        let (resynced, _resynced_rx) = viewer_sent_then_resynced(5, 8);
        let (attached, _attached_rx) = viewer();
        attached.reset_credit(7);
        let (viewers, answers) = input_state(&[(1, resynced), (2, attached)]);

        write_input_locked(&writer, &viewers, &answers, 2, b"iATTACHED", at(5, 0))
            .expect("a snapshot-only claim gives way without failing the connection");
        write_input_locked(&writer, &viewers, &answers, 1, b"iRESYNCED", at(5, 0))
            .expect("the chunk parsed raw before the resync is still answerable");

        assert_eq!(
            &*lock(&written),
            b"iRESYNCED",
            "the viewer that parsed chunk 5 raw answers it, exactly once"
        );
    }

    #[test]
    fn two_viewers_that_parsed_the_chunk_raw_answer_it_once_after_a_resync() {
        // Both viewers parsed chunk 5 (its last query ended inside `OSC 10;?`)
        // and both were resynced at 10. Neither position is snapshot-only, so
        // both claims are accepted and the shared identity keeps one write.
        let written = Arc::new(Mutex::new(Vec::new()));
        let writer = recording_writer(&written);
        let (first, _first_rx) = viewer_sent_then_resynced(5, 10);
        let (second, _second_rx) = viewer_sent_then_resynced(5, 10);
        let (viewers, answers) = input_state(&[(1, first), (2, second)]);

        write_input_locked(&writer, &viewers, &answers, 1, b"iFIRST", at(5, 0))
            .expect("the first viewer's claim is accepted");
        write_input_locked(&writer, &viewers, &answers, 2, b"iSECOND", at(5, 0))
            .expect("the duplicate is dropped without failing the connection");

        assert_eq!(
            &*lock(&written),
            b"iFIRST",
            "two viewers that parsed the chunk raw still write one answer"
        );
        assert!(lock(&answers).is_answered(5, 0));
    }

    #[test]
    fn a_resynced_viewer_answers_a_query_only_it_saw_raw() {
        // A parsed chunk 5 raw and was then resynced at 10; it completes the
        // split query, naming it (5, 0). B attached fresh with 7 in its
        // snapshot and never parsed 5 raw. A's claim is its own: B has no raw
        // bytes for 5, so dropping it would leave the shell with no answer.
        let written = Arc::new(Mutex::new(Vec::new()));
        let writer = recording_writer(&written);
        let (resynced, _resynced_rx) = viewer_sent_then_resynced(5, 10);
        let (fresh, _fresh_rx) = viewer();
        fresh.reset_credit(7);
        let (viewers, answers) = input_state(&[(1, resynced), (2, fresh)]);

        write_input_locked(&writer, &viewers, &answers, 1, b"iCOMPLETE", at(5, 0))
            .expect("the only viewer that parsed the query raw answers it");

        assert_eq!(
            &*lock(&written),
            b"iCOMPLETE",
            "a snapshot-only position with no other raw parser is still answerable"
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

    #[test]
    fn the_legacy_owner_answers_a_snapshot_only_chunk_a_non_owner_has_raw() {
        // A owns replies and parsed 1..=3 raw before a resync at 8, so chunk 6
        // reached it only inside the snapshot. Legacy B, which is not the
        // owner, has 1..=8 raw, but B's unpositioned reply can never be
        // written while A owns. A's claim must not give way to an answer that
        // cannot come: dropping both would leave the shell unanswered.
        let written = Arc::new(Mutex::new(Vec::new()));
        let writer = recording_writer(&written);
        let (owner, _owner_rx) = viewer_sent_then_resynced(3, 8);
        let (legacy, _legacy_rx) = viewer_since_snapshot(0, 8);
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

        write_input_locked(&writer, &viewers, &answers, 1, b"iOWN", at(6, 0))
            .expect("the owner answers the query it only saw in its snapshot");
        write_input_locked(
            &writer,
            &viewers,
            &answers,
            2,
            b"iLEGACY",
            QueryReply::Unpositioned,
        )
        .expect("the non-owner's reply is dropped without failing the connection");

        assert_eq!(
            &*lock(&written),
            b"iOWN",
            "the only viewer that can answer writes exactly one answer"
        );
        assert!(lock(&answers).is_answered(6, 0));
    }

    #[test]
    fn the_legacy_owner_does_not_give_way_to_a_positioned_viewer_that_cannot_write() {
        // The same layout with a positioned non-owner: it parsed 1..=8 raw,
        // but while A owns replies its positioned answer is dropped too, so
        // only A's snapshot-only claim can answer chunk 6.
        let written = Arc::new(Mutex::new(Vec::new()));
        let writer = recording_writer(&written);
        let (owner, _owner_rx) = viewer_sent_then_resynced(3, 8);
        let (positioned, _positioned_rx) = viewer_since_snapshot(0, 8);
        let (legacy, _legacy_rx) = attached_viewer(8);
        let (viewers, answers) = input_state(&[(1, owner), (2, positioned), (3, legacy)]);
        {
            let mut viewers = lock(&viewers);
            viewers
                .get_mut(&1)
                .expect("the positioned owner")
                .legacy_owner = true;
            viewers
                .get_mut(&3)
                .expect("the legacy viewer")
                .reply_positions = false;
        }

        write_input_locked(&writer, &viewers, &answers, 1, b"iOWN", at(6, 0))
            .expect("the owner answers the query it only saw in its snapshot");
        write_input_locked(&writer, &viewers, &answers, 2, b"iOTHER", at(6, 0))
            .expect("a positioned non-owner's reply is dropped");

        assert_eq!(
            &*lock(&written),
            b"iOWN",
            "a claim never gives way to a viewer the owner rule blocks"
        );
        assert!(lock(&answers).is_answered(6, 0));
    }

    #[test]
    fn a_first_reply_from_a_snapshot_only_claimant_gives_way_only_to_a_positioned_viewer() {
        // Before any owner is set, an unpositioned reply names no position: its
        // bytes may answer a different query than the one the claim names, and
        // the claimed query could be left without an answer. So it must not
        // displace a snapshot-only claim. A viewer that names positions and has
        // the chunk raw can answer the claimed query, so the claim does give
        // way to it. Exactly one write happens in either layout.
        for positioned_raw_viewer in [false, true] {
            let written = Arc::new(Mutex::new(Vec::new()));
            let writer = recording_writer(&written);
            // The claimant parsed 1..=3 raw and was resynced at 8, so chunk 6
            // reached it only inside the snapshot.
            let (claimant, _claimant_rx) = viewer_sent_then_resynced(3, 8);
            // The legacy viewer has 1..=8 raw but names no position.
            let (legacy, _legacy_rx) = viewer_since_snapshot(0, 8);
            // The positioned viewer has 6..=8 raw and can name the answer.
            let (positioned, _positioned_rx) = viewer_since_snapshot(5, 8);
            let attached = if positioned_raw_viewer {
                vec![(1, claimant), (2, legacy), (3, positioned)]
            } else {
                vec![(1, claimant), (2, legacy)]
            };
            let (viewers, answers) = input_state(&attached);
            lock(&viewers)
                .get_mut(&2)
                .expect("the legacy viewer")
                .reply_positions = false;

            write_input_locked(&writer, &viewers, &answers, 1, b"iCLAIM", at(6, 0))
                .expect("a claim is dropped or written without failing the connection");
            if positioned_raw_viewer {
                write_input_locked(&writer, &viewers, &answers, 3, b"iRAW", at(6, 0))
                    .expect("the positioned viewer answers the chunk it parsed raw");
            } else {
                write_input_locked(
                    &writer,
                    &viewers,
                    &answers,
                    2,
                    b"iLEGACY",
                    QueryReply::Unpositioned,
                )
                .expect("the unpositioned reply is dropped without failing the connection");
            }

            let written = lock(&written);
            assert_eq!(
                &**written,
                if positioned_raw_viewer {
                    b"iRAW".as_slice()
                } else {
                    b"iCLAIM".as_slice()
                },
                "one write either way: the claim survives an unpositioned viewer and \
                 gives way to a positioned one (positioned={positioned_raw_viewer})"
            );
            assert!(
                lock(&answers).is_answered(6, 0),
                "the answer is recorded once"
            );
        }
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

    /// A shell that ignores the hangup: `kill` is delivered, the child records
    /// it and keeps running, and no process group is known.
    #[cfg(unix)]
    #[derive(Debug)]
    struct StubbornChild {
        pid: u32,
        kills: Arc<AtomicU64>,
    }

    #[cfg(unix)]
    impl portable_pty::ChildKiller for StubbornChild {
        fn kill(&mut self) -> std::io::Result<()> {
            // The SIGHUP that portable-pty sends on Unix is ignored by this
            // shell, so the process is still running after the call returns.
            self.kills.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(CountingKiller {
                kills: Arc::clone(&self.kills),
            })
        }
    }

    #[cfg(unix)]
    impl portable_pty::Child for StubbornChild {
        fn try_wait(&mut self) -> std::io::Result<Option<portable_pty::ExitStatus>> {
            Ok(None)
        }

        fn wait(&mut self) -> std::io::Result<portable_pty::ExitStatus> {
            Err(std::io::Error::other("the test child never exits"))
        }

        fn process_id(&self) -> Option<u32> {
            Some(self.pid)
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

    /// Records the signals a close sends instead of delivering them.
    #[derive(Debug, Default)]
    struct RecordedSignals {
        hangups: Mutex<Vec<i32>>,
        groups: Mutex<Vec<(i32, Signal)>>,
        pids: Mutex<Vec<(Option<i32>, Signal)>>,
    }

    impl CloseSignals for RecordedSignals {
        fn hangup(&self, pid: i32) -> bool {
            lock(&self.hangups).push(pid);
            true
        }

        fn group(&self, pgid: i32, signal: Signal) {
            lock(&self.groups).push((pgid, signal));
        }

        fn pid(&self, pid: Option<i32>, signal: Signal) {
            lock(&self.pids).push((pid, signal));
        }
    }

    /// A shell that exits during the hangup grace and records any later
    /// attempt to end it through the child handle.
    #[derive(Debug)]
    struct ReapThenRecordKillChild {
        pid: u32,
        kills: Arc<AtomicU64>,
    }

    impl portable_pty::ChildKiller for ReapThenRecordKillChild {
        fn kill(&mut self) -> std::io::Result<()> {
            // On Unix this stand-in for `child.kill()` is the SIGHUP by pid
            // that portable-pty sends; a reaped shell must not receive it.
            self.kills.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(Self {
                pid: self.pid,
                kills: Arc::clone(&self.kills),
            })
        }
    }

    impl portable_pty::Child for ReapThenRecordKillChild {
        fn try_wait(&mut self) -> std::io::Result<Option<portable_pty::ExitStatus>> {
            Ok(Some(portable_pty::ExitStatus::with_exit_code(0)))
        }

        fn wait(&mut self) -> std::io::Result<portable_pty::ExitStatus> {
            Err(std::io::Error::other("the child was already reaped"))
        }

        fn process_id(&self) -> Option<u32> {
            Some(self.pid)
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

    /// A close never signals the pid of a shell whose exit is published, even
    /// while the child lock is busy. The published exit is the confirmation:
    /// `alive == false` alone is not, because `finish` clears it before the
    /// shell has exited.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_reaped_shell_with_a_busy_child_lock_is_not_signalled_by_pid() {
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
        let mut session = Session::new(
            TerminalId::new("term_reaped_busy_child"),
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
        );
        let signals = Arc::new(RecordedSignals::default());
        session.signals = Arc::clone(&signals) as Arc<dyn CloseSignals>;
        // The exit is published, so the shell was reaped; a pid signal now
        // could reach a process that reused it.
        session.alive.store(false, Ordering::SeqCst);
        *lock(&session.exit) = Some(TerminalExit {
            status: 0,
            signal: None,
            at: Utc::now(),
        });
        let session = Arc::new(session);

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

        session.kill_with_grace().await;
        let _ = release_tx.send(());
        holder.join().expect("the lock holder joins");

        assert_eq!(
            kills.load(Ordering::SeqCst),
            0,
            "a reaped shell must not get the handle kill's SIGHUP"
        );
        assert!(
            lock(&signals.pids).is_empty(),
            "a reaped shell must not get a pid SIGKILL"
        );
    }

    #[tokio::test]
    async fn a_shell_that_exits_during_the_hangup_grace_is_not_signalled_by_pid() {
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
        let mut session = Session::new(
            TerminalId::new("term_reaped_close"),
            1,
            None,
            HostId::new("host_test"),
            "/tmp".into(),
            "/bin/sh".into(),
            80,
            24,
            pty.master,
            Box::new(ReapThenRecordKillChild {
                pid: 4_000_001,
                kills: Arc::clone(&kills),
            }),
            writer,
        );
        let signals = Arc::new(RecordedSignals::default());
        session.signals = Arc::clone(&signals) as Arc<dyn CloseSignals>;

        session.kill_with_grace().await;

        assert_eq!(
            lock(&signals.hangups).len(),
            1,
            "the close still hangs up the shell first"
        );
        assert!(
            lock(&session.reaped_status).is_some(),
            "the hangup wait reaped the shell"
        );
        let pids = lock(&signals.pids);
        assert!(
            pids.is_empty(),
            "a reaped pid may belong to another process and must not be signalled: {pids:?}"
        );
        // The primary kill is the other way the same pid is reached: on Unix
        // `child.kill()` sends SIGHUP by pid, so a reap must stop it too. On
        // Windows it holds a process handle and still runs.
        #[cfg(unix)]
        assert_eq!(
            kills.load(Ordering::SeqCst),
            0,
            "the handle kill must not signal a reaped shell's pid"
        );
        #[cfg(windows)]
        assert_eq!(
            kills.load(Ordering::SeqCst),
            1,
            "the Windows handle kill still runs after a reap"
        );
    }

    /// A shell that ignores the hangup is still ended when the session only
    /// *looks* reaped. `finish` clears `alive` before it waits for the shell
    /// without reaping it, so a close that lands in that window sees an
    /// inactive session for a shell that is still running; with no usable
    /// process group, nothing else could reach it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_shell_that_survives_the_hangup_is_still_killed() {
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
        let mut session = Session::new(
            TerminalId::new("term_hangup_survived"),
            1,
            None,
            HostId::new("host_test"),
            "/tmp".into(),
            "/bin/sh".into(),
            80,
            24,
            pty.master,
            Box::new(StubbornChild {
                pid: 4_000_002,
                kills: Arc::clone(&kills),
            }),
            writer,
        );
        let signals = Arc::new(RecordedSignals::default());
        session.signals = Arc::clone(&signals) as Arc<dyn CloseSignals>;
        // The cached foreground group is missing, so only the handle kill and
        // the pid fallback can end this shell. The reader thread's `finish` has
        // cleared `alive` and is waiting for the shell, with its exit
        // unpublished.
        session.process_group = None;
        session.alive.store(false, Ordering::SeqCst);
        let session = Arc::new(session);

        session.kill_with_grace().await;

        assert_eq!(
            lock(&signals.hangups).len(),
            1,
            "the close still hangs up the shell first"
        );
        assert_eq!(
            kills.load(Ordering::SeqCst),
            1,
            "a shell that ignored the hangup is ended through its handle, not taken for reaped"
        );
        assert!(
            lock(&signals.pids).is_empty(),
            "the handle kill worked, so the pid fallback must not run"
        );
    }

    /// A shell that has already exited when the close's terminate step runs is
    /// reaped and its exit published inside the reap critical section, never
    /// signalled by pid. The status is kept for the exit report.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_shell_that_exited_before_the_close_is_reaped_not_signalled() {
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
        let mut session = Session::new(
            TerminalId::new("term_reader_reaped"),
            1,
            None,
            HostId::new("host_test"),
            "/tmp".into(),
            "/bin/sh".into(),
            80,
            24,
            pty.master,
            Box::new(ReapThenRecordKillChild {
                pid: 4_000_003,
                kills: Arc::clone(&kills),
            }),
            writer,
        );
        let signals = Arc::new(RecordedSignals::default());
        session.signals = Arc::clone(&signals) as Arc<dyn CloseSignals>;
        // No hangup wait (no shell pid), and the shell has exited while the
        // reader thread has not published its exit yet.
        session.shell_pid = None;
        session.process_group = None;
        session.alive.store(false, Ordering::SeqCst);
        let session = Arc::new(session);

        session.kill_with_grace().await;

        assert!(
            lock(&session.reaped_status).is_some(),
            "the reap the handle found is kept for the exit report"
        );
        assert_eq!(
            kills.load(Ordering::SeqCst),
            0,
            "a reaped shell must not get the handle kill's SIGHUP"
        );
        assert!(
            lock(&signals.pids).is_empty(),
            "a reaped shell must not get a pid SIGKILL"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_shell_reaped_before_the_close_gets_no_hangup_by_pid() {
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
        let mut session = Session::new(
            TerminalId::new("term_reaped_before_close"),
            1,
            None,
            HostId::new("host_test"),
            "/tmp".into(),
            "/bin/sh".into(),
            80,
            24,
            pty.master,
            Box::new(ReapThenRecordKillChild {
                pid: 4_000_004,
                kills: Arc::clone(&kills),
            }),
            writer,
        );
        let signals = Arc::new(RecordedSignals::default());
        session.signals = Arc::clone(&signals) as Arc<dyn CloseSignals>;
        // The reader thread has finished: `finish` reaped the shell and
        // published its exit, but the cached pid is still set, so a careless
        // close would hang up that pid.
        session.shell_pid = Some(4_000_004);
        session.process_group = None;
        session.alive.store(false, Ordering::SeqCst);
        *lock(&session.exit) = Some(TerminalExit {
            status: 0,
            signal: None,
            at: Utc::now(),
        });
        let session = Arc::new(session);

        session.kill_with_grace().await;

        assert!(
            lock(&signals.hangups).is_empty(),
            "a reaped shell's pid must not get the opening SIGHUP"
        );
        assert_eq!(kills.load(Ordering::SeqCst), 0);
        assert!(lock(&signals.pids).is_empty());
    }

    /// Records every pid-targeted signal and checks the invariant at the
    /// moment it is sent: the reap lock is held and the exit is unpublished.
    #[derive(Default)]
    struct GuardedSignals {
        /// Set once the session exists (the recorder is built first so the
        /// session can own it).
        session: Mutex<Option<std::sync::Weak<Session>>>,
        hangups: Mutex<Vec<i32>>,
        pids: Mutex<Vec<(Option<i32>, Signal)>>,
        violations: Mutex<Vec<String>>,
    }

    impl GuardedSignals {
        fn record_pid_target(&self, what: &str) {
            let Some(session) = lock(&self.session).as_ref().and_then(|weak| weak.upgrade()) else {
                return;
            };
            // The signal path holds the reap lock across this call, so a free
            // lock here means the pid was signalled outside its protection.
            if session.reap.try_lock().is_ok() {
                lock(&self.violations).push(format!("{what} was sent without the reap lock"));
            }
            if lock(&session.exit).is_some() {
                lock(&self.violations)
                    .push(format!("{what} was sent after the exit was published"));
            }
        }
    }

    impl CloseSignals for GuardedSignals {
        fn hangup(&self, pid: i32) -> bool {
            self.record_pid_target("the opening hangup");
            lock(&self.hangups).push(pid);
            true
        }

        fn group(&self, _pgid: i32, _signal: Signal) {
            // Group signals are the documented exception: they end the
            // shell's jobs and are allowed after a reap.
        }

        fn pid(&self, pid: Option<i32>, signal: Signal) {
            self.record_pid_target("a pid fallback");
            lock(&self.pids).push((pid, signal));
        }
    }

    /// The exact race GPT reported: the reader thread has waited for the shell
    /// to exit but has not reaped it yet, and a close runs in that pause.
    ///
    /// The shell is still a zombie, so a pid-targeted signal may only go out
    /// while the reap lock shows the exit unpublished. The close then reaps it
    /// and publishes the real status, proving the pid had not been freed when
    /// the signal was sent.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_close_during_the_reader_exit_wait_only_signals_the_unreaped_zombie() {
        let pty = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open pty");
        let mut command = portable_pty::CommandBuilder::new("/bin/sh");
        command.args(["-c", "exit 7"]);
        let child = pty.slave.spawn_command(command).expect("spawn child");
        drop(pty.slave);
        let writer = pty.master.take_writer().expect("pty writer");
        let signals = Arc::new(GuardedSignals::default());
        let mut session = Session::new(
            TerminalId::new("term_exit_pause"),
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
        );
        session.signals = Arc::clone(&signals) as Arc<dyn CloseSignals>;
        let session = Arc::new(session);
        *lock(&signals.session) = Some(Arc::downgrade(&session));
        let pid = session.shell_pid.expect("spawned shell pid");

        // This is the pause `finish` has before it reaps: the shell is a
        // zombie and its pid is still ours.
        session.await_shell_exit_without_reaping();
        #[cfg(target_os = "linux")]
        {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .expect("the exited shell is still a zombie");
            let state = stat
                .rsplit(')')
                .next()
                .and_then(|rest| rest.split_whitespace().next());
            assert_eq!(
                state,
                Some("Z"),
                "the reader waited for the shell without reaping it"
            );
        }

        session.kill_with_grace().await;

        assert_eq!(
            *lock(&signals.hangups),
            vec![pid],
            "the opening hangup targets the shell's own pid"
        );
        assert!(
            lock(&signals.pids).is_empty(),
            "the close reaped the shell during the grace, so no pid fallback ran"
        );
        assert!(
            lock(&signals.violations).is_empty(),
            "a pid-targeted signal skipped the reap lock or followed a published exit: {:?}",
            lock(&signals.violations)
        );
        let exit = session.info().exit.expect("the close reports the exit");
        assert_eq!(
            exit.status, 7,
            "the close reaped the same unreaped zombie the hangup reached"
        );
    }

    /// A live shell still gets the opening SIGHUP. A pid is only skipped once
    /// its exit is published, so the closing hangup cannot be lost.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_live_shell_gets_the_opening_hangup() {
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
        let mut session = Session::new(
            TerminalId::new("term_live_hangup"),
            1,
            None,
            HostId::new("host_test"),
            "/tmp".into(),
            "/bin/sh".into(),
            80,
            24,
            pty.master,
            Box::new(StubbornChild {
                pid: 4_000_005,
                kills: Arc::clone(&kills),
            }),
            writer,
        );
        let signals = Arc::new(RecordedSignals::default());
        session.signals = Arc::clone(&signals) as Arc<dyn CloseSignals>;
        // No process group, and the child never reports an exit on its own.
        session.process_group = None;
        let session = Arc::new(session);

        session.kill_with_grace().await;

        assert_eq!(
            *lock(&signals.hangups),
            vec![4_000_005],
            "a live shell's pid gets the opening SIGHUP"
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
        let mut session = Session::new(
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
        );
        let signals = Arc::new(RecordedSignals::default());
        session.signals = Arc::clone(&signals) as Arc<dyn CloseSignals>;

        session.kill_with_grace().await;

        assert_eq!(
            kills.load(Ordering::SeqCst),
            1,
            "a handle kill that fails falls back to the independent killer"
        );
        assert_eq!(
            &*lock(&signals.pids),
            &[(None, Signal::Kill)],
            "a shell that is still running keeps the pid SIGKILL fallback"
        );
    }
}
