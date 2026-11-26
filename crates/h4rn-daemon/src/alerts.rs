//! Daemon-side publishing of agent alerts (ADR-051).
//!
//! The switch is off until the person turns it on in Settings → Notifications.
//! While it is on, and only while the relay socket is attached, this service
//! observes the thread event stream and publishes a fixed transition for the
//! moments a phone should know about: a turn started, a turn finished, a turn
//! failed, an approval is waiting, an agent asked a question, or a turn stopped
//! at a limit that will not resume by itself.
//!
//! Nothing else leaves the machine. The payload has fields for the thread id,
//! its title, the project name, the state, a transition id and the time — no
//! prompt text, tool output, file path or credential can be carried. The
//! `working` transition is published once per turn start, not for the events
//! inside the turn, so an Android Live Update is not refreshed per token or
//! tool call (ADR-053).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use h4rn_protocol::{
    AgentActivity, AgentAlertSettings, AgentAlertState, Event, ThreadId, ThreadSummary, ThreadTab,
    TurnId, TurnStatus,
};
use tokio::sync::{Mutex, RwLock, broadcast};
use tokio::task::JoinHandle;

use crate::journal::WireEvent;
use crate::threads::ThreadManager;

/// Persisted switch for this machine.
#[derive(Debug)]
pub struct AgentAlertSettingsStore {
    path: PathBuf,
    value: RwLock<AgentAlertSettings>,
}

impl AgentAlertSettingsStore {
    /// Opens `<data_dir>/agent-alerts.json`; a bad file falls back to off.
    pub fn open(data_dir: &Path) -> Self {
        let path = data_dir.join("agent-alerts.json");
        let value = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<AgentAlertSettings>(&bytes).unwrap_or_else(
                |error| {
                    tracing::warn!(%error, path = %path.display(), "ignoring unreadable agent alert settings");
                    AgentAlertSettings::default()
                },
            ),
            Err(_) => AgentAlertSettings::default(),
        };
        Self {
            path,
            value: RwLock::new(value),
        }
    }

    /// An in-memory store for tests.
    pub fn in_memory() -> Self {
        Self {
            path: PathBuf::new(),
            value: RwLock::new(AgentAlertSettings::default()),
        }
    }

    /// The current value.
    pub async fn get(&self) -> AgentAlertSettings {
        *self.value.read().await
    }

    /// Saves and returns the new value.
    pub async fn set(&self, settings: AgentAlertSettings) -> Result<AgentAlertSettings, String> {
        if !self.path.as_os_str().is_empty() {
            let bytes = serde_json::to_vec_pretty(&settings)
                .map_err(|error| format!("could not serialise alert settings: {error}"))?;
            std::fs::write(&self.path, bytes)
                .map_err(|error| format!("could not save alert settings: {error}"))?;
        }
        *self.value.write().await = settings;
        Ok(settings)
    }
}

/// The fields an alert needs from one thread, before it becomes a payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivityThread {
    /// Thread id.
    pub id: ThreadId,
    /// Human title.
    pub title: String,
    /// Project display name.
    pub project: String,
}

impl ActivityThread {
    /// Reads the fields the payload may carry from a summary.
    ///
    /// A thread running in a worktree lives under `<location>/<project>/
    /// <branch>`, so its own folder is the branch, not the project. Naming the
    /// branch folder would tell the person the wrong project, so the folder
    /// under a known worktree location is used; without one the workspace's own
    /// folder is the best answer available.
    pub fn from_summary(summary: &ThreadSummary, worktree_roots: &[PathBuf]) -> Self {
        let path = summary
            .project_path
            .as_deref()
            .unwrap_or(&summary.workspace_path);
        let project = project_name(path, worktree_roots);
        Self {
            id: summary.thread_id.clone(),
            title: summary.title.clone(),
            project,
        }
    }
}

/// Where this daemon may have put worktree checkouts.
///
/// The location is a live setting (Settings → Worktrees), so it is resolved
/// when an alert is built rather than captured once at startup. The default
/// `<data_dir>/worktrees` stays in the list as well, because a thread created
/// before the location changed still lives under it.
#[derive(Clone)]
pub struct WorktreeRoots {
    settings: Option<Arc<crate::worktrees::settings::WorktreeSettingsStore>>,
    data_dir: Option<PathBuf>,
    /// A fixed list, used by tests instead of the live settings.
    roots: Option<Arc<Vec<PathBuf>>>,
}

