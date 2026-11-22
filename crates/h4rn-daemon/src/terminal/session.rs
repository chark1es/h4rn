//! One PTY session: process, screen model, output fan-out and per-viewer flow
//! control (ADR-007).
//!
//! A [`Session`] owns the PTY master, the child process, the headless VT grid
//! and every attached viewer. The reader thread is the only producer of output;
//! it feeds the grid and broadcasts numbered chunks. Each viewer has an
//! independent credit window, so a slow phone never blocks a desktop viewer or
//! the PTY itself. A viewer that falls outside the broadcast ring is resynced
//! from a fresh snapshot instead of replaying history.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::Utc;
use h4rn_protocol::{
    BinaryFrame, BinaryKind, CREDIT_MAX_BYTES, CREDIT_MAX_CHUNKS, HostId, TerminalExit,
    TerminalExitNotice, TerminalId, TerminalInfo, TerminalResponderNotice, ThreadId,
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

    /// Resets the window after a snapshot; the snapshot replaces all output.
    pub fn reset_credit(&self) {
        let mut credit = lock(&self.credit);
        credit.outstanding.clear();
        credit.bytes = 0;
        self.notify.notify_waiters();
    }

    /// Records a sent chunk so a later ack can free its credit.
    pub fn record_sent(&self, seq: u64, bytes: usize) {
        let mut credit = lock(&self.credit);
        credit.outstanding.push_back((seq, bytes));
        credit.bytes += bytes;
    }

    /// Frees credit acknowledged by `up_to`.
    pub fn ack(&self, up_to: u64) {
        let mut credit = lock(&self.credit);
        while let Some((seq, bytes)) = credit.outstanding.front().copied() {
            if seq > up_to {
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
    /// Attach order; the highest value is the most recently attached viewer.
    pub attached_at: u64,
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
    /// The one viewer allowed to answer terminal queries, if any.
    responder: Mutex<Option<ViewerId>>,
    /// Monotonic attach order used to elect the responder.
    next_attach: AtomicU64,
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    pty: Mutex<Box<dyn MasterPty + Send>>,
    process_group: Option<i32>,
    exit: Mutex<Option<TerminalExit>>,
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
            responder: Mutex::new(None),
            next_attach: AtomicU64::new(0),
            writer: Mutex::new(Some(writer)),
            child: Mutex::new(child),
            pty: Mutex::new(master),
            process_group,
            exit: Mutex::new(None),
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
            let mut child = lock(&self.child);
            child.wait().ok()
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
    /// previous task is closed so it cannot outlive its successor.
    ///
    /// The most recently attached viewer becomes the one that answers terminal
    /// queries; the previous responder is told to stop, because two xterm
    /// instances answering one PTY give the shell duplicate replies.
    pub fn add_viewer(&self, id: ViewerId, cols: u16, rows: u16, viewer: Arc<Viewer>) -> bool {
        let attached_at = self.next_attach.fetch_add(1, Ordering::SeqCst) + 1;
        let previous = {
            let mut viewers = lock(&self.viewers);
            viewers.insert(
                id,
                ViewerEntry {
                    viewer,
                    cols,
                    rows,
                    attached_at,
                },
            )
        };
        {
            // Hold the responder lock across the notice so a concurrent attach
            // cannot reorder the two viewers' permissions.
            let mut responder = lock(&self.responder);
            if let Some(previous) = responder.replace(id)
                && previous != id
            {
                self.notify_responder(previous, false);
            }
        }
        if let Some(previous) = previous {
            previous.viewer.close();
            if (previous.cols, previous.rows) != (cols, rows) {
                self.apply_smallest_size();
            }
        } else {
            self.apply_smallest_size();
        }
        true
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
        let (removed, successor) = {
            let mut viewers = lock(&self.viewers);
            let matches = match viewers.get(&id) {
                Some(entry) => expected.is_none_or(|viewer| Arc::ptr_eq(&entry.viewer, viewer)),
                None => false,
            };
            if !matches {
                return;
            }
            let removed = viewers.remove(&id);
            let successor = viewers
                .iter()
                .max_by_key(|(_, entry)| entry.attached_at)
                .map(|(id, entry)| (*id, Arc::clone(&entry.viewer)));
            (removed, successor)
        };
        let Some(removed) = removed else {
            return;
        };
        removed.viewer.close();
        {
            let mut responder = lock(&self.responder);
            if *responder == Some(id) {
                *responder = successor.as_ref().map(|(id, _)| *id);
                if let Some((_, viewer)) = &successor {
                    notify_responder_viewer(viewer, &self.id, true);
                }
            }
        }
        self.apply_smallest_size();
    }

    /// Closes every viewer (daemon shutdown or terminal close).
    pub fn close_viewers(&self) {
        let viewers = std::mem::take(&mut *lock(&self.viewers));
        *lock(&self.responder) = None;
        for entry in viewers.into_values() {
            entry.viewer.close();
        }
    }

    /// The viewer currently allowed to answer terminal queries.
    pub fn responder_id(&self) -> Option<ViewerId> {
        *lock(&self.responder)
    }

    fn notify_responder(&self, id: ViewerId, answers_queries: bool) {
        let viewer = lock(&self.viewers)
            .get(&id)
            .map(|entry| Arc::clone(&entry.viewer));
        if let Some(viewer) = viewer {
            notify_responder_viewer(&viewer, &self.id, answers_queries);
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

    /// Writes user input to the PTY.
    pub fn write_input(&self, data: &[u8]) -> std::io::Result<()> {
        let mut writer = lock(&self.writer);
        let Some(writer) = writer.as_mut() else {
            return Err(std::io::Error::other("terminal input is closed"));
        };
        writer.write_all(data)?;
        writer.flush()
    }

    /// Whether a viewer is attached.
    pub fn has_viewer(&self, id: ViewerId) -> bool {
        lock(&self.viewers).contains_key(&id)
    }

    /// Whether the child is still running.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Terminates every process in the terminal's session, then the child.
    ///
    /// With job control, a shell puts each background job in its own process
    /// group, so signalling only the shell's group leaves `sleep 1000 &`
    /// running. HUP goes to the whole session first: it makes the shell run
    /// its hangup path and reaches the jobs directly, then TERM and KILL clean
    /// up anything that ignored it.
    pub fn kill(&self) {
        if let Some(sid) = self.process_group {
            signal_session(sid, Signal::Hup);
            signal_session(sid, Signal::Term);
        }
        {
            let mut child = lock(&self.child);
            let _ = child.kill();
        }
        if let Some(sid) = self.process_group {
            signal_session(sid, Signal::Kill);
        }
    }

    /// Terminates with a short grace period so shells can save state.
    pub async fn kill_with_grace(&self) {
        // Hang up first, then Term, then wait, so the shell can run its exit
        // hooks; only then fall back to SIGKILL for children that ignored it.
        //
        // Windows has no POSIX sessions: closing the ConPTY is what ends the
        // console processes attached to it, and `child.kill()` below does that.
        if let Some(sid) = self.process_group {
            signal_session(sid, Signal::Hup);
            signal_session(sid, Signal::Term);
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        {
            let mut child = lock(&self.child);
            let _ = child.kill();
        }
        if let Some(sid) = self.process_group {
            signal_session(sid, Signal::Kill);
        }
    }
}

#[derive(Clone, Copy)]
enum Signal {
    Hup,
    Term,
    Kill,
}

/// Signals every process in the session led by `sid`.
///
/// `ps` is available on every Unix we ship to (Linux and macOS), unlike a
/// `/proc` walk. Process groups are signalled through their members because a
/// session can contain several groups when the shell has job control. A
/// process that called `setsid` has a different session id and is deliberately
/// left alone. A `nohup` job stays in this session, ignores the HUP, and is
/// still reached by TERM and KILL.
fn signal_session(sid: i32, signal: Signal) {
    #[cfg(unix)]
    {
        let signal = match signal {
            Signal::Hup => libc::SIGHUP,
            Signal::Term => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
        };
        // The shell is usually the session and group leader; this also
        // catches a just-forked job before `ps` can see it.
        let _ = unsafe { libc::kill(-sid, signal) };
        for pid in session_pids(sid) {
            let _ = unsafe { libc::kill(pid, signal) };
        }
    }
    #[cfg(windows)]
    {
        let _ = (sid, signal);
    }
}

/// Process ids whose session id is `sid`.
#[cfg(unix)]
fn session_pids(sid: i32) -> Vec<i32> {
    let Ok(output) = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=", "-o", "sess="])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid: i32 = fields.next()?.parse().ok()?;
            let session: i32 = fields.next()?.parse().ok()?;
            (pid > 0 && session == sid).then_some(pid)
        })
        .collect()
}

/// Sends a viewer the current answer-terminal-queries permission.
fn notify_responder_viewer(viewer: &Viewer, terminal_id: &TerminalId, answers_queries: bool) {
    let frame = h4rn_protocol::ServerFrame::TerminalResponder {
        event: TerminalResponderNotice {
            terminal_id: terminal_id.clone(),
            answers_queries,
        },
    };
    if let Ok(text) = serde_json::to_string(&frame) {
        let _ = viewer.tx.try_send(Outbound::Text(text));
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
pub async fn send_chunk(
    viewer: &Viewer,
    stream_id: u32,
    chunk: &OutputChunk,
) -> Result<(), mpsc::error::SendError<Outbound>> {
    viewer.record_sent(chunk.seq, chunk.data.len());
    viewer
        .tx
        .send(Outbound::Binary(output_frame(stream_id, chunk)))
        .await
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
                        viewer.reset_credit();
                        let frame = snapshot_frame(stream_id, &snapshot);
                        last_seq = snapshot.seq;
                        if viewer.tx.send(Outbound::Binary(frame)).await.is_err() {
                            break;
                        }
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

    #[test]
    fn stream_ids_start_at_one_and_increase() {
        let counter = AtomicU64::new(0);
        assert_eq!(next_stream_id(&counter), 1);
        assert_eq!(next_stream_id(&counter), 2);
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }
}
