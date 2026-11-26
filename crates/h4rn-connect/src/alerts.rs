//! Alert selection and delivery for both phone platforms (ADR-051, ADR-052).
//!
//! Selection is platform-neutral: the same record, dedupe, cutoff and grouping
//! rules decide what to send. Delivery reads each registration's platform and
//! hands an Android row to FCM and an iOS row to APNs, each with its own
//! message shape.
//! Connect may see the same transition many times (a reconnect, a retry, a
//! daemon restart), so the delivered baseline is stored per `(user, thread)`
//! and keyed by the transition id: a replayed frame is dropped before it can
//! queue, while a later transition that repeats the same state still alerts.
//! Transitions that arrive together are grouped: several agents asking for a
//! person become one "N agents need attention" alert, and several finishing
//! become one "N agents finished" alert. A grouped alert names no single
//! thread, so a phone cannot mistake it for the thread already on screen and
//! mute the other agents in the group.
//!
//! The selection functions are pure so the dedupe, cutoff and grouping rules
//! are tested without a database or a network.

use std::collections::{BTreeMap, HashMap};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::FutureExt as _;
use h4rn_protocol::{AgentActivity, AgentAlertState};
use tokio::sync::{Mutex, mpsc, oneshot};
use uuid::Uuid;

use crate::apns::ApnsClient;
use crate::push::{FcmClient, PushMessage, PushPlatform, SendError};
use crate::store::{PushDeviceRow, Store};

/// The most title text one notification line carries.
const MAX_LINE_CHARS: usize = 120;

/// The most bytes a grouped alert's `thread_titles` field may add.
///
/// FCM refuses a message over 4096 bytes, and the English `body` already
/// lists the same titles. A group too large for both drops this field, and the
/// phone shows Connect's English body instead of losing the whole alert.
const MAX_GROUP_TITLES_BYTES: usize = 1024;

/// One notification Connect decided to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushAlert {
    /// Notification title.
    pub title: String,
    /// Notification body.
    pub body: String,
    /// State that drove it; a grouped finish keeps `failed` when any failed.
    pub state: AgentAlertState,
    /// Thread the tap opens; empty for a grouped alert.
    ///
    /// A group can name several threads, so it names none: the tap opens h4rn
    /// instead of one thread, and the phone never treats the group as an alert
    /// for whichever thread happens to be on screen.
    pub thread_id: String,
    /// Project name from the record, or empty for a grouped alert.
    ///
    /// Android composes a Live Update's text itself, so it needs the project
    /// on its own rather than folded into the alert body (ADR-053).
    pub project: String,
    /// Number of threads represented.
    pub count: usize,
    /// The thread's own title, cleaned, or empty when it has none.
    ///
    /// `title` and `body` are English for app versions that show them as
    /// sent; a phone that words alerts itself uses this, the state, the
    /// project and the count instead, and says "A thread" in its own
    /// language when this is empty. Empty for a grouped alert.
    pub thread_title: String,
    /// Each grouped thread's title, cleaned, in the body's order; an untitled
    /// thread is an empty entry. Empty for a single alert.
    pub thread_titles: Vec<String>,
}

impl PushAlert {
    /// The string-only data payload FCM carries.
    ///
    /// `title` and `body` are the English wording, kept for app versions that
    /// show them as sent. The other fields let a newer phone word the alert in
    /// its own language: no field here carries a prompt, reply, tool output,
    /// path or credential (ADR-051, ADR-054).
    pub fn data(&self) -> BTreeMap<String, String> {
        let mut data = BTreeMap::from([
            ("h4rn_alert".to_string(), "agent".to_string()),
            ("state".to_string(), self.state.as_str().to_string()),
            ("thread_id".to_string(), self.thread_id.clone()),
            ("title".to_string(), self.title.clone()),
            ("body".to_string(), self.body.clone()),
            ("project".to_string(), self.project.clone()),
            ("count".to_string(), self.count.to_string()),
            ("thread_title".to_string(), self.thread_title.clone()),
        ]);
        if self.count > 1 {
            // A cleaned title has no line breaks, so one per line is
            // unambiguous, and an empty line is an untitled thread.
            let titles = self.thread_titles.join("\n");
            if titles.len() <= MAX_GROUP_TITLES_BYTES {
                data.insert("thread_titles".to_string(), titles);
            }
        }
        data
    }
}

/// Whether a completion is too old to alert.
///
/// Only finishing states have a cutoff. An approval that sat on the relay is
/// still waiting, so it stays alertable.
pub fn completion_is_stale(
    activity: &AgentActivity,
    now: DateTime<Utc>,
    max_age: Duration,
) -> bool {
    if !activity.state.is_terminal() {
        return false;
    }
    let age = now.signed_duration_since(activity.at);
    age > chrono::Duration::from_std(max_age).unwrap_or(chrono::Duration::zero())
}

fn clean(value: &str, fallback: &str) -> String {
    let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim();
    if trimmed.is_empty() {
        return fallback.to_string();
    }
    trimmed.chars().take(MAX_LINE_CHARS).collect()
}