impl WorktreeRoots {
    /// Reads the configured location and keeps the default root as a fallback.
    pub fn configured(
        settings: Arc<crate::worktrees::settings::WorktreeSettingsStore>,
        data_dir: PathBuf,
    ) -> Self {
        Self {
            settings: Some(settings),
            data_dir: Some(data_dir),
            roots: None,
        }
    }

    /// A fixed list for tests.
    pub fn fixed(roots: Vec<PathBuf>) -> Self {
        Self {
            settings: None,
            data_dir: None,
            roots: Some(Arc::new(roots)),
        }
    }

    fn roots_list(&self) -> Vec<PathBuf> {
        if let Some(roots) = &self.roots {
            return roots.to_vec();
        }
        let mut roots = Vec::new();
        if let Some(settings) = &self.settings
            && let Some(location) = settings.get().location
        {
            let home = dirs::home_dir();
            if let Ok(path) =
                crate::worktrees::settings::expand_location(&location, home.as_deref())
            {
                roots.push(path);
            }
        }
        if let Some(data_dir) = &self.data_dir {
            roots.push(data_dir.join("worktrees"));
        }
        roots
    }
}

/// The project folder to show for a thread's checkout.
///
/// `project_path` (an import or fork) already names the project, so the caller
/// resolves that first. A worktree checkout is identified by living directly
/// below one of the daemon's worktree locations with a project folder and a
/// branch folder under it, and by being a linked checkout (its `.git` is a
/// file). The location is the configured one, so a custom Settings → Worktrees
/// location names the project too, while an ordinary checkout that merely sits
/// two folders below a broad location (`~/code/acme/api`) keeps its own name.
/// Everything else keeps today's behaviour: the workspace's own folder name.
fn project_name(path: &str, worktree_roots: &[PathBuf]) -> String {
    if !Path::new(path).join(".git").is_file() {
        return folder_name(path);
    }
    for root in worktree_roots {
        let Ok(relative) = Path::new(path).strip_prefix(root) else {
            continue;
        };
        let mut parts = relative.components().filter_map(|part| match part {
            std::path::Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        });
        let project = parts.next();
        let branch = parts.next();
        if let (Some(project), Some(_), None) = (project, branch, parts.next()) {
            return project;
        }
    }
    folder_name(path)
}

fn folder_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Which alertable state a finished turn is, if any.
///
/// The turn's own status is the truth, not the agent state it lands in: a
/// person's Stop also becomes `AgentState::Error`, and a usage limit lands in
/// Done without meaning the work finished. A turn the person stopped says
/// nothing new, so it does not alert.
pub fn completion_alert(status: TurnStatus) -> Option<AgentAlertState> {
    match status {
        TurnStatus::Completed => Some(AgentAlertState::Completed),
        TurnStatus::Failed => Some(AgentAlertState::Failed),
        TurnStatus::Interrupted => None,
        TurnStatus::TokenLimit | TurnStatus::TurnLimit => Some(AgentAlertState::Limit),
    }
}

/// Which alertable state a provider pause is, if any.
///
/// Auto-resume means the daemon will continue by itself, so the person is not
/// needed; without it the thread waits for the person and the phone says so.
pub fn limit_alert(auto_resume: bool) -> Option<AgentAlertState> {
    if auto_resume {
        None
    } else {
        Some(AgentAlertState::Limit)
    }
}

/// Which alertable state an approval hand is.
pub fn approval_state(kind: h4rn_protocol::ApprovalKind) -> AgentAlertState {
    if kind == h4rn_protocol::ApprovalKind::Question {
        AgentAlertState::Input
    } else {
        AgentAlertState::Approval
    }
}

/// How a transition leaves the daemon. Tests replace the relay with a recorder.
#[async_trait::async_trait]
pub trait ActivityPublisher: Send + Sync {
    /// Publishes one activity; false means the relay is not connected.
    async fn publish(&self, activity: AgentActivity) -> bool;
}

#[async_trait::async_trait]
impl ActivityPublisher for crate::remote::RelayDialer {
    async fn publish(&self, activity: AgentActivity) -> bool {
        self.publish_activity(activity)
    }
}

/// Where an alert reads the thread's title and project from.
///
/// The daemon's [`ThreadManager`] is the real answer; tests inject a small
/// table so the observation rules can be exercised without a full manager.
#[async_trait::async_trait]
pub trait ThreadLookup: Send + Sync {
    /// The thread's summary, or `None` when it no longer exists.
    async fn thread_summary(&self, thread_id: &ThreadId) -> Option<ThreadSummary>;
    /// The thread event stream the watcher follows.
    fn subscribe(&self) -> broadcast::Receiver<WireEvent>;
}

