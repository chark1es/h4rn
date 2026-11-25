//! Alert selection and delivery for Android push (ADR-051).
//!
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
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use h4rn_protocol::{AgentActivity, AgentAlertState};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::push::{FcmClient, PushMessage, SendError};
use crate::store::Store;

/// The most title text one notification line carries.
const MAX_LINE_CHARS: usize = 120;

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
    /// Number of threads represented.
    pub count: usize,
}

impl PushAlert {
    /// The string-only data payload FCM carries.
    pub fn data(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("h4rn_alert".to_string(), "agent".to_string()),
            ("state".to_string(), self.state.as_str().to_string()),
            ("thread_id".to_string(), self.thread_id.clone()),
            ("title".to_string(), self.title.clone()),
            ("body".to_string(), self.body.clone()),
            ("count".to_string(), self.count.to_string()),
        ])
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
        count: 1,
    }
}

fn grouped(activities: &[AgentActivity]) -> PushAlert {
    let needs_attention = activities
        .iter()
        .all(|activity| activity.state.needs_attention());
    let mut titles: Vec<String> = Vec::new();
    for activity in activities {
        let title = clean(&activity.thread_title, "A thread");
        if !titles.contains(&title) {
            titles.push(title);
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
        count,
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
    let activities = latest_per_thread(activities);
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
    scheduled: bool,
}

/// The relay-side alert service.
pub struct AlertService {
    store: Store,
    fcm: Arc<FcmClient>,
    group_window: Duration,
    max_age: Duration,
    pending: Mutex<HashMap<Uuid, PendingGroup>>,
    /// How many transitions reached `ingest`, so a relay test can prove a
    /// gated frame never got this far.
    #[cfg(test)]
    pub(crate) ingested: std::sync::atomic::AtomicUsize,
}

impl AlertService {
    /// Builds the service around the store and the FCM sender.
    pub fn new(
        store: Store,
        fcm: Arc<FcmClient>,
        group_window: Duration,
        max_age: Duration,
    ) -> Self {
        Self {
            store,
            fcm,
            group_window,
            max_age,
            pending: Mutex::new(HashMap::new()),
            #[cfg(test)]
            ingested: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Records one transition and schedules a grouped flush.
    ///
    /// Runs after the sender is dropped as a fire-and-forget task; callers on
    /// the relay loop never wait for FCM. The baseline is written first, so a
    /// duplicate published while the group is still collecting is dropped.
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
        if !self.fcm.configured() {
            return;
        }
        let schedule = {
            let mut pending = self.pending.lock().await;
            let group = pending.entry(user_id).or_default();
            group.activities.push(activity);
            if group.scheduled {
                false
            } else {
                group.scheduled = true;
                true
            }
        };
        if schedule {
            let service = Arc::clone(self);
            let window = self.group_window;
            tokio::spawn(async move {
                tokio::time::sleep(window).await;
                service.flush(user_id).await;
            });
        }
    }

    /// Sends the alerts for everything collected for one user.
    ///
    /// Public so tests can drain without waiting for the window.
    pub async fn flush(&self, user_id: Uuid) {
        let activities = {
            let mut pending = self.pending.lock().await;
            match pending.remove(&user_id) {
                Some(group) => group.activities,
                None => return,
            }
        };
        let alerts = group_activities(&activities);
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
            let data = alert.data();
            for device in &devices {
                let message = PushMessage {
                    token: &device.token,
                    package_name: device.package_name.as_deref(),
                    data: data.clone(),
                };
                match self.fcm.send(&message).await {
                    Ok(()) => {}
                    Err(SendError::Unregistered) => {
                        if let Err(error) =
                            self.store.delete_push_device_by_token(&device.token).await
                        {
                            tracing::warn!(%error, "could not delete an unregistered token");
                        }
                    }
                    Err(SendError::Disabled) => return,
                    Err(SendError::Failed(error)) => {
                        tracing::warn!(%error, device = %device.id, "push delivery failed");
                    }
                }
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
    fn push_data_carries_only_the_documented_fields() {
        let alert = single(&activity(AgentAlertState::Completed, "one"), "proj");
        let data = alert.data();
        let keys: Vec<&str> = data.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec!["body", "count", "h4rn_alert", "state", "thread_id", "title"]
        );
        assert_eq!(data["h4rn_alert"], "agent");
        assert_eq!(data["state"], "completed");
    }
}