fn single(activity: &AgentActivity, project: &str) -> PushAlert {
    let title = clean(&activity.thread_title, "A thread");
    let body = match activity.state {
        // Only Android shows this state, as the ongoing Live Update's first
        // frame; the words are what the phone displays while the turn runs.
        AgentAlertState::Working => {
            if project.is_empty() {
                "Working".to_string()
            } else {
                format!("{project} · Working")
            }
        }
        AgentAlertState::Completed => {
            if project.is_empty() {
                "Finished".to_string()
            } else {
                format!("{project} · Finished")
            }
        }
        AgentAlertState::Failed => {
            if project.is_empty() {
                "Failed".to_string()
            } else {
                format!("{project} · Failed")
            }
        }
        AgentAlertState::Approval => "Needs approval".to_string(),
        AgentAlertState::Input => "Asks for input".to_string(),
        AgentAlertState::Limit => "Stopped at a limit".to_string(),
    };
    PushAlert {
        title,
        body,
        state: activity.state,
        thread_id: activity.thread_id.to_string(),
        project: project.to_string(),
        count: 1,
        thread_title: clean(&activity.thread_title, ""),
        thread_titles: Vec::new(),
    }
}

fn grouped(activities: &[AgentActivity]) -> PushAlert {
    let needs_attention = activities
        .iter()
        .all(|activity| activity.state.needs_attention());
    let mut titles: Vec<String> = Vec::new();
    let mut thread_titles: Vec<String> = Vec::new();
    for activity in activities {
        let title = clean(&activity.thread_title, "A thread");
        if !titles.contains(&title) {
            titles.push(title);
        }
        let raw = clean(&activity.thread_title, "");
        if !thread_titles.contains(&raw) {
            thread_titles.push(raw);
        }
    }
    let count = activities.len();
    let title = if needs_attention {
        format!(
            "{count} agent{} need{} attention",
            if count == 1 { "" } else { "s" },
            if count == 1 { "s" } else { "" },
        )
    } else {
        format!(
            "{count} agent{} finished",
            if count == 1 { "" } else { "s" }
        )
    };
    let state = if needs_attention {
        // Input is the stronger hand: it asks for an answer, not only a yes/no.
        // A limit needs the person too; it ranks below the two direct asks.
        if activities
            .iter()
            .any(|activity| activity.state == AgentAlertState::Input)
        {
            AgentAlertState::Input
        } else if activities
            .iter()
            .any(|activity| activity.state == AgentAlertState::Approval)
        {
            AgentAlertState::Approval
        } else {
            AgentAlertState::Limit
        }
    } else if activities
        .iter()
        .any(|activity| activity.state == AgentAlertState::Failed)
    {
        AgentAlertState::Failed
    } else {
        AgentAlertState::Completed
    };
    PushAlert {
        title,
        body: titles.join(" · "),
        state,
        // Naming the first thread would let a phone that has that thread open
        // drop the whole group, losing the other threads' alerts.
        thread_id: String::new(),
        project: String::new(),
        count,
        thread_title: String::new(),
        thread_titles,
    }
}

/// The latest transition per thread, in first-seen order.
///
/// A thread that asks twice inside one group window is still one agent the
/// person has to answer, so the later transition replaces the earlier one.
fn latest_per_thread(activities: &[AgentActivity]) -> Vec<AgentActivity> {
    let mut ordered: Vec<AgentActivity> = Vec::new();
    for activity in activities {
        match ordered
            .iter_mut()
            .find(|existing| existing.thread_id == activity.thread_id)
        {
            Some(existing) => *existing = activity.clone(),
            None => ordered.push(activity.clone()),
        }
    }
    ordered
}

/// Turns a batch of new transitions into the alerts it deserves.
///
/// Attention and completion form separate groups, so two agents asking for a
/// person produce one attention alert even if a third finished in the same
/// window. A group of one keeps that thread's own title and link, and a thread
/// that raised several transitions contributes only its latest one.
pub fn group_activities(activities: &[AgentActivity]) -> Vec<PushAlert> {
    // `working` is an ongoing update per thread, never part of an alert group:
    // counting a turn that just started as "finished" would be wrong.
    let activities: Vec<AgentActivity> = latest_per_thread(activities)
        .into_iter()
        .filter(|activity| activity.state != AgentAlertState::Working)
        .collect();
    let mut attention: Vec<AgentActivity> = Vec::new();
    let mut finished: Vec<AgentActivity> = Vec::new();
    for activity in &activities {
        if activity.state.needs_attention() {
            attention.push(activity.clone());
        } else {
            finished.push(activity.clone());
        }
    }
    let mut alerts = Vec::new();
    if attention.len() == 1 {
        alerts.push(single(&attention[0], &attention[0].project_name));
    } else if attention.len() > 1 {
        alerts.push(grouped(&attention));
    }
    if finished.len() == 1 {
        alerts.push(single(&finished[0], &finished[0].project_name));
    } else if finished.len() > 1 {
        alerts.push(grouped(&finished));
    }
    alerts
}