#[async_trait::async_trait]
impl ThreadLookup for ThreadManager {
    async fn thread_summary(&self, thread_id: &ThreadId) -> Option<ThreadSummary> {
        ThreadManager::thread_summary(self, thread_id).await
    }

    fn subscribe(&self) -> broadcast::Receiver<WireEvent> {
        ThreadManager::subscribe(self)
    }
}

/// What the daemon remembers about one thread between events.
#[derive(Default)]
struct ThreadAlerts {
    /// The state of the last frame it published.
    published: Option<AgentAlertState>,
    /// A usage limit was published for the turn now finishing, so its Failed
    /// completion is the same stop and must not alert again.
    limit_alerted: bool,
    /// The turn whose start already published `working`.
    ///
    /// A turn start is one transition even when the daemon sees the event more
    /// than once, so the id stops a repeat from resetting the phone's ongoing
    /// notification to "Working" (ADR-053).
    working_turn: Option<TurnId>,
}

/// Watches the thread stream and publishes while the switch is on.
pub struct AgentAlertService {
    settings: Arc<AgentAlertSettingsStore>,
    manager: Option<Arc<dyn ThreadLookup>>,
    publisher: Arc<dyn ActivityPublisher>,
    /// Where this daemon may have put worktree checkouts, when known.
    worktree_roots: Option<WorktreeRoots>,
    published: Mutex<HashMap<ThreadId, ThreadAlerts>>,
    watcher: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for AgentAlertService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentAlertService")
            .finish_non_exhaustive()
    }
}

impl AgentAlertService {
    /// Builds the service; call [`Self::start`] to observe events.
    pub fn new(
        settings: Arc<AgentAlertSettingsStore>,
        manager: Option<Arc<dyn ThreadLookup>>,
        publisher: Arc<dyn ActivityPublisher>,
        worktree_roots: Option<WorktreeRoots>,
    ) -> Self {
        Self {
            settings,
            manager,
            publisher,
            worktree_roots,
            published: Mutex::new(HashMap::new()),
            watcher: Mutex::new(None),
        }
    }

    /// The current switch.
    pub async fn settings(&self) -> AgentAlertSettings {
        self.settings.get().await
    }

    /// Saves the switch. Turning it off also clears the dedupe memory, so the
    /// next time it is on, the current state can alert again.
    pub async fn set_enabled(&self, enabled: bool) -> Result<AgentAlertSettings, String> {
        let settings = self.settings.set(AgentAlertSettings { enabled }).await?;
        if !enabled {
            self.published.lock().await.clear();
        }
        Ok(settings)
    }

    /// Starts the one observation loop. Repeated calls are ignored.
    pub async fn start(self: &Arc<Self>) {
        let mut slot = self.watcher.lock().await;
        if slot.is_some() {
            return;
        }
        let Some(manager) = self.manager.clone() else {
            return;
        };
        let service = Arc::clone(self);
        *slot = Some(tokio::spawn(async move {
            let mut events = manager.subscribe();
            loop {
                match events.recv().await {
                    Ok(event) => service.observe(event).await,
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        tracing::debug!(missed, "agent alert watcher skipped thread events");
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        }));
    }

    /// Handles one thread event.
    async fn observe(self: &Arc<Self>, wire: WireEvent) {
        if !self.settings.get().await.enabled {
            return;
        }
        match wire.event {
            Event::ThreadUpserted(summary) => {
                if summary.tab == ThreadTab::Working {
                    // A new turn is a new set of transitions: its result can
                    // alert even when it repeats the previous one. The turn id
                    // stays: it is what keeps one turn start from publishing
                    // `working` twice.
                    let mut published = self.published.lock().await;
                    if let Some(alerts) = published.get_mut(&summary.thread_id) {
                        alerts.published = None;
                        alerts.limit_alerted = false;
                    }
                }
            }
            Event::TurnStarted {
                thread_id, turn_id, ..
            } => {
                // A Live Update is published once per turn start, not for the
                // many thread summaries, deltas and tool calls inside it.
                let first_start = {
                    let mut published = self.published.lock().await;
                    let alerts = published.entry(thread_id.clone()).or_default();
                    if alerts.working_turn.as_ref() == Some(&turn_id) {
                        false
                    } else {
                        alerts.working_turn = Some(turn_id.clone());
                        true
                    }
                };
                if !first_start {
                    return;
                }
                let Some(thread) = self.alertable_thread(&thread_id).await else {
                    return;
                };
                self.publish(&thread, AgentAlertState::Working, Utc::now())
                    .await;
            }
            Event::ApprovalRequested {
                thread_id, kind, ..
            } => {
                let Some(thread) = self.alertable_thread(&thread_id).await else {
                    return;
                };
                let state = approval_state(kind);
                self.publish(&thread, state, Utc::now()).await;
            }
            Event::ApprovalResolved { thread_id, .. } => {
                // Once answered, the same hand can be raised again later in
                // the same turn and should alert again.
                self.published.lock().await.remove(&thread_id);
            }
            Event::UsageLimitHit {
                thread_id,
                auto_resume,
                ..
            } => {
                let Some(state) = limit_alert(auto_resume) else {
                    return;
                };
                let Some(thread) = self.alertable_thread(&thread_id).await else {
                    return;
                };
                if self.publish(&thread, state, Utc::now()).await {
                    self.published
                        .lock()
                        .await
                        .entry(thread_id)
                        .or_default()
                        .limit_alerted = true;
                }
            }
            Event::TurnCompleted {
                thread_id, status, ..
            } => {
                let limit_stop = self
                    .published
                    .lock()
                    .await
                    .get(&thread_id)
                    .is_some_and(|alerts| alerts.limit_alerted);
                if limit_stop {
                    if let Some(alerts) = self.published.lock().await.get_mut(&thread_id) {
                        alerts.limit_alerted = false;
                    }
                    // The limit alert already told the person; this completion
                    // is the same stop, not a separate failure or finish.
                    if completion_alert(status) != Some(AgentAlertState::Limit) {
                        return;
                    }
                }
                let Some(state) = completion_alert(status) else {
                    return;
                };
                let Some(thread) = self.alertable_thread(&thread_id).await else {
                    return;
                };
                self.publish(&thread, state, Utc::now()).await;
                if let Some(alerts) = self.published.lock().await.get_mut(&thread_id) {
                    alerts.limit_alerted = false;
                }
            }
            _ => {}
        }
    }

    /// The thread an alert may name, or `None` for a subagent's thread.
    async fn alertable_thread(&self, thread_id: &ThreadId) -> Option<ActivityThread> {
        let summary = self.manager.as_ref()?.thread_summary(thread_id).await?;
        if summary.parent_thread_id.is_some() {
            return None;
        }
        let roots = self
            .worktree_roots
            .as_ref()
            .map(WorktreeRoots::roots_list)
            .unwrap_or_default();
        Some(ActivityThread::from_summary(&summary, &roots))
    }

    /// Publishes one transition if the switch is on and it is new.
    ///
    /// Returns whether it was handed to the relay.
    pub async fn publish(
        &self,
        thread: &ActivityThread,
        state: AgentAlertState,
        at: DateTime<Utc>,
    ) -> bool {
        if !self.settings.get().await.enabled {
            return false;
        }
        {
            let published = self.published.lock().await;
            if published
                .get(&thread.id)
                .and_then(|alerts| alerts.published)
                == Some(state)
            {
                return false;
            }
        }
        let activity = AgentActivity {
            thread_id: thread.id.clone(),
            thread_title: thread.title.clone(),
            project_name: thread.project.clone(),
            state,
            at,
            transition: uuid::Uuid::now_v7().to_string(),
        };
        if !self.publisher.publish(activity).await {
            return false;
        }
        self.published
            .lock()
            .await
            .entry(thread.id.clone())
            .or_default()
            .published = Some(state);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Recorder {
        sent: Mutex<Vec<AgentActivity>>,
    }

    #[async_trait::async_trait]
    impl ActivityPublisher for Recorder {
        async fn publish(&self, activity: AgentActivity) -> bool {
            self.sent.lock().await.push(activity);
            true
        }
    }

    fn thread() -> ActivityThread {
        ActivityThread {
            id: ThreadId::new("th_test"),
            title: "Fix the thing".into(),
            project: "h4rn".into(),
        }
    }

    /// A table of threads the observation tests can name.
    #[derive(Default)]
    struct Lookup {
        summaries: HashMap<ThreadId, ThreadSummary>,
    }

    #[async_trait::async_trait]
    impl ThreadLookup for Lookup {
        async fn thread_summary(&self, thread_id: &ThreadId) -> Option<ThreadSummary> {
            self.summaries.get(thread_id).cloned()
        }

        fn subscribe(&self) -> broadcast::Receiver<WireEvent> {
            // These tests call `observe` directly; no watcher is started.
            broadcast::channel(1).0.subscribe()
        }
    }

    fn summary(thread_id: &ThreadId, parent: Option<&str>) -> ThreadSummary {
        use h4rn_protocol::{AgentState, HarnessKind, HostId, RuntimeMode, ThreadTab, Usage};

        ThreadSummary {
            thread_id: thread_id.clone(),
            parent_thread_id: parent.map(ThreadId::new),
            host: HostId::new("host_test"),
            harness: HarnessKind::ClaudeCode,
            imported_from: None,
            agent_id: None,
            title: "Fix the thing".into(),
            workspace_path: "/work/h4rn".into(),
            project_path: None,
            branch: None,
            model: "test-model".into(),
            runtime_mode: RuntimeMode::ApprovalRequired,
            reasoning_effort: None,
            reasoning_efforts: Vec::new(),
            models: Vec::new(),
            fast: false,
            fast_models: None,
            fast_available: None,
            tab: ThreadTab::Working,
            placed_by_user: false,
            pinned: false,
            snoozed_until: None,
            last_activity: Utc::now(),
            agent_state: AgentState::Done,
            usage: Usage::default(),
            ttft_ms: None,
            context_window: None,
            context_fill_percent: None,
            needs_attention: false,
            pending_guest_requests: 0,
            unread: false,
            resume_at: None,
            limit: None,
            queued_count: 0,
            queue_revision: 0,
            active_turn_id: None,
            steer: true,
            forked_from: None,
            rewind: h4rn_protocol::RewindMethod::Unsupported,
            file_rewind: false,
        }
    }

    /// A service that can name one thread and records what it publishes.
    fn observing_service(recorder: Arc<Recorder>) -> Arc<AgentAlertService> {
        let thread_id = ThreadId::new("th_test");
        let mut lookup = Lookup::default();
        lookup
            .summaries
            .insert(thread_id.clone(), summary(&thread_id, None));
        let store = Arc::new(AgentAlertSettingsStore::in_memory());
        Arc::new(AgentAlertService::new(
            store,
            Some(Arc::new(lookup) as Arc<dyn ThreadLookup>),
            recorder as Arc<dyn ActivityPublisher>,
            None,
        ))
    }

    /// Sends one event through the real observation path.
    async fn observe(service: &Arc<AgentAlertService>, event: Event) {
        let thread_id = match &event {
            Event::ThreadUpserted(summary) => summary.thread_id.clone(),
            _ => ThreadId::new("th_test"),
        };
        service
            .observe(WireEvent {
                seq: 0,
                thread_id,
                event,
            })
            .await;
    }

    fn working_event() -> Event {
        let thread_id = ThreadId::new("th_test");
        Event::ThreadUpserted(summary(&thread_id, None))
    }

    fn turn_started(turn_id: &str) -> Event {
        Event::TurnStarted {
            thread_id: ThreadId::new("th_test"),
            turn_id: h4rn_protocol::TurnId::new(turn_id),
            started_at: Utc::now(),
            harness: None,
        }
    }

    fn completion(status: TurnStatus) -> Event {
        Event::TurnCompleted {
            thread_id: ThreadId::new("th_test"),
            turn_id: h4rn_protocol::TurnId::new("turn_1"),
            usage: h4rn_protocol::Usage::default(),
            status,
            changes: None,
        }
    }

    fn approval_requested() -> Event {
        Event::ApprovalRequested {
            thread_id: ThreadId::new("th_test"),
            turn_id: h4rn_protocol::TurnId::new("turn_1"),
            approval_id: "approval_1".into(),
            subagent_id: None,
            kind: h4rn_protocol::ApprovalKind::Command,
            prompt: "Run the tests?".into(),
            options: Vec::new(),
        }
    }

    fn approval_resolved() -> Event {
        Event::ApprovalResolved {
            thread_id: ThreadId::new("th_test"),
            approval_id: "approval_1".into(),
            option_id: None,
            by_device: None,
        }
    }

    fn limit_hit(auto_resume: bool) -> Event {
        Event::UsageLimitHit {
            thread_id: ThreadId::new("th_test"),
            reset_at: None,
            auto_resume,
        }
    }

    /// A service with no manager: these tests drive `publish` directly.
    fn service(_settings: AgentAlertSettings, recorder: Arc<Recorder>) -> AgentAlertService {
        let store = Arc::new(AgentAlertSettingsStore::in_memory());
        AgentAlertService::new(store, None, recorder as Arc<dyn ActivityPublisher>, None)
    }

    #[tokio::test]
    async fn nothing_is_published_while_the_switch_is_off() {
        let recorder = Arc::new(Recorder::default());
        let service = service(AgentAlertSettings::default(), Arc::clone(&recorder));
        assert!(
            !service
                .publish(&thread(), AgentAlertState::Completed, Utc::now())
                .await
        );
        assert!(recorder.sent.lock().await.is_empty());
    }

    #[tokio::test]
    async fn the_documented_fields_are_the_only_ones_published() {
        let recorder = Arc::new(Recorder::default());
        let service = service(AgentAlertSettings::default(), Arc::clone(&recorder));
        service.set_enabled(true).await.expect("enable");
        assert!(
            service
                .publish(&thread(), AgentAlertState::Completed, Utc::now())
                .await
        );
        let sent = recorder.sent.lock().await;
        assert_eq!(sent.len(), 1);
        let json = serde_json::to_value(&sent[0]).expect("json");
        let object = json.as_object().expect("object");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "at",
                "project_name",
                "state",
                "thread_id",
                "thread_title",
                "transition"
            ]
        );
        assert_eq!(object["thread_title"], "Fix the thing");
        assert_eq!(
            object["transition"].as_str().map(str::len),
            Some(36),
            "each transition carries its own identity"
        );
    }

    #[tokio::test]
    async fn the_same_state_is_not_published_twice() {
        let recorder = Arc::new(Recorder::default());
        let service = service(AgentAlertSettings::default(), Arc::clone(&recorder));
        service.set_enabled(true).await.expect("enable");
        let thread = thread();
        assert!(
            service
                .publish(&thread, AgentAlertState::Approval, Utc::now())
                .await
        );
        assert!(
            !service
                .publish(&thread, AgentAlertState::Approval, Utc::now())
                .await
        );
        assert_eq!(recorder.sent.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn a_different_state_alerts_again() {
        let recorder = Arc::new(Recorder::default());
        let service = service(AgentAlertSettings::default(), Arc::clone(&recorder));
        service.set_enabled(true).await.expect("enable");
        let thread = thread();
        assert!(
            service
                .publish(&thread, AgentAlertState::Approval, Utc::now())
                .await
        );
        assert!(
            service
                .publish(&thread, AgentAlertState::Failed, Utc::now())
                .await
        );
        assert_eq!(recorder.sent.lock().await.len(), 2);
    }

    #[tokio::test]
    async fn settings_default_to_off() {
        let store = AgentAlertSettingsStore::in_memory();
        assert!(!store.get().await.enabled);
    }

    #[test]
    fn only_a_real_finished_turn_alerts() {
        use h4rn_protocol::TurnStatus;

        assert_eq!(
            completion_alert(TurnStatus::Completed),
            Some(AgentAlertState::Completed)
        );
        assert_eq!(
            completion_alert(TurnStatus::Failed),
            Some(AgentAlertState::Failed)
        );
        // A Stop ends the turn with an error state, but the person did it.
        assert_eq!(completion_alert(TurnStatus::Interrupted), None);
        // A limit stop is a request for the person, not "finished".
        assert_eq!(
            completion_alert(TurnStatus::TokenLimit),
            Some(AgentAlertState::Limit)
        );
        assert_eq!(
            completion_alert(TurnStatus::TurnLimit),
            Some(AgentAlertState::Limit)
        );
    }

    #[test]
    fn a_pause_alerts_only_when_nobody_resumes_it() {
        assert_eq!(limit_alert(true), None);
        assert_eq!(limit_alert(false), Some(AgentAlertState::Limit));
    }

    #[test]
    fn a_question_is_input_and_a_permission_is_approval() {
        use h4rn_protocol::ApprovalKind;

        assert_eq!(
            approval_state(ApprovalKind::Question),
            AgentAlertState::Input
        );
        assert_eq!(
            approval_state(ApprovalKind::Command),
            AgentAlertState::Approval
        );
        assert_eq!(
            approval_state(ApprovalKind::Plan),
            AgentAlertState::Approval
        );
    }

    #[tokio::test]
    async fn a_turn_start_publishes_working_once_and_never_per_event() {
        let recorder = Arc::new(Recorder::default());
        let service = observing_service(Arc::clone(&recorder));
        service.set_enabled(true).await.expect("enable");

        // The same start seen again — even after a summary reset the published
        // baseline — plus the work inside the turn, must not publish another
        // Live Update. The turn id, not the last state, is what remembers it.
        observe(&service, turn_started("turn_1")).await;
        observe(&service, working_event()).await;
        observe(&service, turn_started("turn_1")).await;
        observe(&service, working_event()).await;
        observe(&service, working_event()).await;

        let sent = recorder.sent.lock().await;
        assert_eq!(sent.len(), 1, "one turn start published more than once");
        assert_eq!(sent[0].state, AgentAlertState::Working);
        assert_eq!(sent[0].thread_id, ThreadId::new("th_test"));
        assert_eq!(sent[0].thread_title, "Fix the thing");
        assert_eq!(sent[0].project_name, "h4rn");
    }

    #[tokio::test]
    async fn a_later_turn_publishes_working_again() {
        let recorder = Arc::new(Recorder::default());
        let service = observing_service(Arc::clone(&recorder));
        service.set_enabled(true).await.expect("enable");

        observe(&service, turn_started("turn_1")).await;
        observe(&service, completion(TurnStatus::Completed)).await;
        observe(&service, turn_started("turn_2")).await;

        let sent = recorder.sent.lock().await;
        let states: Vec<AgentAlertState> = sent.iter().map(|activity| activity.state).collect();
        assert_eq!(
            states,
            vec![
                AgentAlertState::Working,
                AgentAlertState::Completed,
                AgentAlertState::Working
            ],
            "the second turn's start was not its own transition"
        );
    }

    #[tokio::test]
    async fn a_turn_start_in_a_subagent_thread_publishes_nothing() {
        let recorder = Arc::new(Recorder::default());
        let thread_id = ThreadId::new("th_test");
        let mut lookup = Lookup::default();
        lookup
            .summaries
            .insert(thread_id.clone(), summary(&thread_id, Some("th_parent")));
        let store = Arc::new(AgentAlertSettingsStore::in_memory());
        let service = Arc::new(AgentAlertService::new(
            store,
            Some(Arc::new(lookup) as Arc<dyn ThreadLookup>),
            Arc::clone(&recorder) as Arc<dyn ActivityPublisher>,
            None,
        ));
        service.set_enabled(true).await.expect("enable");

        observe(&service, turn_started("turn_1")).await;

        assert!(
            recorder.sent.lock().await.is_empty(),
            "a subagent's own thread named itself instead of its parent"
        );
    }

    #[tokio::test]
    async fn a_second_finished_turn_alerts_again() {
        let recorder = Arc::new(Recorder::default());
        let service = observing_service(Arc::clone(&recorder));
        service.set_enabled(true).await.expect("enable");

        observe(&service, working_event()).await;
        observe(&service, completion(TurnStatus::Completed)).await;
        observe(&service, working_event()).await;
        observe(&service, completion(TurnStatus::Completed)).await;

        let sent = recorder.sent.lock().await;
        assert_eq!(
            sent.len(),
            2,
            "a new Working turn clears the baseline so its result can alert"
        );
        assert!(
            sent.iter()
                .all(|activity| activity.state == AgentAlertState::Completed)
        );
    }

    #[tokio::test]
    async fn a_second_approval_in_the_same_turn_alerts_again() {
        let recorder = Arc::new(Recorder::default());
        let service = observing_service(Arc::clone(&recorder));
        service.set_enabled(true).await.expect("enable");

        observe(&service, approval_requested()).await;
        observe(&service, approval_resolved()).await;
        observe(&service, approval_requested()).await;

        let sent = recorder.sent.lock().await;
        assert_eq!(
            sent.len(),
            2,
            "answering a hand clears the baseline so the next hand alerts"
        );
        assert!(
            sent.iter()
                .all(|activity| activity.state == AgentAlertState::Approval)
        );
    }

    #[tokio::test]
    async fn a_limit_stop_alerts_once_and_its_completion_stays_quiet() {
        let recorder = Arc::new(Recorder::default());
        let service = observing_service(Arc::clone(&recorder));
        service.set_enabled(true).await.expect("enable");

        observe(&service, limit_hit(false)).await;
        observe(&service, completion(TurnStatus::Failed)).await;

        let sent = recorder.sent.lock().await;
        assert_eq!(
            sent.len(),
            1,
            "the limit alert is the person's cue; the Failed completion is the same stop"
        );
        assert_eq!(sent[0].state, AgentAlertState::Limit);
    }

    #[tokio::test]
    async fn a_turn_the_person_stopped_publishes_nothing() {
        let recorder = Arc::new(Recorder::default());
        let service = observing_service(Arc::clone(&recorder));
        service.set_enabled(true).await.expect("enable");

        observe(&service, working_event()).await;
        observe(&service, completion(TurnStatus::Interrupted)).await;

        assert!(recorder.sent.lock().await.is_empty());
    }

    /// A linked checkout at `<root>/<project>/<branch>`: its `.git` is a file.
    fn linked_checkout(root: &Path, project: &str, branch: &str) -> String {
        let checkout = root.join(project).join(branch);
        std::fs::create_dir_all(&checkout).expect("checkout");
        std::fs::write(checkout.join(".git"), "gitdir: /elsewhere\n").expect("gitdir file");
        checkout.to_string_lossy().into_owned()
    }

    #[test]
    fn a_worktree_thread_alerts_with_its_project_name() {
        let data = tempfile::tempdir().expect("data");
        let root = data.path().join("worktrees");
        let mut summary = summary(&ThreadId::new("th_worktree"), None);
        summary.workspace_path = linked_checkout(&root, "h4rn", "fix-the-login");
        let roots = vec![root];

        let worktree = ActivityThread::from_summary(&summary, &roots);
        assert_eq!(
            worktree.project, "h4rn",
            "the alert named the branch folder instead of the project"
        );

        // A project_path (an import or fork) is already the checkout itself.
        summary.project_path = Some("/code/h4rn".into());
        assert_eq!(
            ActivityThread::from_summary(&summary, &roots).project,
            "h4rn"
        );

        // Without a known root there is nothing better than the folder name.
        summary.project_path = None;
        assert_eq!(
            ActivityThread::from_summary(&summary, &[]).project,
            "fix-the-login"
        );
    }

    #[test]
    fn a_custom_worktree_location_still_names_the_project() {
        // Settings → Worktrees can put checkouts anywhere; the alert must read
        // the configured location rather than the default data folder.
        let fast = tempfile::tempdir().expect("location");
        let mut summary = summary(&ThreadId::new("th_worktree"), None);
        summary.workspace_path = linked_checkout(fast.path(), "h4rn", "fix-the-login");
        let roots = WorktreeRoots::fixed(vec![fast.path().to_path_buf()]);
        let roots = roots.roots_list();

        assert_eq!(
            ActivityThread::from_summary(&summary, &roots).project,
            "h4rn",
            "a custom location made the alert name the branch folder"
        );
    }

    #[test]
    fn an_ordinary_checkout_under_a_broad_location_keeps_its_name() {
        // A location such as `~/code` also holds ordinary clones two folders
        // down (`~/code/acme/api`). Only a linked checkout is a worktree.
        let code = tempfile::tempdir().expect("location");
        let api = code.path().join("acme").join("api");
        std::fs::create_dir_all(api.join(".git")).expect("a clone's .git directory");
        let mut summary = summary(&ThreadId::new("th_clone"), None);
        summary.workspace_path = api.to_string_lossy().into_owned();
        let roots = vec![code.path().to_path_buf()];

        assert_eq!(
            ActivityThread::from_summary(&summary, &roots).project,
            "api",
            "an ordinary clone was named after its parent folder"
        );
    }

    #[test]
    fn the_configured_location_is_read_when_the_alert_is_built() {
        let settings = Arc::new(crate::worktrees::settings::WorktreeSettingsStore::in_memory());
        let roots = WorktreeRoots::configured(Arc::clone(&settings), PathBuf::from("/data"));
        assert_eq!(
            roots.roots_list(),
            vec![PathBuf::from("/data/worktrees")],
            "the default root is the fallback before a location is set"
        );

        let fast = tempfile::tempdir().expect("location");
        settings
            .set(h4rn_protocol::WorktreeSettings {
                location: Some(fast.path().to_string_lossy().into_owned()),
                ..Default::default()
            })
            .expect("save the location");

        let mut summary = summary(&ThreadId::new("th_worktree"), None);
        summary.workspace_path = linked_checkout(fast.path(), "h4rn", "fix-the-login");
        let roots = roots.roots_list();
        assert_eq!(roots.first(), Some(&fast.path().to_path_buf()));
        assert_eq!(
            ActivityThread::from_summary(&summary, &roots).project,
            "h4rn"
        );
    }
}