#[derive(Default)]
struct PendingGroup {
    activities: Vec<AgentActivity>,
    /// The window this group's timer was started for, once it has one.
    ///
    /// A turn start can take the group away before its timer fires, and the
    /// account's next group gets a fresh window. The old timer still wakes up,
    /// so it carries this number and only flushes a group that still has it:
    /// otherwise it would send the next group early and split one burst into
    /// two alerts.
    window: Option<u64>,
}

impl PendingGroup {
    /// Records one transition and returns the window to start a timer for.
    ///
    /// Only the first transition of a window starts the timer; later ones are
    /// carried by the flush it already owns. A service built for a caller that
    /// flushes itself never starts one. `next_window` is only called when a
    /// timer is started.
    fn push(
        &mut self,
        activity: AgentActivity,
        auto_flush: bool,
        next_window: impl FnOnce() -> u64,
    ) -> Option<u64> {
        self.activities.push(activity);
        if !auto_flush || self.window.is_some() {
            return None;
        }
        let window = next_window();
        self.window = Some(window);
        Some(window)
    }
}

/// Takes the account's collecting group if it still belongs to this window.
///
/// A timer whose group a turn start already took finds either nothing or the
/// account's next group, which has its own timer; it leaves that one alone.
fn take_window(
    pending: &mut HashMap<Uuid, PendingGroup>,
    user_id: Uuid,
    window: u64,
) -> Option<Vec<AgentActivity>> {
    if pending.get(&user_id)?.window != Some(window) {
        return None;
    }
    pending.remove(&user_id).map(|group| group.activities)
}

/// Takes the account's collecting group when it holds this thread.
///
/// The whole group goes out early rather than only the thread's entry, so an
/// iPhone still gets the finish (it has no Live Update to protect) and the
/// other threads in the group are not split into a second alert.
fn take_group_with(
    pending: &mut HashMap<Uuid, PendingGroup>,
    user_id: Uuid,
    activity: &AgentActivity,
) -> Option<Vec<AgentActivity>> {
    let holds_thread = pending.get(&user_id).is_some_and(|group| {
        group
            .activities
            .iter()
            .any(|queued| queued.thread_id == activity.thread_id)
    });
    if !holds_thread {
        return None;
    }
    pending.remove(&user_id).map(|group| group.activities)
}

/// The platform a stored device row is for (ADR-052).
pub fn device_platform(device: &PushDeviceRow) -> PushPlatform {
    PushPlatform::parse(Some(device.platform.as_str()))
}

/// The first phone app release that shows a `working` frame as a Live Update.
///
/// Older Android builds have no Live Update code and would show "Working" as an
/// ordinary alert on every turn start, so a row registered by one is never sent
/// that state. The comparison is on the numeric release only, so a pre-release
/// of this version counts (ADR-053).
pub const LIVE_UPDATES_MIN_APP_VERSION: (u64, u64, u64) = (1, 0, 2);

/// The numeric `major.minor.patch` of a registered app version, if it has one.
fn release_triple(version: &str) -> Option<(u64, u64, u64)> {
    let core = version
        .trim()
        .trim_start_matches('v')
        .split(['-', '+'])
        .next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Whether this phone may be sent a turn-start (`working`) frame.
///
/// Only an Android app new enough to render it as an ongoing Live Update gets
/// one. A row with no version, or one that cannot be read, is treated as an
/// older build: a missed Live Update is quiet, a stray alert is not.
pub fn device_shows_live_updates(device: &PushDeviceRow) -> bool {
    if device_platform(device) != PushPlatform::Android {
        return false;
    }
    device
        .app_version
        .as_deref()
        .and_then(release_triple)
        .is_some_and(|version| version >= LIVE_UPDATES_MIN_APP_VERSION)
}

/// Whether this relay has a sender for that platform.
///
/// A relay with only FCM configured must not try to send an iPhone's APNs
/// token to Google, and the reverse: each platform has its own sender.
pub fn platform_has_sender(platform: PushPlatform, fcm: bool, apns: bool) -> bool {
    match platform {
        PushPlatform::Android => fcm,
        PushPlatform::Ios => apns,
    }
}

/// One piece of work for an account's delivery worker.
///
/// Every job carries the transitions it sends, taken from the collecting group
/// when the job was queued. A job never reads `pending` when it runs: a worker
/// can be behind a slow push provider, and by then `pending` may hold
/// transitions Connect saw after jobs still waiting in the queue. Sending one of
/// those early would put a turn's finish ahead of its own turn-start update and
/// leave a stale "Working" entry on the phone (ADR-053).
enum DeliveryJob {
    /// Send a group of transitions.
    ///
    /// `done` is signalled once the group (and every job queued before it) has
    /// been delivered, so a caller can wait for the queue to drain.
    Flush {
        activities: Vec<AgentActivity>,
        done: Option<oneshot::Sender<()>>,
    },
    /// Send one turn-start update, after the collecting group that held the
    /// same thread when the update arrived.
    Working {
        activity: AgentActivity,
        before: Option<Vec<AgentActivity>>,
    },
    /// A job that panics, so a test can prove the worker outlives it.
    #[cfg(test)]
    Panic,
}

/// The relay-side alert service.
pub struct AlertService {
    store: Store,
    fcm: Arc<FcmClient>,
    apns: Arc<ApnsClient>,
    group_window: Duration,
    max_age: Duration,
    /// Whether `ingest` starts a timer for the group window. Only a test that
    /// calls `flush` itself turns this off.
    auto_flush: bool,
    pending: Mutex<HashMap<Uuid, PendingGroup>>,
    /// The number the next group window's timer gets.
    next_window: AtomicU64,
    /// Each account's delivery worker, while it has work.
    ///
    /// A worker drains its queue in arrival order, so a deferred group and a
    /// turn-start update for the same thread reach the phone in the order
    /// Connect saw them: the finish of a turn sent after the next turn's
    /// `working` frame would replace the ongoing entry while the agent is still
    /// running (ADR-053). Senders only enqueue, so nothing on the relay loop
    /// waits for FCM or APNs. A worker removes its own entry once its queue is
    /// empty, so the map holds only accounts with deliveries in flight.
    workers: StdMutex<HashMap<Uuid, mpsc::UnboundedSender<DeliveryJob>>>,
    /// How many transitions reached `ingest`, so a relay test can prove a
    /// gated frame never got this far.
    #[cfg(test)]
    pub(crate) ingested: std::sync::atomic::AtomicUsize,
}

impl AlertService {
    /// Builds the service around the store and the two platform senders.
    pub fn new(
        store: Store,
        fcm: Arc<FcmClient>,
        apns: Arc<ApnsClient>,
        group_window: Duration,
        max_age: Duration,
    ) -> Self {
        Self {
            store,
            fcm,
            apns,
            group_window,
            max_age,
            auto_flush: true,
            pending: Mutex::new(HashMap::new()),
            next_window: AtomicU64::new(0),
            workers: StdMutex::new(HashMap::new()),
            #[cfg(test)]
            ingested: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// The same service with the flush left to the caller.
    ///
    /// The relay always lets `ingest` schedule the window. A test that calls
    /// `flush` itself uses this so a slow database write cannot let the window
    /// expire between two transitions that belong to one group.
    #[doc(hidden)]
    pub fn manual_flush(mut self) -> Self {
        self.auto_flush = false;
        self
    }

    /// Records one transition and queues its delivery.
    ///
    /// The relay's read loop awaits this, so it only writes the baseline and
    /// enqueues: FCM and APNs are called from the account's delivery worker,
    /// and a slow push provider never stops Connect reading a machine's socket.
    /// The baseline is written first, so a duplicate published while the group
    /// is still collecting is dropped.
    pub async fn ingest(self: &Arc<Self>, user_id: Uuid, activity: AgentActivity) {
        #[cfg(test)]
        self.ingested
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match self
            .store
            .claim_push_thread_state(
                user_id,
                activity.thread_id.as_str(),
                activity.state.as_str(),
                &activity.transition_id(),
            )
            .await
        {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                tracing::warn!(%error, "could not record the push baseline");
                return;
            }
        }
        if completion_is_stale(&activity, Utc::now(), self.max_age) {
            return;
        }
        if !self.fcm.configured() && !self.apns.configured() {
            return;
        }
        // A turn that started is an ongoing Live Update for its own thread on
        // Android, not an alert to group with finishing ones (ADR-053). The
        // claimed transition above already makes a replay of this frame a
        // no-op, so each state change updates the phone at most once.
        if activity.state == AgentAlertState::Working {
            // A queued message starts its turn the moment the previous one
            // finishes, so that finish is usually still waiting out the group
            // window. It goes out first, in the same job: delivered later, it
            // would reuse the thread's notification and replace the new turn's
            // Live Update. Only what is collecting now goes with it; a finish
            // that arrives after this frame stays for its own flush. The job is
            // queued under the same lock, so no flush can slip in between.
            let mut pending = self.pending.lock().await;
            let before = take_group_with(&mut pending, user_id, &activity);
            self.enqueue(user_id, DeliveryJob::Working { activity, before });
            return;
        }
        let window = {
            let mut pending = self.pending.lock().await;
            pending
                .entry(user_id)
                .or_default()
                .push(activity, self.auto_flush, || {
                    self.next_window.fetch_add(1, Ordering::Relaxed)
                })
        };
        if let Some(window) = window {
            let service = Arc::clone(self);
            let delay = self.group_window;
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                service.queue_window(user_id, window).await;
            });
        }
    }

    /// Queues the group a window timer was started for, if it is still
    /// collecting.
    ///
    /// The group is taken and queued under the `pending` lock, like every
    /// other flush, so the queue keeps the order Connect saw the transitions.
    async fn queue_window(self: &Arc<Self>, user_id: Uuid, window: u64) {
        let mut pending = self.pending.lock().await;
        if let Some(activities) = take_window(&mut pending, user_id, window) {
            self.enqueue(
                user_id,
                DeliveryJob::Flush {
                    activities,
                    done: None,
                },
            );
        }
    }

    /// Takes the collecting group and queues it, under the `pending` lock.
    ///
    /// Queuing under the lock keeps the queue in the order Connect saw the
    /// transitions: a turn-start that takes the group with it is queued under
    /// the same lock. The group's own window timer, if it has one, then finds
    /// nothing to send.
    async fn queue_flush(self: &Arc<Self>, user_id: Uuid, done: oneshot::Sender<()>) {
        let mut pending = self.pending.lock().await;
        let activities = pending
            .remove(&user_id)
            .map(|group| group.activities)
            .unwrap_or_default();
        self.enqueue(
            user_id,
            DeliveryJob::Flush {
                activities,
                done: Some(done),
            },
        );
    }

    /// Sends the collecting group now and waits until the account's queue,
    /// including everything enqueued before this call, has been delivered.
    ///
    /// Public so tests can drain without waiting for the window.
    pub async fn flush(self: &Arc<Self>, user_id: Uuid) {
        let (done, finished) = oneshot::channel();
        self.queue_flush(user_id, done).await;
        let _ = finished.await;
    }

    /// Hands a job to the account's delivery worker, starting one if needed.
    ///
    /// The send happens under the map lock, and a worker only removes itself
    /// under the same lock after finding its queue empty, so a job is never
    /// left in a queue nobody drains.
    fn enqueue(self: &Arc<Self>, user_id: Uuid, job: DeliveryJob) {
        let mut workers = self
            .workers
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let job = match workers.get(&user_id) {
            Some(sender) => match sender.send(job) {
                Ok(()) => return,
                // The worker stopped without removing itself (its task was
                // aborted); start a fresh one below.
                Err(mpsc::error::SendError(job)) => job,
            },
            None => job,
        };
        let (sender, receiver) = mpsc::unbounded_channel();
        // A new channel's receiver is alive, so this send cannot fail.
        let _ = sender.send(job);
        workers.insert(user_id, sender);
        drop(workers);
        let service = Arc::clone(self);
        tokio::spawn(async move { service.run_worker(user_id, receiver).await });
    }

    /// Drains one account's queue in order, then removes its own entry.
    ///
    /// While this worker holds its receiver, a send to its entry cannot fail,
    /// so no other worker can have replaced it: the entry it removes is its own.
    async fn run_worker(&self, user_id: Uuid, mut queue: mpsc::UnboundedReceiver<DeliveryJob>) {
        loop {
            let job = {
                let mut workers = self
                    .workers
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                match queue.try_recv() {
                    Ok(job) => job,
                    Err(_) => {
                        workers.remove(&user_id);
                        return;
                    }
                }
            };
            // A panic in one delivery must not end the worker: the jobs
            // queued behind it would sit in a channel nobody drains.
            if let Err(panic) = AssertUnwindSafe(self.run_job(user_id, job))
                .catch_unwind()
                .await
            {
                let message = panic
                    .downcast_ref::<&str>()
                    .map(|text| (*text).to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_default();
                tracing::error!(%user_id, panic = %message, "a push delivery panicked");
            }
        }
    }

    async fn run_job(&self, user_id: Uuid, job: DeliveryJob) {
        match job {
            DeliveryJob::Flush { activities, done } => {
                if !activities.is_empty() {
                    self.deliver_group(user_id, &activities).await;
                }
                if let Some(done) = done {
                    let _ = done.send(());
                }
            }
            DeliveryJob::Working { activity, before } => {
                if let Some(activities) = before {
                    self.deliver_group(user_id, &activities).await;
                }
                self.deliver_working(user_id, &activity).await;
            }
            #[cfg(test)]
            DeliveryJob::Panic => panic!("a test delivery panicked"),
        }
    }

    /// Whether the account has a delivery worker, that is, deliveries queued
    /// or in flight. An idle account has no entry.
    pub fn delivery_in_flight(&self, user_id: Uuid) -> bool {
        self.workers
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .contains_key(&user_id)
    }

    /// Sends the alerts a batch of transitions deserves to every phone.
    async fn deliver_group(&self, user_id: Uuid, activities: &[AgentActivity]) {
        let alerts = group_activities(activities);
        if alerts.is_empty() {
            return;
        }
        let devices = match self.store.push_devices(user_id).await {
            Ok(devices) => devices,
            Err(error) => {
                tracing::warn!(%error, "could not list push devices");
                return;
            }
        };
        for alert in alerts {
            for device in &devices {
                self.send_to(device, &alert).await;
            }
        }
    }

    /// Sends one turn-start update to this account's Android phones.
    ///
    /// iOS has no Live Update equivalent here, so an iPhone is not sent this
    /// record at all: its `working` state would otherwise become a visible
    /// alert for a turn that has not finished. An Android build older than
    /// [`LIVE_UPDATES_MIN_APP_VERSION`] is skipped for the same reason
    /// (ADR-053).
    async fn deliver_working(&self, user_id: Uuid, activity: &AgentActivity) {
        if !self.fcm.configured() {
            return;
        }
        let alert = single(activity, &activity.project_name);
        let devices = match self.store.push_devices(user_id).await {
            Ok(devices) => devices,
            Err(error) => {
                tracing::warn!(%error, "could not list push devices");
                return;
            }
        };
        for device in &devices {
            if !device_shows_live_updates(device) {
                continue;
            }
            self.send_to(device, &alert).await;
        }
    }

    /// Delivers one alert to one phone over its own platform's transport.
    async fn send_to(&self, device: &PushDeviceRow, alert: &PushAlert) {
        let platform = device_platform(device);
        if !platform_has_sender(platform, self.fcm.configured(), self.apns.configured()) {
            // This phone's platform has no sender on this relay. The other
            // platform's phones still get their alert.
            return;
        }
        let message = PushMessage {
            token: &device.token,
            platform,
            package_name: device.package_name.as_deref(),
            data: alert.data(),
        };
        let result = match platform {
            PushPlatform::Android => self.fcm.send(&message).await,
            PushPlatform::Ios => self.apns.send(message.token, &message.apns_payload()).await,
        };
        match result {
            Ok(()) => {}
            Err(SendError::Unregistered) => {
                if let Err(error) = self.store.delete_push_device_by_token(&device.token).await {
                    tracing::warn!(%error, "could not delete an unregistered token");
                }
            }
            Err(SendError::Disabled) => {}
            Err(SendError::Failed(error)) => {
                tracing::warn!(
                    %error,
                    device = %device.id,
                    platform = platform.as_str(),
                    "push delivery failed"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn activity(state: AgentAlertState, title: &str) -> AgentActivity {
        AgentActivity {
            thread_id: h4rn_protocol::ThreadId::new_uuid_v7(),
            thread_title: title.to_string(),
            project_name: "proj".to_string(),
            state,
            at: Utc::now(),
            transition: Uuid::now_v7().to_string(),
        }
    }

    #[test]
    fn a_replayed_frame_is_the_same_transition() {
        let activity = activity(AgentAlertState::Completed, "one");
        assert_eq!(activity.transition_id(), activity.clone().transition_id());
    }

    fn device(platform: &str) -> PushDeviceRow {
        PushDeviceRow {
            id: Uuid::now_v7(),
            user_id: Uuid::now_v7(),
            device_id: "device-1".to_string(),
            token: "token".to_string(),
            platform: platform.to_string(),
            package_name: None,
            app_version: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn each_phone_gets_the_sender_for_its_own_platform() {
        assert_eq!(device_platform(&device("android")), PushPlatform::Android);
        assert_eq!(device_platform(&device("ios")), PushPlatform::Ios);
        // A row written before the column existed is an Android install.
        assert_eq!(device_platform(&device("")), PushPlatform::Android);

        assert!(platform_has_sender(PushPlatform::Android, true, false));
        assert!(!platform_has_sender(PushPlatform::Ios, true, false));
        assert!(platform_has_sender(PushPlatform::Ios, false, true));
        assert!(!platform_has_sender(PushPlatform::Android, false, true));
        assert!(!platform_has_sender(PushPlatform::Ios, false, false));
    }

    fn device_at(platform: &str, version: Option<&str>) -> PushDeviceRow {
        PushDeviceRow {
            app_version: version.map(str::to_string),
            ..device(platform)
        }
    }

    #[test]
    fn only_a_new_enough_android_app_is_sent_a_working_frame() {
        let live = |platform: &str, version: Option<&str>| {
            device_shows_live_updates(&device_at(platform, version))
        };
        assert!(live("android", Some("1.0.2")));
        assert!(live("android", Some("1.1.0")));
        assert!(live("android", Some("2.0.0")));
        assert!(live("android", Some("1.0.2-rc.1")));
        // Builds without Live Updates would show "Working" as an alert.
        assert!(!live("android", Some("1.0.1")));
        assert!(!live("android", Some("0.9.9")));
        // No version, or an unreadable one, is an older build.
        assert!(!live("android", None));
        assert!(!live("android", Some("")));
        assert!(!live("android", Some("dev")));
        assert!(!live("android", Some("1.2")));
        // An iPhone has no Live Update, whatever its version.
        assert!(!live("ios", Some("9.9.9")));
    }

    #[test]
    fn a_later_completion_in_the_same_state_is_a_new_transition() {
        let first = activity(AgentAlertState::Completed, "one");
        let mut second = first.clone();
        // Same state and same instant: only the transition id separates them.
        second.transition = Uuid::now_v7().to_string();
        assert_ne!(first.transition_id(), second.transition_id());
    }

    #[test]
    fn a_limit_says_the_agent_stopped() {
        let alert = single(&activity(AgentAlertState::Limit, "one"), "proj");
        assert_eq!(alert.body, "Stopped at a limit");
        assert_eq!(alert.state, AgentAlertState::Limit);
    }

    #[test]
    fn completions_older_than_two_minutes_are_stale() {
        let mut activity = activity(AgentAlertState::Completed, "one");
        activity.at = Utc::now() - chrono::Duration::minutes(3);
        assert!(completion_is_stale(
            &activity,
            Utc::now(),
            Duration::from_secs(120)
        ));
        activity.at = Utc::now() - chrono::Duration::seconds(30);
        assert!(!completion_is_stale(
            &activity,
            Utc::now(),
            Duration::from_secs(120)
        ));
    }

    #[test]
    fn a_waiting_approval_has_no_age_cutoff() {
        let mut activity = activity(AgentAlertState::Approval, "one");
        activity.at = Utc::now() - chrono::Duration::hours(3);
        assert!(!completion_is_stale(
            &activity,
            Utc::now(),
            Duration::from_secs(120)
        ));
    }

    #[test]
    fn two_attention_states_become_one_alert() {
        let alerts = group_activities(&[
            activity(AgentAlertState::Approval, "one"),
            activity(AgentAlertState::Input, "two"),
        ]);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].title, "2 agents need attention");
        assert_eq!(alerts[0].body, "one · two");
        assert_eq!(alerts[0].count, 2);
        assert_eq!(alerts[0].state, AgentAlertState::Input);
    }

    #[test]
    fn a_limit_is_attention() {
        let alerts = group_activities(&[
            activity(AgentAlertState::Limit, "one"),
            activity(AgentAlertState::Approval, "two"),
        ]);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].title, "2 agents need attention");
        assert_eq!(alerts[0].state, AgentAlertState::Approval);
    }

    #[test]
    fn two_completions_become_one_finished_alert() {
        let alerts = group_activities(&[
            activity(AgentAlertState::Completed, "one"),
            activity(AgentAlertState::Failed, "two"),
        ]);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].title, "2 agents finished");
        assert_eq!(alerts[0].state, AgentAlertState::Failed);
    }

    #[test]
    fn a_grouped_alert_names_no_single_thread() {
        let alerts = group_activities(&[
            activity(AgentAlertState::Approval, "one"),
            activity(AgentAlertState::Approval, "two"),
        ]);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].count, 2);
        assert_eq!(alerts[0].thread_id, "", "a group named one thread");
        assert_eq!(alerts[0].data()["thread_id"], "");
    }

    #[test]
    fn a_single_transition_keeps_its_thread_and_link() {
        let one = activity(AgentAlertState::Approval, "one");
        let expected = one.thread_id.to_string();
        let alerts = group_activities(&[one]);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].title, "one");
        assert_eq!(alerts[0].body, "Needs approval");
        assert_eq!(alerts[0].thread_id, expected);
    }

    #[test]
    fn attention_and_finishing_stay_separate_groups() {
        let alerts = group_activities(&[
            activity(AgentAlertState::Approval, "one"),
            activity(AgentAlertState::Completed, "two"),
            activity(AgentAlertState::Input, "three"),
            activity(AgentAlertState::Completed, "four"),
        ]);
        assert_eq!(alerts.len(), 2);
        assert_eq!(alerts[0].title, "2 agents need attention");
        assert_eq!(alerts[1].title, "2 agents finished");
    }

    #[test]
    fn the_grouped_count_counts_threads_not_transitions() {
        let first = activity(AgentAlertState::Approval, "one");
        let mut second = first.clone();
        second.transition = Uuid::now_v7().to_string();
        let alerts = group_activities(&[first.clone(), second]);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].title, "one");
        assert_eq!(alerts[0].body, "Needs approval");
        assert_eq!(alerts[0].count, 1);
        assert_eq!(alerts[0].thread_id, first.thread_id.to_string());
    }

    #[test]
    fn a_repeated_thread_does_not_inflate_a_group() {
        let one = activity(AgentAlertState::Completed, "one");
        let two = activity(AgentAlertState::Completed, "two");
        let mut one_again = one.clone();
        one_again.transition = Uuid::now_v7().to_string();
        let alerts = group_activities(&[one, two, one_again]);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].title, "2 agents finished");
        assert_eq!(alerts[0].count, 2);
    }

    #[test]
    fn one_window_timer_covers_every_transition_it_collects() {
        let mut group = PendingGroup::default();
        assert_eq!(
            group.push(activity(AgentAlertState::Completed, "one"), true, || 7),
            Some(7)
        );
        assert_eq!(
            group.push(activity(AgentAlertState::Completed, "two"), true, || {
                panic!("a second timer was started for one window")
            }),
            None
        );
        assert_eq!(group.activities.len(), 2);
    }

    #[test]
    fn a_manual_service_never_starts_a_window() {
        let mut group = PendingGroup::default();
        let never = || -> u64 { panic!("a manual service started a window") };
        assert_eq!(
            group.push(activity(AgentAlertState::Completed, "one"), false, never),
            None
        );
        assert_eq!(
            group.push(activity(AgentAlertState::Completed, "two"), false, never),
            None
        );
        assert_eq!(group.activities.len(), 2);
    }

    #[test]
    fn a_window_timer_only_takes_the_group_it_was_started_for() {
        let user = Uuid::now_v7();
        let mut pending: HashMap<Uuid, PendingGroup> = HashMap::new();
        // A turn start took the first window's group; the next group has its
        // own window.
        let next = pending.entry(user).or_default();
        next.push(activity(AgentAlertState::Completed, "c"), true, || 2);
        assert!(take_window(&mut pending, user, 1).is_none());
        assert_eq!(
            pending[&user].activities.len(),
            1,
            "the next group was taken"
        );
        let taken = take_window(&mut pending, user, 2).expect("its own timer takes it");
        assert_eq!(taken.len(), 1);
        assert!(!pending.contains_key(&user));
        // A timer whose group is gone finds nothing.
        assert!(take_window(&mut pending, user, 2).is_none());
    }

    #[test]
    fn push_data_carries_only_the_documented_fields() {
        let alert = single(&activity(AgentAlertState::Completed, "one"), "proj");
        let data = alert.data();
        let keys: Vec<&str> = data.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                "body",
                "count",
                "h4rn_alert",
                "project",
                "state",
                "thread_id",
                "thread_title",
                "title"
            ]
        );
        assert_eq!(data["h4rn_alert"], "agent");
        assert_eq!(data["state"], "completed");
        assert_eq!(data["project"], "proj");
        assert_eq!(data["thread_title"], "one");
    }

    #[test]
    fn a_phone_gets_the_raw_title_and_older_apps_keep_the_english_words() {
        // An untitled thread sends an empty title, so the phone says "A
        // thread" in its own language; the English fallback stays in `title`
        // for an app that shows it as sent.
        let alert = single(&activity(AgentAlertState::Approval, "  "), "proj");
        let data = alert.data();
        assert_eq!(data["thread_title"], "");
        assert_eq!(data["title"], "A thread");
        assert_eq!(data["body"], "Needs approval");
        assert!(!data.contains_key("thread_titles"));

        let titled = single(&activity(AgentAlertState::Input, " fix\n the  bug "), "");
        assert_eq!(titled.data()["thread_title"], "fix the bug");
    }

    #[test]
    fn a_group_lists_each_raw_title_once_in_the_body_order() {
        let alerts = group_activities(&[
            activity(AgentAlertState::Completed, "one"),
            activity(AgentAlertState::Failed, ""),
            activity(AgentAlertState::Completed, "one"),
            activity(AgentAlertState::Completed, "two"),
        ]);
        assert_eq!(alerts.len(), 1);
        let data = alerts[0].data();
        assert_eq!(data["body"], "one · A thread · two");
        assert_eq!(data["thread_titles"], "one\n\ntwo");
        assert_eq!(data["thread_title"], "");
        assert_eq!(data["count"], "4");
        assert_eq!(data["state"], "failed");
        assert_eq!(data["title"], "4 agents finished");
    }

    #[test]
    fn a_group_too_large_for_fcm_keeps_only_the_english_body() {
        let long = "x".repeat(MAX_LINE_CHARS);
        let activities: Vec<AgentActivity> = (0..10)
            .map(|index| activity(AgentAlertState::Completed, &format!("{index}{long}")))
            .collect();
        let alerts = group_activities(&activities);
        let data = alerts[0].data();
        assert!(
            !data.contains_key("thread_titles"),
            "a group past the budget still carried its titles twice"
        );
        assert!(data["body"].starts_with("0x"));
        // A small group keeps them.
        let small = group_activities(&activities[..2]);
        assert!(small[0].data().contains_key("thread_titles"));
    }

    #[test]
    fn a_turn_start_is_a_working_update_for_its_own_thread() {
        let activity = activity(AgentAlertState::Working, "one");
        let expected_thread = activity.thread_id.to_string();
        let alert = single(&activity, "proj");
        assert_eq!(alert.state, AgentAlertState::Working);
        assert_eq!(alert.title, "one");
        assert_eq!(alert.body, "proj · Working");
        assert_eq!(alert.project, "proj");
        assert_eq!(alert.thread_id, expected_thread);
        assert_eq!(alert.data()["state"], "working");
    }

    #[test]
    fn a_working_update_is_never_grouped_with_finishing_threads() {
        let alerts = group_activities(&[
            activity(AgentAlertState::Working, "one"),
            activity(AgentAlertState::Completed, "two"),
        ]);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].title, "two");
        assert_eq!(alerts[0].body, "proj · Finished");
        assert_eq!(alerts[0].count, 1);
    }

    /// A service whose store never connects; enough for jobs that send nothing.
    fn offline_service() -> Arc<AlertService> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://h4rn@127.0.0.1:1/h4rn")
            .expect("a well-formed lazy pool URL");
        let http = reqwest::Client::new();
        Arc::new(AlertService::new(
            Store::from_pool(pool),
            Arc::new(FcmClient::disabled(http.clone())),
            Arc::new(ApnsClient::disabled(http)),
            Duration::from_millis(1),
            Duration::from_secs(120),
        ))
    }

    #[tokio::test]
    async fn a_panicking_delivery_leaves_the_queue_draining() {
        let service = offline_service();
        let user_id = Uuid::now_v7();
        let (done, finished) = oneshot::channel();
        // Both land in the same worker's queue before it runs either.
        service.enqueue(user_id, DeliveryJob::Panic);
        service.enqueue(
            user_id,
            DeliveryJob::Flush {
                activities: Vec::new(),
                done: Some(done),
            },
        );
        let delivered = tokio::time::timeout(Duration::from_secs(5), finished).await;
        assert!(
            matches!(delivered, Ok(Ok(()))),
            "the job after a panic was never run"
        );
    }
}
