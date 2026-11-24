//! Runner, persistence and validation tests for scheduled tasks.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use h4rn_harness::FakeHarness;
use h4rn_protocol::{
    AgentState, HarnessKind, HostId, IntervalUnit, RuntimeMode, ScheduleDestination, ScheduleDraft,
    ScheduleRunOutcome, ScheduleSpec, ScheduleTaskId, ThreadId, ThreadTab,
};

use super::*;
use crate::harnesses::StaticHarnessCatalog;
use crate::journal::SqliteJournal;
use crate::threads::ThreadManagerOptions;

/// A clock only the test moves.
#[derive(Debug)]
struct ManualClock {
    wall: Mutex<DateTime<Utc>>,
    mono: Mutex<Duration>,
    base: Instant,
}

impl ManualClock {
    fn at(at: DateTime<Utc>) -> Self {
        Self {
            wall: Mutex::new(at),
            mono: Mutex::new(Duration::ZERO),
            base: Instant::now(),
        }
    }

    fn advance(&self, by: chrono::Duration) {
        *self.wall.lock().unwrap() += by;
        if let Ok(by) = by.to_std() {
            *self.mono.lock().unwrap() += by;
        }
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.wall.lock().unwrap()
    }

    fn monotonic(&self) -> Instant {
        self.base + *self.mono.lock().unwrap()
    }
}

struct Fixture {
    _workspace: tempfile::TempDir,
    manager: Arc<ThreadManager>,
    service: ScheduleService,
    clock: Arc<ManualClock>,
    journal: Arc<dyn Journal>,
    workspace_path: PathBuf,
}

async fn fixture(harness: FakeHarness, clock: Arc<ManualClock>) -> Fixture {
    fixture_with_journal(harness, clock, None).await
}

async fn fixture_with_journal(
    harness: FakeHarness,
    clock: Arc<ManualClock>,
    journal: Option<Arc<dyn Journal>>,
) -> Fixture {
    fixture_with_harnesses(vec![harness], clock, journal).await
}

async fn fixture_with_harnesses(
    harnesses: Vec<FakeHarness>,
    clock: Arc<ManualClock>,
    journal: Option<Arc<dyn Journal>>,
) -> Fixture {
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_path = workspace.path().to_path_buf();
    let journal = match journal {
        Some(journal) => journal,
        None => Arc::new(SqliteJournal::in_memory().await.expect("journal")),
    };
    let host = HostId::new("host_schedule_test");
    let mut catalog = StaticHarnessCatalog::new();
    for harness in harnesses {
        catalog.register(Arc::new(harness));
    }
    let manager = Arc::new(
        ThreadManager::open(
            Arc::clone(&journal),
            Arc::new(catalog),
            host.clone(),
            ThreadManagerOptions {
                tick_interval: Duration::from_secs(3600),
                clock: Some(clock.clone()),
                ..ThreadManagerOptions::default()
            },
            None,
        )
        .await
        .expect("manager"),
    );
    let service = ScheduleService::open(
        Arc::clone(&journal),
        Arc::clone(&manager),
        host,
        clock.clone(),
    )
    .await
    .expect("schedule service");
    Fixture {
        _workspace: workspace,
        manager,
        service,
        clock,
        journal,
        workspace_path,
    }
}

fn draft(
    workspace: &std::path::Path,
    schedule: ScheduleSpec,
    destination: ScheduleDestination,
) -> ScheduleDraft {
    ScheduleDraft {
        name: "Nightly review".into(),
        host: HostId::new("host_schedule_test"),
        workspace_path: workspace.display().to_string(),
        harness: HarnessKind::ClaudeCode,
        agent_id: None,
        model: "fake-model".into(),
        runtime_mode: RuntimeMode::ApprovalRequired,
        prompt: "Review the diff and report.".into(),
        schedule,
        timezone: "Europe/Berlin".into(),
        destination,
        enabled: true,
    }
}

fn every_minute() -> ScheduleSpec {
    ScheduleSpec::Every {
        count: 1,
        unit: IntervalUnit::Minutes,
    }
}

async fn wait_idle(manager: &ThreadManager, thread_id: &ThreadId) -> bool {
    tokio::time::timeout(Duration::from_secs(5), async {
        while manager.thread_is_busy(thread_id).await {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .is_ok()
}

async fn wait_runs(
    service: &ScheduleService,
    task: &ScheduleTaskId,
    count: usize,
) -> Vec<ScheduleRun> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let runs = service.runs(task).await.expect("runs");
            if runs.len() >= count {
                return runs;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("run history did not reach the expected length")
}

/// The two-phase delivery a server performs, for tests.
async fn deliver(
    service: &ScheduleService,
    secret: &str,
    request: WebhookRequest,
) -> WebhookOutcome {
    match service.begin_webhook(secret).await {
        WebhookStart::Start(admission) => service.deliver_webhook(*admission, request).await,
        WebhookStart::NotFound => WebhookOutcome::NotFound,
        WebhookStart::RateLimited => WebhookOutcome::RateLimited,
        WebhookStart::Skipped => WebhookOutcome::Skipped,
        WebhookStart::Rejected(message) => WebhookOutcome::Rejected(message),
    }
}

#[tokio::test]
async fn a_new_thread_task_starts_a_new_thread_each_run() {
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture(FakeHarness::new(), clock).await;
    let task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            every_minute(),
            ScheduleDestination::NewThread,
        ))
        .await
        .expect("create");

    fixture.clock.advance(chrono::Duration::seconds(61));
    fixture.service.run_due().await;
    let runs = wait_runs(&fixture.service, &task.task_id, 1).await;
    let first = runs[0].thread_id.clone().expect("first thread");
    assert_eq!(runs[0].outcome, ScheduleRunOutcome::Started);
    assert!(wait_idle(&fixture.manager, &first).await);
    let live = fixture.service.task(&task.task_id).await.expect("task");
    assert_eq!(
        live.last_run.as_ref().map(|run| run.run_id),
        Some(runs[0].run_id),
        "the live last run carries the id the journal assigned"
    );

    fixture.clock.advance(chrono::Duration::seconds(61));
    fixture.service.run_due().await;
    let runs = wait_runs(&fixture.service, &task.task_id, 2).await;
    let second = runs[0].thread_id.clone().expect("second thread");
    assert_ne!(
        first, second,
        "a new thread each run must not reuse the old one"
    );

    let task = fixture.service.task(&task.task_id).await.expect("task");
    assert!(task.thread_id.is_none(), "new-thread tasks own no thread");
    assert!(task.next_run_at.is_some());
}

#[tokio::test]
async fn a_continuing_task_keeps_one_thread_and_its_session() {
    let (harness, sessions) = FakeHarness::new().with_session_count();
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture(harness, clock).await;
    let task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            every_minute(),
            ScheduleDestination::ContinueThread,
        ))
        .await
        .expect("create");

    fixture.clock.advance(chrono::Duration::seconds(61));
    fixture.service.run_due().await;
    let runs = wait_runs(&fixture.service, &task.task_id, 1).await;
    let thread_id = runs[0].thread_id.clone().expect("thread");
    assert!(wait_idle(&fixture.manager, &thread_id).await);
    sessions.wait_for(1).await;

    fixture.clock.advance(chrono::Duration::seconds(61));
    fixture.service.run_due().await;
    let runs = wait_runs(&fixture.service, &task.task_id, 2).await;
    assert_eq!(runs[0].thread_id.as_ref(), Some(&thread_id));
    assert!(wait_idle(&fixture.manager, &thread_id).await);
    assert_eq!(sessions.get(), 1, "continuing keeps the live session");

    let task = fixture.service.task(&task.task_id).await.expect("task");
    assert_eq!(task.thread_id.as_ref(), Some(&thread_id));
}

#[tokio::test]
async fn a_fresh_session_task_keeps_the_thread_but_starts_a_new_session() {
    let (harness, sessions) = FakeHarness::new().with_session_count();
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture(harness, clock).await;
    let task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            every_minute(),
            ScheduleDestination::FreshSession,
        ))
        .await
        .expect("create");

    fixture.clock.advance(chrono::Duration::seconds(61));
    fixture.service.run_due().await;
    let runs = wait_runs(&fixture.service, &task.task_id, 1).await;
    let thread_id = runs[0].thread_id.clone().expect("thread");
    assert!(wait_idle(&fixture.manager, &thread_id).await);
    sessions.wait_for(1).await;

    fixture.clock.advance(chrono::Duration::seconds(61));
    fixture.service.run_due().await;
    let runs = wait_runs(&fixture.service, &task.task_id, 2).await;
    assert_eq!(runs[0].thread_id.as_ref(), Some(&thread_id));
    tokio::time::timeout(Duration::from_secs(5), sessions.wait_for(2))
        .await
        .expect("a fresh session should start");
}

#[tokio::test]
async fn a_busy_previous_run_skips_the_slot_and_records_it() {
    let (harness, gate) = FakeHarness::new().with_completion_gate();
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture(harness, clock).await;
    let task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            every_minute(),
            ScheduleDestination::NewThread,
        ))
        .await
        .expect("create");

    fixture.clock.advance(chrono::Duration::seconds(61));
    fixture.service.run_due().await;
    let runs = wait_runs(&fixture.service, &task.task_id, 1).await;
    let thread_id = runs[0].thread_id.clone().expect("thread");
    assert!(fixture.manager.thread_is_busy(&thread_id).await);

    fixture.clock.advance(chrono::Duration::seconds(61));
    fixture.service.run_due().await;
    let runs = wait_runs(&fixture.service, &task.task_id, 2).await;
    assert_eq!(runs[0].outcome, ScheduleRunOutcome::Skipped);
    assert_eq!(runs[0].thread_id.as_ref(), Some(&thread_id));
    assert!(
        runs[0]
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("still working"))
    );
    assert_eq!(
        fixture
            .manager
            .list_threads(&Default::default())
            .await
            .len(),
        1,
        "a skipped slot starts nothing"
    );
    gate.release();
}

#[tokio::test]
async fn a_missed_slot_after_downtime_runs_once_and_survives_a_restart() {
    let dir = tempfile::tempdir().expect("dir");
    let db = dir.path().join("journal.db");
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let first = fixture_with_journal(
        FakeHarness::new(),
        clock.clone(),
        Some(Arc::new(
            SqliteJournal::connect(&db).await.expect("journal"),
        )),
    )
    .await;
    let task = first
        .service
        .create(draft(
            &first.workspace_path,
            every_minute(),
            ScheduleDestination::NewThread,
        ))
        .await
        .expect("create");
    let next = task.next_run_at.expect("next run");
    // The daemon is down for ten slots.
    clock.advance(chrono::Duration::seconds(10 * 60 + 1));
    drop(first);

    let second = fixture_with_journal(
        FakeHarness::new(),
        clock.clone(),
        Some(Arc::new(
            SqliteJournal::connect(&db).await.expect("journal"),
        )),
    )
    .await;
    let restored = second
        .service
        .task(&task.task_id)
        .await
        .expect("task survived the restart");
    assert_eq!(restored.name, "Nightly review");
    assert_eq!(restored.next_run_at, Some(next));

    second.service.run_due().await;
    let runs = wait_runs(&second.service, &task.task_id, 1).await;
    assert_eq!(runs.len(), 1, "ten missed slots collapse into one run");
    assert_eq!(runs[0].outcome, ScheduleRunOutcome::Started);
    let after = second.service.task(&task.task_id).await.expect("task");
    assert!(after.next_run_at.is_some_and(|at| at > clock.now()));
    second.service.run_due().await;
    assert_eq!(
        second
            .service
            .runs(&task.task_id)
            .await
            .expect("runs")
            .len(),
        1
    );
}

#[tokio::test]
async fn run_now_records_a_manual_run_and_refuses_webhook_tasks() {
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture(FakeHarness::new(), clock).await;
    let task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            every_minute(),
            ScheduleDestination::NewThread,
        ))
        .await
        .expect("create");
    let run = fixture
        .service
        .run_now(&task.task_id)
        .await
        .expect("manual run");
    assert_eq!(run.outcome, ScheduleRunOutcome::Started);

    let webhook = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            ScheduleSpec::Webhook,
            ScheduleDestination::NewThread,
        ))
        .await
        .expect("webhook task");
    let error = fixture
        .service
        .run_now(&webhook.task_id)
        .await
        .expect_err("webhook tasks only run on a call");
    assert!(error.to_string().contains("webhook"));
}

#[tokio::test]
async fn a_webhook_delivery_starts_a_run_and_rotation_kills_the_old_path() {
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture(FakeHarness::new(), clock).await;
    let task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            ScheduleSpec::Webhook,
            ScheduleDestination::NewThread,
        ))
        .await
        .expect("webhook task");
    let old_path = task.webhook_path.clone().expect("webhook path");
    let old_secret = old_path
        .strip_prefix(WEBHOOK_PREFIX)
        .expect("prefix")
        .to_owned();

    let mut headers = std::collections::BTreeMap::new();
    headers.insert("x-event".to_owned(), "push".to_owned());
    let request = WebhookRequest {
        method: "POST".into(),
        path: old_path.clone(),
        query: Default::default(),
        headers,
        body: r#"{"ref":"main"}"#.into(),
        secret: Some(old_secret.clone()),
    };
    assert_eq!(
        deliver(&fixture.service, &old_secret, request).await,
        WebhookOutcome::Accepted
    );
    let runs = wait_runs(&fixture.service, &task.task_id, 1).await;
    assert_eq!(runs[0].outcome, ScheduleRunOutcome::Started);

    let (rotated, rotated_path) = fixture
        .service
        .rotate_webhook(&task.task_id)
        .await
        .expect("rotate");
    assert_ne!(rotated_path, Some(old_path));
    assert_eq!(rotated.webhook_path, rotated_path);
    let request = WebhookRequest {
        method: "POST".into(),
        path: "".into(),
        query: Default::default(),
        headers: Default::default(),
        body: String::new(),
        secret: Some(old_secret.clone()),
    };
    assert_eq!(
        deliver(&fixture.service, &old_secret, request).await,
        WebhookOutcome::NotFound
    );
}

#[tokio::test]
async fn a_disabled_task_does_not_run_and_its_thread_stays_kept_while_enabled() {
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture(FakeHarness::new(), clock).await;
    let task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            every_minute(),
            ScheduleDestination::ContinueThread,
        ))
        .await
        .expect("create");
    fixture.clock.advance(chrono::Duration::seconds(61));
    fixture.service.run_due().await;
    let runs = wait_runs(&fixture.service, &task.task_id, 1).await;
    let thread_id = runs[0].thread_id.clone().expect("thread");
    assert!(fixture.service.kept_threads().await.contains(&thread_id));

    fixture
        .service
        .set_enabled(&task.task_id, false)
        .await
        .expect("disable");
    assert!(fixture.service.kept_threads().await.is_empty());
    let paused = fixture.service.task(&task.task_id).await.expect("task");
    assert!(
        paused.next_run_at.is_none(),
        "a paused task has no next run"
    );
    fixture.clock.advance(chrono::Duration::seconds(61));
    fixture.service.run_due().await;
    assert_eq!(
        fixture
            .service
            .runs(&task.task_id)
            .await
            .expect("runs")
            .len(),
        1
    );
}

#[tokio::test]
async fn saving_rejects_bad_fields_before_they_reach_the_runner() {
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture(FakeHarness::new(), clock).await;

    let mut bad = draft(
        &fixture.workspace_path,
        ScheduleSpec::Once {
            date: "2020-01-01".into(),
            time: "09:00".into(),
        },
        ScheduleDestination::NewThread,
    );
    bad.prompt = "Do it".into();
    let error = fixture.service.create(bad).await.expect_err("past once");
    assert!(error.to_string().contains("past"));

    let mut bad = draft(
        &fixture.workspace_path,
        every_minute(),
        ScheduleDestination::NewThread,
    );
    bad.timezone = "Mars/Olympus_Mons".into();
    let error = fixture.service.create(bad).await.expect_err("zone");
    assert!(error.to_string().contains("time zone"));

    let mut bad = draft(
        &fixture.workspace_path,
        every_minute(),
        ScheduleDestination::NewThread,
    );
    bad.host = HostId::new("host_other");
    assert!(fixture.service.create(bad).await.is_err());

    let mut bad = draft(
        &fixture.workspace_path,
        every_minute(),
        ScheduleDestination::NewThread,
    );
    bad.prompt = "   ".into();
    assert!(fixture.service.create(bad).await.is_err());
}

#[tokio::test]
async fn history_is_trimmed_to_the_bound() {
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture(FakeHarness::new(), clock).await;
    let task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            ScheduleSpec::Cron {
                expression: "0 9 * * *".into(),
            },
            ScheduleDestination::NewThread,
        ))
        .await
        .expect("create");
    for _ in 0..(HISTORY_LIMIT + 5) {
        let mut run = ScheduleRun {
            run_id: 0,
            task_id: task.task_id.clone(),
            scheduled_for: None,
            started_at: Utc::now(),
            thread_id: None,
            outcome: ScheduleRunOutcome::Started,
            detail: None,
        };
        fixture
            .journal
            .append_schedule_run(&mut run, HISTORY_LIMIT)
            .await
            .expect("append");
    }
    let runs = fixture.service.runs(&task.task_id).await.expect("runs");
    assert_eq!(runs.len(), HISTORY_LIMIT);
}

#[tokio::test]
async fn deleting_a_task_while_its_run_is_working_does_not_resurrect_it() {
    let (harness, gate) = FakeHarness::new().with_completion_gate();
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture(harness, clock).await;
    let task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            every_minute(),
            ScheduleDestination::NewThread,
        ))
        .await
        .expect("create");
    fixture.clock.advance(chrono::Duration::seconds(61));
    fixture.service.run_due().await;
    let runs = wait_runs(&fixture.service, &task.task_id, 1).await;
    assert_eq!(runs[0].outcome, ScheduleRunOutcome::Started);

    fixture
        .service
        .delete(&task.task_id)
        .await
        .expect("delete while the run works");
    gate.release();
    assert!(fixture.service.task(&task.task_id).await.is_none());
    assert!(fixture.service.list().await.is_empty());
}

#[tokio::test]
async fn a_spent_once_task_still_saves_an_edit() {
    let clock = Arc::new(ManualClock::at(
        "2026-11-01T00:00:00Z"
            .parse::<DateTime<Utc>>()
            .expect("clock"),
    ));
    let fixture = fixture(FakeHarness::new(), clock).await;
    let task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            ScheduleSpec::Once {
                date: "2026-12-01".into(),
                time: "09:00".into(),
            },
            ScheduleDestination::NewThread,
        ))
        .await
        .expect("create");
    fixture.clock.advance(chrono::Duration::days(32));
    fixture.service.run_due().await;
    let runs = wait_runs(&fixture.service, &task.task_id, 1).await;
    assert_eq!(runs[0].outcome, ScheduleRunOutcome::Started);
    let spent = fixture.service.task(&task.task_id).await.expect("task");
    assert!(spent.next_run_at.is_none());

    let mut edit = draft(
        &fixture.workspace_path,
        ScheduleSpec::Once {
            date: "2026-12-01".into(),
            time: "09:00".into(),
        },
        ScheduleDestination::NewThread,
    );
    edit.name = "Renamed once".into();
    let saved = fixture
        .service
        .update(&task.task_id, edit)
        .await
        .expect("rename a spent once task");
    assert_eq!(saved.name, "Renamed once");
    assert!(saved.next_run_at.is_none());
}

#[tokio::test]
async fn a_rotated_path_and_an_edit_survive_a_run_that_was_already_starting() {
    let (harness, gate) = FakeHarness::new().with_start_gate();
    let (harness, sessions) = harness.with_session_count();
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture(harness, clock).await;
    let task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            ScheduleSpec::Webhook,
            ScheduleDestination::NewThread,
        ))
        .await
        .expect("create");
    let old_path = task.webhook_path.clone().expect("path");
    let secret = old_path
        .strip_prefix(WEBHOOK_PREFIX)
        .expect("prefix")
        .to_owned();
    let request = WebhookRequest {
        method: "POST".into(),
        path: old_path,
        query: Default::default(),
        headers: Default::default(),
        body: "{}".into(),
        secret: Some(secret.clone()),
    };

    // The delivery is accepted, then blocks inside the harness start, which is
    // exactly the window a rotation or an edit can race.
    assert_eq!(
        deliver(&fixture.service, &secret, request).await,
        WebhookOutcome::Accepted
    );
    sessions.wait_for(1).await;

    let (_, new_path) = fixture
        .service
        .rotate_webhook(&task.task_id)
        .await
        .expect("rotate during the run");
    let mut edit = task.draft();
    edit.name = "Renamed while running".into();
    fixture
        .service
        .update(&task.task_id, edit)
        .await
        .expect("edit during the run");

    gate.release();
    wait_runs(&fixture.service, &task.task_id, 1).await;
    let stored = fixture.service.task(&task.task_id).await.expect("task");
    assert_eq!(
        stored.webhook_path, new_path,
        "the run's snapshot must not restore the rotated path"
    );
    assert_eq!(
        stored.name, "Renamed while running",
        "the run's snapshot must not undo an edit"
    );
}

#[tokio::test]
async fn two_simultaneous_webhook_deliveries_start_one_run() {
    let (harness, gate) = FakeHarness::new().with_start_gate();
    let (harness, sessions) = harness.with_session_count();
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture(harness, clock).await;
    let task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            ScheduleSpec::Webhook,
            ScheduleDestination::NewThread,
        ))
        .await
        .expect("create");
    let path = task.webhook_path.clone().expect("path");
    let secret = path
        .strip_prefix(WEBHOOK_PREFIX)
        .expect("prefix")
        .to_owned();
    let request = || WebhookRequest {
        method: "POST".into(),
        path: path.clone(),
        query: Default::default(),
        headers: Default::default(),
        body: "{}".into(),
        secret: Some(secret.clone()),
    };

    assert_eq!(
        deliver(&fixture.service, &secret, request()).await,
        WebhookOutcome::Accepted
    );
    // No thread or started run exists yet, so only the in-flight reservation
    // can refuse the second delivery.
    let before = fixture.service.task(&task.task_id).await.expect("task");
    assert!(before.last_run.is_none());
    assert!(before.thread_id.is_none());
    assert_eq!(
        deliver(&fixture.service, &secret, request()).await,
        WebhookOutcome::Skipped,
        "the second delivery is skipped while the first is starting"
    );

    gate.release();
    sessions.wait_for(1).await;
    let runs = wait_runs(&fixture.service, &task.task_id, 2).await;
    assert_eq!(
        runs.iter()
            .filter(|run| run.outcome == ScheduleRunOutcome::Started)
            .count(),
        1,
        "only one delivery starts a run"
    );
    let skipped = runs
        .iter()
        .find(|run| run.outcome == ScheduleRunOutcome::Skipped)
        .expect("the skipped delivery is recorded");
    assert!(
        skipped
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("already starting"))
    );
    assert_eq!(
        fixture
            .manager
            .list_threads(&Default::default())
            .await
            .len(),
        1
    );
}

#[tokio::test]
async fn a_slot_picked_before_a_pause_is_not_dispatched() {
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture(FakeHarness::new(), clock).await;
    let task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            every_minute(),
            ScheduleDestination::NewThread,
        ))
        .await
        .expect("create");
    fixture.clock.advance(chrono::Duration::seconds(61));
    let slot = task.next_run_at.expect("due slot");

    fixture
        .service
        .set_enabled(&task.task_id, false)
        .await
        .expect("pause");
    // This is the runner admitting a slot it collected before the pause.
    assert!(matches!(
        fixture
            .service
            .admit(&task.task_id, Trigger::Scheduled, Some(slot))
            .await,
        Err(AdmissionError::Disabled)
    ));
    fixture.service.run_due().await;
    assert!(
        fixture
            .service
            .runs(&task.task_id)
            .await
            .expect("runs")
            .is_empty()
    );
}

#[tokio::test]
async fn a_continuing_run_uses_the_tasks_saved_permissions() {
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture(FakeHarness::new(), clock).await;
    let mut first = draft(
        &fixture.workspace_path,
        every_minute(),
        ScheduleDestination::ContinueThread,
    );
    first.runtime_mode = RuntimeMode::FullAccess;
    let task = fixture.service.create(first).await.expect("create");

    fixture.clock.advance(chrono::Duration::seconds(61));
    fixture.service.run_due().await;
    let runs = wait_runs(&fixture.service, &task.task_id, 1).await;
    let thread_id = runs[0].thread_id.clone().expect("thread");
    assert!(wait_idle(&fixture.manager, &thread_id).await);
    assert_eq!(
        fixture
            .manager
            .thread_meta(&thread_id)
            .await
            .expect("meta")
            .runtime_mode,
        RuntimeMode::FullAccess
    );

    // Changing the task to Approval Required must change the next run, not
    // just the task row.
    let mut edit = fixture
        .service
        .task(&task.task_id)
        .await
        .expect("task")
        .draft();
    edit.runtime_mode = RuntimeMode::ApprovalRequired;
    fixture
        .service
        .update(&task.task_id, edit)
        .await
        .expect("update");
    fixture.clock.advance(chrono::Duration::seconds(61));
    fixture.service.run_due().await;
    wait_runs(&fixture.service, &task.task_id, 2).await;
    assert!(wait_idle(&fixture.manager, &thread_id).await);
    assert_eq!(
        fixture
            .manager
            .thread_meta(&thread_id)
            .await
            .expect("meta")
            .runtime_mode,
        RuntimeMode::ApprovalRequired,
        "the run must use the settings the task was saved with"
    );
}

#[tokio::test]
async fn changing_the_harness_starts_a_new_destination_thread() {
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture_with_harnesses(
        vec![
            FakeHarness::new(),
            FakeHarness::new().with_kind(HarnessKind::Codex),
        ],
        clock,
        None,
    )
    .await;
    let task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            every_minute(),
            ScheduleDestination::ContinueThread,
        ))
        .await
        .expect("create");
    fixture.clock.advance(chrono::Duration::seconds(61));
    fixture.service.run_due().await;
    let runs = wait_runs(&fixture.service, &task.task_id, 1).await;
    let first_thread = runs[0].thread_id.clone().expect("thread");
    assert!(wait_idle(&fixture.manager, &first_thread).await);

    let mut edit = fixture
        .service
        .task(&task.task_id)
        .await
        .expect("task")
        .draft();
    edit.harness = HarnessKind::Codex;
    edit.model = "fake-model".into();
    fixture
        .service
        .update(&task.task_id, edit)
        .await
        .expect("change the harness");
    assert!(
        fixture
            .service
            .task(&task.task_id)
            .await
            .expect("task")
            .thread_id
            .is_none(),
        "a harness change drops the old destination thread"
    );

    fixture.clock.advance(chrono::Duration::seconds(61));
    fixture.service.run_due().await;
    let runs = wait_runs(&fixture.service, &task.task_id, 2).await;
    let second_thread = runs[0].thread_id.clone().expect("thread");
    assert_ne!(first_thread, second_thread);
}

#[tokio::test]
async fn a_completed_once_task_is_never_run_again_in_the_repeated_hour() {
    let clock = Arc::new(ManualClock::at(
        "2026-10-24T12:00:00Z"
            .parse::<DateTime<Utc>>()
            .expect("clock"),
    ));
    let fixture = fixture(FakeHarness::new(), clock).await;
    let task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            ScheduleSpec::Once {
                date: "2026-10-25".into(),
                time: "02:30".into(),
            },
            ScheduleDestination::NewThread,
        ))
        .await
        .expect("create");
    assert_eq!(
        task.next_run_at,
        Some(
            "2026-10-25T00:30:00Z"
                .parse::<DateTime<Utc>>()
                .expect("slot")
        )
    );

    fixture
        .clock
        .advance(chrono::Duration::minutes(12 * 60 + 31));
    fixture.service.run_due().await;
    let runs = wait_runs(&fixture.service, &task.task_id, 1).await;
    assert_eq!(runs[0].outcome, ScheduleRunOutcome::Started);
    let spent = fixture.service.task(&task.task_id).await.expect("task");
    assert!(spent.once_consumed);
    assert_eq!(
        spent.once_consumed_slot.as_deref(),
        Some("2026-10-25T02:30@Europe/Berlin")
    );
    assert!(spent.next_run_at.is_none());

    // The second 02:30 (CET) is 01:30 UTC. It must not match again.
    fixture.clock.advance(chrono::Duration::hours(1));
    fixture.service.run_due().await;
    assert_eq!(
        fixture
            .service
            .runs(&task.task_id)
            .await
            .expect("runs")
            .len(),
        1
    );

    // An edit keeps the slot spent, and re-enabling does not re-arm it.
    let mut edit = spent.draft();
    edit.name = "Renamed once".into();
    fixture
        .service
        .update(&task.task_id, edit)
        .await
        .expect("edit");
    assert!(
        fixture
            .service
            .task(&task.task_id)
            .await
            .expect("task")
            .once_consumed
    );
    fixture
        .service
        .set_enabled(&task.task_id, false)
        .await
        .expect("pause");
    assert!(
        fixture
            .service
            .set_enabled(&task.task_id, true)
            .await
            .is_err(),
        "a spent once task cannot be re-armed"
    );
    fixture.service.run_due().await;
    assert_eq!(
        fixture
            .service
            .runs(&task.task_id)
            .await
            .expect("runs")
            .len(),
        1
    );
}

#[tokio::test]
async fn a_failed_recording_retries_only_that_write_and_keeps_the_real_slot() {
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture(FakeHarness::new(), clock).await;
    let task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            every_minute(),
            ScheduleDestination::NewThread,
        ))
        .await
        .expect("create");
    let first_slot = task.next_run_at.expect("first slot");
    let real_next = first_slot + chrono::Duration::minutes(1);
    fixture.clock.advance(chrono::Duration::seconds(61));
    fixture.service.fail_next_commit();
    fixture.service.run_due().await;

    let after_failure = fixture.service.task(&task.task_id).await.expect("task");
    assert!(
        after_failure.next_run_at == Some(real_next),
        "the real next slot is kept, not replaced by a retry backoff"
    );
    assert_eq!(
        fixture
            .manager
            .list_threads(&Default::default())
            .await
            .len(),
        1,
        "the run started exactly one thread"
    );
    assert!(
        fixture
            .service
            .runs(&task.task_id)
            .await
            .expect("runs")
            .is_empty(),
        "the run is not in the history yet"
    );

    // One second later the retry is not due, so nothing is re-admitted.
    fixture.clock.advance(chrono::Duration::seconds(1));
    fixture.service.run_due().await;
    assert_eq!(
        fixture
            .manager
            .list_threads(&Default::default())
            .await
            .len(),
        1,
        "the slot must not start a second thread while its write is pending"
    );

    // Once the retry deadline passes, only the pending write is repeated.
    fixture.clock.advance(chrono::Duration::seconds(31));
    fixture.service.run_due().await;
    let runs = wait_runs(&fixture.service, &task.task_id, 1).await;
    assert_eq!(runs[0].outcome, ScheduleRunOutcome::Started);
    assert_eq!(runs[0].scheduled_for, Some(first_slot));
    assert_eq!(
        fixture
            .manager
            .list_threads(&Default::default())
            .await
            .len(),
        1,
        "the retry starts no second thread"
    );
    let stored = fixture.service.task(&task.task_id).await.expect("task");
    assert_eq!(
        stored.next_run_at,
        Some(real_next),
        "the retried write keeps the real next slot"
    );
    // The live last run is the one the journal assigned an id to.
    assert_eq!(
        stored.last_run.as_ref().map(|run| run.run_id),
        Some(runs[0].run_id)
    );
}

#[tokio::test]
async fn a_pending_write_is_never_a_due_slot_for_the_runner() {
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture(FakeHarness::new(), clock).await;
    let task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            every_minute(),
            ScheduleDestination::NewThread,
        ))
        .await
        .expect("create");

    // The slot fails to record, then its retry fails too.
    fixture.clock.advance(chrono::Duration::seconds(61));
    fixture.service.fail_next_commit();
    fixture.service.run_due().await;
    fixture.service.fail_next_commit();
    fixture.clock.advance(chrono::Duration::seconds(31));
    fixture.service.run_due().await;

    let real_next = fixture
        .service
        .task(&task.task_id)
        .await
        .expect("task")
        .next_run_at
        .expect("the real next slot is kept");
    // Move past that real slot while the write is still pending.
    fixture.clock.advance(chrono::Duration::seconds(30));
    let now = fixture.clock.now();
    assert!(
        real_next <= now,
        "the test needs the real slot to be due behind the pending write"
    );
    let pending_due = lock(&fixture.service.inner.pending)
        .get(&task.task_id)
        .expect("pending write")
        .due_at;
    assert!(pending_due > now, "the retry deadline is still ahead");

    let next = fixture
        .service
        .next_due()
        .await
        .expect("a pending deadline");
    assert!(
        next > now,
        "a slot that admission refuses must not read as due now: {next} <= {now}"
    );
    assert_eq!(next, pending_due);

    // The runner admits nothing for the pending slot, and still wakes on the
    // retry deadline rather than spinning on the past slot.
    fixture.service.run_due().await;
    let after = fixture
        .service
        .next_due()
        .await
        .expect("the pending deadline");
    assert!(
        after > fixture.clock.now(),
        "run_due must not leave a past instant to busy-loop on"
    );
    assert_eq!(
        fixture
            .manager
            .list_threads(&Default::default())
            .await
            .len(),
        1,
        "the pending slot starts no second thread"
    );
}

#[tokio::test]
async fn a_failed_webhook_or_manual_recording_never_writes_a_clock_slot() {
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture(FakeHarness::new(), clock).await;
    let webhook = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            ScheduleSpec::Webhook,
            ScheduleDestination::NewThread,
        ))
        .await
        .expect("webhook task");
    let path = webhook.webhook_path.clone().expect("path");
    let secret = path
        .strip_prefix(WEBHOOK_PREFIX)
        .expect("prefix")
        .to_owned();
    let request = WebhookRequest {
        method: "POST".into(),
        path,
        query: Default::default(),
        headers: Default::default(),
        body: "{}".into(),
        secret: Some(secret.clone()),
    };
    fixture.service.fail_next_commit();
    assert_eq!(
        deliver(&fixture.service, &secret, request).await,
        WebhookOutcome::Accepted
    );
    // The delivery answers before its run is recorded; wait for the failure to
    // become a pending write.
    wait_pending(&fixture.service, &webhook.task_id).await;
    let after = fixture.service.task(&webhook.task_id).await.expect("task");
    assert!(
        after.next_run_at.is_none(),
        "a failed webhook recording must not arm a clock slot"
    );
    fixture.clock.advance(chrono::Duration::seconds(31));
    fixture.service.run_due().await;
    let runs = wait_runs(&fixture.service, &webhook.task_id, 1).await;
    assert_eq!(runs[0].outcome, ScheduleRunOutcome::Started);
    assert!(
        fixture
            .service
            .task(&webhook.task_id)
            .await
            .expect("task")
            .next_run_at
            .is_none()
    );

    let clock_task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            every_minute(),
            ScheduleDestination::NewThread,
        ))
        .await
        .expect("clock task");
    let slot = clock_task.next_run_at.expect("slot");
    fixture.service.fail_next_commit();
    let error = fixture
        .service
        .run_now(&clock_task.task_id)
        .await
        .expect_err("the run could not be recorded");
    assert!(error.to_string().contains("could not be saved"));
    let after = fixture
        .service
        .task(&clock_task.task_id)
        .await
        .expect("task");
    assert_eq!(
        after.next_run_at,
        Some(slot),
        "a failed manual recording keeps the task's clock slot"
    );
    assert!(after.last_run.is_none());
    wait_pending(&fixture.service, &clock_task.task_id).await;
    fixture.clock.advance(chrono::Duration::seconds(31));
    fixture.service.run_due().await;
    wait_runs(&fixture.service, &clock_task.task_id, 1).await;
    assert_eq!(
        fixture
            .service
            .task(&clock_task.task_id)
            .await
            .expect("task")
            .next_run_at,
        Some(slot)
    );
}

/// Waits until a run's journal write is waiting for its retry.
async fn wait_pending(service: &ScheduleService, task: &ScheduleTaskId) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !lock(&service.inner.pending).contains_key(task) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the run should be pending a retry");
}

#[tokio::test]
async fn a_bad_stored_zone_disarms_the_task_instead_of_starting_more_threads() {
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture(FakeHarness::new(), clock).await;
    let task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            every_minute(),
            ScheduleDestination::NewThread,
        ))
        .await
        .expect("create");
    // Model a row written by an older or corrupted daemon.
    fixture
        .service
        .inner
        .tasks
        .lock()
        .await
        .get_mut(&task.task_id)
        .expect("task")
        .timezone = "Mars/Olympus_Mons".into();

    fixture.clock.advance(chrono::Duration::seconds(61));
    fixture.service.run_due().await;
    let after = fixture.service.task(&task.task_id).await.expect("task");
    assert!(
        after.next_run_at.is_none(),
        "a schedule with no computable next run is disarmed"
    );
    let last = after.last_run.expect("the refusal is recorded");
    assert_eq!(last.outcome, ScheduleRunOutcome::Failed);
    assert!(
        last.detail
            .as_deref()
            .is_some_and(|detail| detail.contains("time zone")),
        "the reason is visible: {:?}",
        last.detail
    );
    assert_eq!(
        fixture
            .manager
            .list_threads(&Default::default())
            .await
            .len(),
        0,
        "no thread is started for a run that could never be recorded"
    );

    // Several retry windows later the disarmed task still runs nothing: a
    // backoff used to leave the slot armed, so every backoff started a thread.
    for _ in 0..4 {
        fixture.clock.advance(chrono::Duration::minutes(5));
        fixture.service.run_due().await;
    }
    assert_eq!(
        fixture
            .manager
            .list_threads(&Default::default())
            .await
            .len(),
        0
    );
    assert_eq!(
        fixture
            .service
            .runs(&task.task_id)
            .await
            .expect("runs")
            .len(),
        1,
        "one visible refusal, not one per backoff"
    );
}

#[tokio::test]
async fn a_disarmed_task_is_paused_and_stops_keeping_its_thread() {
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture(FakeHarness::new(), clock).await;
    let task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            every_minute(),
            ScheduleDestination::ContinueThread,
        ))
        .await
        .expect("create");
    fixture.clock.advance(chrono::Duration::seconds(61));
    fixture.service.run_due().await;
    let runs = wait_runs(&fixture.service, &task.task_id, 1).await;
    let thread_id = runs[0].thread_id.clone().expect("thread");
    assert!(
        fixture.service.kept_threads().await.contains(&thread_id),
        "an enabled task keeps its thread's checkout"
    );

    // Model a row written by an older or corrupted daemon: the schedule can no
    // longer name a slot.
    fixture
        .service
        .inner
        .tasks
        .lock()
        .await
        .get_mut(&task.task_id)
        .expect("task")
        .timezone = "Mars/Olympus_Mons".into();
    fixture.clock.advance(chrono::Duration::seconds(61));
    fixture.service.run_due().await;

    let disarmed = fixture.service.task(&task.task_id).await.expect("task");
    assert!(
        !disarmed.enabled,
        "a task that will never run again is paused, so the toggle offers Resume"
    );
    assert!(disarmed.next_run_at.is_none());
    assert_eq!(
        disarmed.last_run.expect("the refusal is recorded").outcome,
        ScheduleRunOutcome::Failed
    );
    assert!(
        fixture.service.kept_threads().await.is_empty(),
        "a disarmed task no longer protects its checkout"
    );

    // The pause is stored with the failure: a restart must not offer to pause a
    // task that already runs nothing.
    let restarted = ScheduleService::open(
        Arc::clone(&fixture.journal),
        Arc::clone(&fixture.manager),
        HostId::new("host_schedule_test"),
        fixture.clock.clone(),
    )
    .await
    .expect("restart");
    let reopened = restarted.task(&task.task_id).await.expect("task");
    assert!(!reopened.enabled, "the disarm is durable");
    assert!(reopened.next_run_at.is_none());
    let failure = reopened
        .last_run
        .expect("the failure that explains the pause survives the restart");
    assert_eq!(failure.outcome, ScheduleRunOutcome::Failed);
    assert!(
        failure
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("no next run")),
        "the reopened task still says why it cannot run: {:?}",
        failure.detail
    );
    fixture.clock.advance(chrono::Duration::days(1));
    restarted.run_due().await;
    assert_eq!(
        fixture
            .manager
            .list_threads(&Default::default())
            .await
            .len(),
        1,
        "a disarmed task starts nothing after a restart"
    );
}

/// A task whose stored zone cannot name a slot, due now.
async fn a_task_that_cannot_name_its_next_slot(fixture: &Fixture) -> ScheduleTask {
    let task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            every_minute(),
            ScheduleDestination::NewThread,
        ))
        .await
        .expect("create");
    // Model a row written by an older or corrupted daemon.
    fixture
        .service
        .inner
        .tasks
        .lock()
        .await
        .get_mut(&task.task_id)
        .expect("task")
        .timezone = "Mars/Olympus_Mons".into();
    fixture.clock.advance(chrono::Duration::seconds(61));
    fixture.service.task(&task.task_id).await.expect("task")
}

#[tokio::test]
async fn a_disarm_whose_write_fails_lands_its_pause_with_the_failure() {
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture(FakeHarness::new(), clock).await;
    let task = a_task_that_cannot_name_its_next_slot(&fixture).await;

    fixture.service.fail_next_commit();
    fixture.service.run_due().await;
    wait_pending(&fixture.service, &task.task_id).await;
    let waiting = fixture.service.task(&task.task_id).await.expect("task");
    assert!(
        waiting.enabled,
        "the pause is not shown before the failure that explains it is saved"
    );
    assert_eq!(
        waiting.next_run_at, task.next_run_at,
        "the task keeps its slot while its write waits"
    );
    assert!(waiting.last_run.is_none());

    // The retry writes the pause and the Failed run together.
    fixture.clock.advance(chrono::Duration::seconds(31));
    fixture.service.run_due().await;
    let disarmed = fixture.service.task(&task.task_id).await.expect("task");
    assert!(!disarmed.enabled, "the retried write pauses the task");
    assert!(disarmed.next_run_at.is_none());
    let failure = disarmed
        .last_run
        .expect("the pause arrives with its failure");
    assert_eq!(failure.outcome, ScheduleRunOutcome::Failed);
    assert!(
        failure
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("no next run")),
        "the reason is visible: {:?}",
        failure.detail
    );
    assert_eq!(
        fixture
            .service
            .runs(&task.task_id)
            .await
            .expect("runs")
            .len(),
        1,
        "one refusal, not one per retry"
    );
    fixture.clock.advance(chrono::Duration::days(1));
    fixture.service.run_due().await;
    assert_eq!(
        fixture
            .service
            .runs(&task.task_id)
            .await
            .expect("runs")
            .len(),
        1,
        "a paused task starts nothing more"
    );
}

#[tokio::test]
async fn a_pause_that_cannot_be_saved_is_never_stored_without_its_failure() {
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture(FakeHarness::new(), clock).await;
    let task = a_task_that_cannot_name_its_next_slot(&fixture).await;

    fixture.service.fail_next_commit();
    fixture.service.run_due().await;
    wait_pending(&fixture.service, &task.task_id).await;

    // The person corrects the zone while the failed write waits, and the
    // editor saves the state it was given.
    let waiting = fixture.service.task(&task.task_id).await.expect("task");
    let mut edit = waiting.draft();
    edit.timezone = "Europe/Berlin".into();
    fixture
        .service
        .update(&task.task_id, edit)
        .await
        .expect("edit");

    // A restart before the retry loses the pending failure. What was saved is
    // the edit, so it stands, and no pause is left behind without its reason.
    let reopened = ScheduleService::open(
        Arc::clone(&fixture.journal),
        Arc::clone(&fixture.manager),
        HostId::new("host_schedule_test"),
        fixture.clock.clone(),
    )
    .await
    .expect("reopen");
    let live = reopened.task(&task.task_id).await.expect("task");
    match (live.enabled, live.last_run.as_ref()) {
        // The person's edit stands: they fixed the schedule, and nothing paused
        // the task behind their back.
        (true, None) => assert!(
            live.next_run_at.is_some(),
            "the corrected schedule has its next run"
        ),
        // Or the pause is stored together with the failure that explains it.
        (false, Some(failure)) => {
            assert_eq!(failure.outcome, ScheduleRunOutcome::Failed);
            assert!(
                failure
                    .detail
                    .as_deref()
                    .is_some_and(|detail| detail.contains("no next run")),
                "a pause never outlives its reason: {:?}",
                failure.detail
            );
            assert!(live.next_run_at.is_none());
        }
        other => panic!("a pause must never be stored without the failure behind it: {other:?}"),
    }
}

#[tokio::test]
async fn a_pending_disarm_keeps_the_edit_the_person_saved() {
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture(FakeHarness::new(), clock).await;
    let task = a_task_that_cannot_name_its_next_slot(&fixture).await;

    fixture.service.fail_next_commit();
    fixture.service.run_due().await;
    wait_pending(&fixture.service, &task.task_id).await;

    let waiting = fixture.service.task(&task.task_id).await.expect("task");
    let mut edit = waiting.draft();
    edit.timezone = "Europe/Berlin".into();
    let edited = fixture
        .service
        .update(&task.task_id, edit)
        .await
        .expect("edit");
    assert!(edited.enabled);
    assert!(edited.next_run_at.is_some());

    // The retry records the failure but does not pause a task the person has
    // already moved to another slot.
    fixture.clock.advance(chrono::Duration::seconds(31));
    fixture.service.run_due().await;
    let live = fixture.service.task(&task.task_id).await.expect("task");
    assert!(
        live.enabled,
        "an edit that fixed the schedule wins over the staged pause"
    );
    assert!(live.next_run_at.is_some());
    assert_eq!(
        live.last_run
            .expect("the failure is still recorded")
            .outcome,
        ScheduleRunOutcome::Failed
    );
}

#[tokio::test]
async fn a_staged_pause_rechecks_the_settings_the_person_fixed() {
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture(FakeHarness::new(), clock).await;
    let task = a_task_that_cannot_name_its_next_slot(&fixture).await;
    let slot = task
        .next_run_at
        .expect("the broken task is still armed for a slot");

    fixture.service.fail_next_commit();
    fixture.service.run_due().await;
    wait_pending(&fixture.service, &task.task_id).await;

    // The person fixes the zone while the failed write waits, and the clock is
    // just before the slot the failed run was for: the corrected settings name
    // that very same slot again.
    fixture.clock.advance(chrono::Duration::seconds(-2));
    let waiting = fixture.service.task(&task.task_id).await.expect("task");
    let mut edit = waiting.draft();
    edit.timezone = "Europe/Berlin".into();
    let edited = fixture
        .service
        .update(&task.task_id, edit)
        .await
        .expect("edit");
    assert!(edited.enabled);
    assert_eq!(
        edited.next_run_at,
        Some(slot),
        "the corrected settings name the slot the failed run was for"
    );

    // The retry still records the original failure, but the stale pause loses
    // to the fixed settings and the slot runs once.
    fixture.clock.advance(chrono::Duration::seconds(32));
    fixture.service.run_due().await;
    let live = fixture.service.task(&task.task_id).await.expect("task");
    assert!(
        live.enabled,
        "a fixed task is not paused by the retry of its old failure"
    );
    let runs = fixture.service.runs(&task.task_id).await.expect("runs");
    assert_eq!(
        runs.last().map(|run| run.outcome),
        Some(ScheduleRunOutcome::Failed),
        "the original failure is still recorded: {runs:?}"
    );
    let started: Vec<&ScheduleRun> = runs
        .iter()
        .filter(|run| run.outcome == ScheduleRunOutcome::Started)
        .collect();
    assert_eq!(
        started.len(),
        1,
        "the corrected task runs that slot once: {runs:?}"
    );
    assert_eq!(started[0].scheduled_for, Some(slot));
    assert!(
        live.next_run_at.is_some_and(|at| at > slot),
        "the task moves on to a later slot"
    );
}

#[tokio::test]
async fn the_first_disarm_attempt_rechecks_the_settings_the_person_fixed() {
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture(FakeHarness::new(), clock).await;
    let task = a_task_that_cannot_name_its_next_slot(&fixture).await;
    let slot = task
        .next_run_at
        .expect("the broken task is still armed for a slot");

    // The runner admits the slot and finds the stored zone cannot name the slot
    // after it, but its bookkeeping has not run yet.
    let reason = match fixture
        .service
        .admit(&task.task_id, Trigger::Scheduled, Some(slot))
        .await
    {
        Err(AdmissionError::Uncomputable(reason)) => reason,
        Err(other) => panic!("a zone the schedule cannot read must be uncomputable: {other:?}"),
        Ok(_) => panic!("a zone the schedule cannot read must be uncomputable, not admitted"),
    };

    // The person fixes the zone just before the slot the refusal was for, so the
    // corrected settings name that very same slot again.
    fixture.clock.advance(chrono::Duration::seconds(-2));
    let waiting = fixture.service.task(&task.task_id).await.expect("task");
    let mut edit = waiting.draft();
    edit.timezone = "Europe/Berlin".into();
    let edited = fixture
        .service
        .update(&task.task_id, edit)
        .await
        .expect("edit");
    assert!(edited.enabled);
    assert_eq!(
        edited.next_run_at,
        Some(slot),
        "the corrected settings name the slot the refusal was for"
    );

    // The bookkeeping of the first attempt runs now. It must make its decision
    // from the settings the person saved, not the ones the refusal came from.
    fixture
        .service
        .record_uncomputable(&task.task_id, slot, reason)
        .await;
    let live = fixture.service.task(&task.task_id).await.expect("task");
    assert!(
        live.enabled,
        "the first disarm attempt must not pause a task the person just fixed"
    );
    assert_eq!(
        live.next_run_at,
        Some(slot),
        "the corrected task keeps the slot the refusal was for"
    );
    assert_eq!(
        live.last_run.as_ref().map(|run| run.outcome),
        Some(ScheduleRunOutcome::Failed),
        "the failure is still recorded"
    );

    // The corrected task runs that slot once.
    fixture.clock.advance(chrono::Duration::seconds(32));
    fixture.service.run_due().await;
    let runs = wait_runs(&fixture.service, &task.task_id, 2).await;
    let started: Vec<&ScheduleRun> = runs
        .iter()
        .filter(|run| run.outcome == ScheduleRunOutcome::Started)
        .collect();
    assert_eq!(
        started.len(),
        1,
        "the corrected task runs that slot exactly once: {runs:?}"
    );
    assert_eq!(started[0].scheduled_for, Some(slot));
    assert_eq!(
        runs.last().map(|run| run.outcome),
        Some(ScheduleRunOutcome::Failed),
        "the original failure stays in the history: {runs:?}"
    );
    let after = fixture.service.task(&task.task_id).await.expect("task");
    assert!(after.enabled);
    assert!(
        after.next_run_at.is_some_and(|at| at > slot),
        "the task moves on to a later slot"
    );
}

#[tokio::test]
async fn a_row_disarmed_before_the_pause_existed_opens_paused() {
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture(FakeHarness::new(), clock).await;
    let task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            every_minute(),
            ScheduleDestination::ContinueThread,
        ))
        .await
        .expect("create");
    fixture.clock.advance(chrono::Duration::seconds(61));
    fixture.service.run_due().await;
    let runs = wait_runs(&fixture.service, &task.task_id, 1).await;
    let thread_id = runs[0].thread_id.clone().expect("thread");
    let live = fixture.service.task(&task.task_id).await.expect("task");

    // A row disarmed by the daemon before the pause existed: the schedule could
    // not name a slot, so `next_run_at` was cleared, but `enabled` stayed on.
    let mut legacy = live.clone();
    legacy.enabled = true;
    legacy.next_run_at = None;
    fixture
        .journal
        .upsert_schedule(&legacy)
        .await
        .expect("write the legacy row");
    let mut refusal = ScheduleRun {
        run_id: 0,
        task_id: task.task_id.clone(),
        scheduled_for: live.next_run_at,
        started_at: fixture.clock.now(),
        thread_id: None,
        outcome: ScheduleRunOutcome::Failed,
        detail: Some(
            "Its schedule has no next run: the task has an invalid time zone. Fix the schedule to run it again."
                .into(),
        ),
    };
    fixture
        .journal
        .append_schedule_run(&mut refusal, HISTORY_LIMIT)
        .await
        .expect("write the refusal");

    // A webhook task has no clock slot by design, and a spent Once task already
    // shows as spent: neither is a disarm.
    let webhook = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            ScheduleSpec::Webhook,
            ScheduleDestination::NewThread,
        ))
        .await
        .expect("webhook task");
    let once = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            ScheduleSpec::Once {
                date: "2030-01-01".into(),
                time: "09:00".into(),
            },
            ScheduleDestination::NewThread,
        ))
        .await
        .expect("once task");
    let mut spent = fixture.service.task(&once.task_id).await.expect("once");
    spent.next_run_at = None;
    spent.once_consumed = true;
    spent.once_consumed_slot = spent.schedule.once_slot(&spent.timezone);
    fixture
        .journal
        .upsert_schedule(&spent)
        .await
        .expect("write the spent once row");

    let reopened = ScheduleService::open(
        Arc::clone(&fixture.journal),
        Arc::clone(&fixture.manager),
        HostId::new("host_schedule_test"),
        fixture.clock.clone(),
    )
    .await
    .expect("reopen");
    let opened = reopened.task(&task.task_id).await.expect("task");
    assert!(
        !opened.enabled,
        "a task disarmed on upgrade opens paused, so the toggle offers Resume"
    );
    assert!(opened.next_run_at.is_none());
    assert!(
        !reopened.kept_threads().await.contains(&thread_id),
        "a disarmed task releases its checkout"
    );
    let stored = fixture
        .journal
        .schedule(&task.task_id)
        .await
        .expect("journal row")
        .expect("task row");
    assert!(
        !stored.enabled,
        "the normalized pause is saved, not recomputed on every open"
    );
    assert!(
        reopened
            .task(&webhook.task_id)
            .await
            .expect("webhook")
            .enabled,
        "a webhook task is enabled without a clock slot"
    );
    let reopened_once = reopened.task(&once.task_id).await.expect("once");
    assert!(reopened_once.enabled, "a spent Once task keeps its state");
    assert!(reopened_once.once_consumed);
    assert!(reopened_once.next_run_at.is_none());
}

#[tokio::test]
async fn a_legacy_disarm_with_a_later_manual_run_opens_paused() {
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture(FakeHarness::new(), clock).await;
    let task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            every_minute(),
            ScheduleDestination::ContinueThread,
        ))
        .await
        .expect("create");
    let live = fixture.service.task(&task.task_id).await.expect("task");

    // A row disarmed by the daemon before the pause existed: the schedule could
    // not name a slot, so `next_run_at` was cleared, but `enabled` stayed on.
    let mut legacy = live.clone();
    legacy.enabled = true;
    legacy.next_run_at = None;
    fixture
        .journal
        .upsert_schedule(&legacy)
        .await
        .expect("write the legacy row");
    let mut refusal = ScheduleRun {
        run_id: 0,
        task_id: task.task_id.clone(),
        scheduled_for: live.next_run_at,
        started_at: fixture.clock.now(),
        thread_id: None,
        outcome: ScheduleRunOutcome::Failed,
        detail: Some(
            "Its schedule has no next run: the task has an invalid time zone. Fix the schedule to run it again."
                .into(),
        ),
    };
    fixture
        .journal
        .append_schedule_run(&mut refusal, HISTORY_LIMIT)
        .await
        .expect("write the refusal");
    // Before upgrading, the person pressed Run now. That manual run is the
    // newest history entry, but it never armed a clock slot.
    let mut manual = ScheduleRun {
        run_id: 0,
        task_id: task.task_id.clone(),
        scheduled_for: None,
        started_at: fixture.clock.now(),
        thread_id: None,
        outcome: ScheduleRunOutcome::Started,
        detail: None,
    };
    fixture
        .journal
        .append_schedule_run(&mut manual, HISTORY_LIMIT)
        .await
        .expect("write the manual run");

    let reopened = ScheduleService::open(
        Arc::clone(&fixture.journal),
        Arc::clone(&fixture.manager),
        HostId::new("host_schedule_test"),
        fixture.clock.clone(),
    )
    .await
    .expect("reopen");
    let opened = reopened.task(&task.task_id).await.expect("task");
    assert!(
        !opened.enabled,
        "a later manual run does not hide the disarm, so the task opens paused"
    );
    assert!(opened.next_run_at.is_none());
    assert_eq!(
        opened.last_run.as_ref().map(|run| run.outcome),
        Some(ScheduleRunOutcome::Started),
        "the manual run is still the newest history entry"
    );
    let stored = fixture
        .journal
        .schedule(&task.task_id)
        .await
        .expect("journal row")
        .expect("task row");
    assert!(!stored.enabled, "the normalized pause is saved");
    assert_eq!(stored.name, legacy.name, "only the pause changes");
    assert_eq!(stored.schedule, legacy.schedule, "only the pause changes");
    assert_eq!(stored.timezone, legacy.timezone, "only the pause changes");
}

#[tokio::test]
async fn an_unreadable_slot_without_a_disarm_failure_is_left_as_stored() {
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let journal = Arc::new(SqliteJournal::in_memory().await.expect("journal"));
    let fixture = fixture_with_journal(
        FakeHarness::new(),
        clock,
        Some(journal.clone() as Arc<dyn Journal>),
    )
    .await;
    let task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            every_minute(),
            ScheduleDestination::NewThread,
        ))
        .await
        .expect("create");

    // A row whose timestamp no longer decodes: it opens with no readable next
    // run, but its last recorded run is not the disarm failure.
    journal
        .set_raw_next_run_at(&task.task_id, Some("not-a-timestamp"))
        .await
        .expect("write the unreadable slot");

    let reopened = ScheduleService::open(
        Arc::clone(&fixture.journal),
        Arc::clone(&fixture.manager),
        HostId::new("host_schedule_test"),
        fixture.clock.clone(),
    )
    .await
    .expect("reopen");
    let opened = reopened.task(&task.task_id).await.expect("task");
    assert!(
        opened.enabled,
        "a row that never failed to disarm keeps its enabled state"
    );
    assert!(
        opened.next_run_at.is_none(),
        "the stored slot cannot be decoded"
    );
    assert_eq!(
        journal
            .raw_next_run_at(&task.task_id)
            .await
            .expect("raw slot")
            .as_deref(),
        Some("not-a-timestamp"),
        "an upgrade never erases a timestamp it could not read"
    );
}

#[tokio::test]
async fn a_legacy_once_marker_is_migrated_and_keeps_the_slot_spent() {
    for legacy_slot in [None, Some("2026-10-25T02:30".to_owned())] {
        let clock = Arc::new(ManualClock::at(
            "2026-10-24T12:00:00Z"
                .parse::<DateTime<Utc>>()
                .expect("clock"),
        ));
        let fixture = fixture(FakeHarness::new(), clock).await;
        let task = fixture
            .service
            .create(draft(
                &fixture.workspace_path,
                ScheduleSpec::Once {
                    date: "2026-10-25".into(),
                    time: "02:30".into(),
                },
                ScheduleDestination::NewThread,
            ))
            .await
            .expect("create");
        let slot = task.next_run_at.expect("the once slot is armed");

        // A row written before the zone joined the slot identity: it is spent,
        // and its marker is either missing (only the old flag) or names just
        // the local date and time. The armed slot is still stored.
        let mut legacy = task.clone();
        legacy.once_consumed = true;
        legacy.once_consumed_slot = legacy_slot.clone();
        legacy.next_run_at = Some(slot);
        fixture
            .journal
            .upsert_schedule(&legacy)
            .await
            .expect("write the legacy row");

        let reopened = ScheduleService::open(
            Arc::clone(&fixture.journal),
            Arc::clone(&fixture.manager),
            HostId::new("host_schedule_test"),
            fixture.clock.clone(),
        )
        .await
        .expect("reopen");
        let migrated = reopened.task(&task.task_id).await.expect("task");
        assert_eq!(
            migrated.once_consumed_slot.as_deref(),
            Some("2026-10-25T02:30@Europe/Berlin"),
            "the marker gains the zone that names its instant"
        );
        assert!(migrated.once_consumed);
        assert!(
            migrated.next_run_at.is_none(),
            "a spent slot is not left armed"
        );

        // The slot never runs, even while the daemon keeps running: cross the
        // original 02:30 slot itself, not just a few minutes after the clock
        // started.
        let cross = slot - fixture.clock.now() + chrono::Duration::minutes(1);
        fixture.clock.advance(cross);
        assert!(
            fixture.clock.now() > slot,
            "the clock is past the slot the legacy row still named"
        );
        reopened.run_due().await;
        assert_eq!(
            reopened.runs(&task.task_id).await.expect("runs").len(),
            0,
            "a spent once slot starts no run"
        );
        assert_eq!(
            fixture
                .manager
                .list_threads(&Default::default())
                .await
                .len(),
            0
        );

        // Renaming the task names the same instant, so it stays spent.
        let mut edit = migrated.draft();
        edit.name = "Renamed once".into();
        let renamed = reopened.update(&task.task_id, edit).await.expect("rename");
        assert!(renamed.once_consumed);
        assert!(renamed.next_run_at.is_none());
        assert_eq!(
            renamed.once_consumed_slot.as_deref(),
            Some("2026-10-25T02:30@Europe/Berlin")
        );
        fixture.clock.advance(chrono::Duration::days(1));
        reopened.run_due().await;
        assert_eq!(
            reopened.runs(&task.task_id).await.expect("runs").len(),
            0,
            "a renamed task keeps its spent slot spent"
        );
    }
}

#[tokio::test]
async fn a_thread_pinned_to_working_but_idle_is_not_busy() {
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let workspace = tempfile::tempdir().expect("workspace");
    let journal: Arc<dyn Journal> = Arc::new(SqliteJournal::in_memory().await.expect("journal"));
    // A thread row with no turn is the one state that reads as `Idle`: a
    // conversation that exists but has never worked.
    let idle_thread = ThreadId::new_uuid_v7();
    journal
        .upsert_thread(&idle_thread_meta(&idle_thread, workspace.path()))
        .await
        .expect("seed an idle thread");
    let fixture = fixture_with_journal(FakeHarness::new(), clock, Some(journal)).await;
    let task = fixture
        .service
        .create(draft(
            workspace.path(),
            every_minute(),
            ScheduleDestination::ContinueThread,
        ))
        .await
        .expect("create");
    fixture
        .service
        .inner
        .tasks
        .lock()
        .await
        .get_mut(&task.task_id)
        .expect("task")
        .thread_id = Some(idle_thread.clone());
    fixture
        .manager
        .set_thread_tab(&idle_thread, ThreadTab::Working, true)
        .await
        .expect("pin");
    let summary = fixture
        .manager
        .list_threads(&Default::default())
        .await
        .into_iter()
        .find(|thread| thread.thread_id == idle_thread)
        .expect("the pinned thread is listed");
    assert_eq!(summary.agent_state, AgentState::Idle, "the thread is idle");
    assert_eq!(summary.tab, ThreadTab::Working);
    assert!(summary.pinned, "the thread is pinned to Working");
    assert!(
        !fixture.manager.thread_is_busy(&idle_thread).await,
        "a pinned but idle thread has nothing to overlap"
    );

    fixture.clock.advance(chrono::Duration::seconds(61));
    fixture.service.run_due().await;
    let runs = wait_runs(&fixture.service, &task.task_id, 1).await;
    assert_eq!(
        runs[0].outcome,
        ScheduleRunOutcome::Started,
        "a pinned but idle thread does not make the run skip"
    );
    assert_eq!(runs[0].thread_id.as_ref(), Some(&idle_thread));
}

/// A thread row with no turn, which the manager restores as an idle thread.
fn idle_thread_meta(
    thread_id: &ThreadId,
    workspace: &std::path::Path,
) -> crate::journal::ThreadMeta {
    crate::journal::ThreadMeta {
        thread_id: thread_id.clone(),
        parent_thread_id: None,
        subagent_id: None,
        host: HostId::new("host_schedule_test"),
        harness: HarnessKind::ClaudeCode,
        agent_id: None,
        title: "Idle thread".into(),
        title_source: crate::journal::TitleSource::Prompt,
        workspace_path: workspace.display().to_string(),
        model: "fake-model".into(),
        runtime_mode: RuntimeMode::ApprovalRequired,
        reasoning_effort: None,
        fast: false,
        worktree: None,
        created_at: Utc::now(),
        usage: Default::default(),
        resume_id: None,
        agent_fingerprint: None,
        parked: None,
        queued: Vec::new(),
        claimed: Vec::new(),
        imported: None,
    }
}

#[tokio::test]
async fn a_recorded_run_survives_a_restart() {
    let dir = tempfile::tempdir().expect("dir");
    let db = dir.path().join("journal.db");
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let first = fixture_with_journal(
        FakeHarness::new(),
        clock.clone(),
        Some(Arc::new(
            SqliteJournal::connect(&db).await.expect("journal"),
        )),
    )
    .await;
    let task = first
        .service
        .create(draft(
            &first.workspace_path,
            every_minute(),
            ScheduleDestination::NewThread,
        ))
        .await
        .expect("create");
    clock.advance(chrono::Duration::seconds(61));
    first.service.run_due().await;
    let runs = wait_runs(&first.service, &task.task_id, 1).await;
    let run_id = runs[0].run_id;
    drop(first);

    let second = fixture_with_journal(
        FakeHarness::new(),
        clock,
        Some(Arc::new(
            SqliteJournal::connect(&db).await.expect("journal"),
        )),
    )
    .await;
    let restored = second
        .service
        .task(&task.task_id)
        .await
        .expect("task survived");
    assert_eq!(
        restored.last_run.as_ref().map(|run| run.run_id),
        Some(run_id)
    );
    let runs = second
        .service
        .runs(&task.task_id)
        .await
        .expect("runs survived");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].run_id, run_id);
}

#[tokio::test]
async fn a_task_body_and_event_never_carry_the_webhook_path() {
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture(FakeHarness::new(), clock).await;
    let task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            ScheduleSpec::Webhook,
            ScheduleDestination::NewThread,
        ))
        .await
        .expect("create");
    let secret = task
        .webhook_path
        .as_deref()
        .expect("path")
        .strip_prefix(WEBHOOK_PREFIX)
        .expect("prefix")
        .to_owned();

    let json = serde_json::to_string(&task).expect("serialise task");
    assert!(!json.contains(&secret), "a task body leaked the path");
    assert!(!json.contains("webhook_path\""));
    assert!(json.contains("\"webhook_path_set\":true"));

    let event = ScheduleEvent::Upserted {
        task: Box::new(task.clone()),
    };
    let json = serde_json::to_string(&event).expect("serialise event");
    assert!(!json.contains(&secret), "a change event leaked the path");

    // The admin request still reveals it.
    assert_eq!(
        fixture
            .service
            .webhook_url(&task.task_id)
            .await
            .expect("url"),
        task.webhook_path
    );
}

#[tokio::test]
async fn a_wrong_webhook_path_is_refused_before_the_body_is_read() {
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture(FakeHarness::new(), clock).await;
    let task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            ScheduleSpec::Webhook,
            ScheduleDestination::NewThread,
        ))
        .await
        .expect("create");
    let reads = Arc::new(AtomicUsize::new(0));
    let body = |reads: Arc<AtomicUsize>| {
        axum::body::Body::from_stream(futures::stream::once(async move {
            reads.fetch_add(1, Ordering::SeqCst);
            Err::<axum::body::Bytes, std::io::Error>(std::io::Error::other(
                "the body must not be read",
            ))
        }))
    };

    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/webhooks/not-the-secret")
        .body(body(Arc::clone(&reads)))
        .expect("request");
    let response = crate::server::serve_webhook_request(
        &fixture.service,
        "not-the-secret".to_owned(),
        request,
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
    assert_eq!(
        reads.load(Ordering::SeqCst),
        0,
        "a wrong path must not cost a body read"
    );

    // The real path reads the body, so the assertion above is not vacuous.
    let secret = task
        .webhook_path
        .as_deref()
        .expect("path")
        .strip_prefix(WEBHOOK_PREFIX)
        .expect("prefix")
        .to_owned();
    let request = axum::http::Request::builder()
        .method("POST")
        .uri(format!("{WEBHOOK_PREFIX}{secret}"))
        .body(body(Arc::clone(&reads)))
        .expect("request");
    let response = crate::server::serve_webhook_request(&fixture.service, secret, request).await;
    assert_eq!(response.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(reads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_disabled_future_once_task_saves_without_a_next_run() {
    let clock = Arc::new(ManualClock::at(
        "2026-11-01T00:00:00Z"
            .parse::<DateTime<Utc>>()
            .expect("clock"),
    ));
    let fixture = fixture(FakeHarness::new(), clock).await;
    let mut draft = draft(
        &fixture.workspace_path,
        ScheduleSpec::Once {
            date: "2026-12-01".into(),
            time: "09:00".into(),
        },
        ScheduleDestination::NewThread,
    );
    draft.enabled = false;
    let task = fixture
        .service
        .create(draft)
        .await
        .expect("save paused once");
    assert!(task.next_run_at.is_none());
    let enabled = fixture
        .service
        .set_enabled(&task.task_id, true)
        .await
        .expect("resume");
    assert!(enabled.next_run_at.is_some());
}

#[tokio::test]
async fn a_harness_change_during_a_run_is_not_undone_by_the_run() {
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture_with_harnesses(
        vec![
            FakeHarness::new(),
            FakeHarness::new().with_kind(HarnessKind::Codex),
        ],
        clock,
        None,
    )
    .await;
    let task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            every_minute(),
            ScheduleDestination::ContinueThread,
        ))
        .await
        .expect("create");
    fixture.clock.advance(chrono::Duration::seconds(61));
    let slot = task.next_run_at.expect("slot");
    // Admit first, then hold the task's write lock so the run's bookkeeping
    // waits while the harness change lands: the window a person can edit in.
    let (snapshot, reservation) = fixture
        .service
        .admit(&task.task_id, Trigger::Scheduled, Some(slot))
        .await
        .expect("admit");
    let write = fixture.service.write_lock(&task.task_id).await;
    let running = {
        let service = fixture.service.clone();
        let prompt = snapshot.prompt.clone();
        tokio::spawn(async move {
            service
                .execute(reservation, snapshot, Trigger::Scheduled, prompt)
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        while fixture
            .manager
            .list_threads(&Default::default())
            .await
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the run should have started its thread");

    let mut edited = fixture.service.task(&task.task_id).await.expect("task");
    edited.harness = HarnessKind::Codex;
    edited.thread_id = None;
    fixture
        .service
        .inner
        .tasks
        .lock()
        .await
        .insert(edited.task_id.clone(), edited.clone());
    fixture
        .journal
        .upsert_schedule(&edited)
        .await
        .expect("save the edit");
    drop(write);
    let run = running
        .await
        .expect("the run task ends")
        .expect("the run is recorded");
    assert_eq!(run.outcome, ScheduleRunOutcome::Started);
    let first_thread = run.thread_id.clone().expect("first thread");
    assert!(wait_idle(&fixture.manager, &first_thread).await);

    let stored = fixture.service.task(&task.task_id).await.expect("task");
    assert!(
        stored.thread_id.is_none(),
        "the run must not reattach the thread it started on the old harness"
    );
    fixture.clock.advance(chrono::Duration::seconds(61));
    fixture.service.run_due().await;
    let runs = wait_runs(&fixture.service, &task.task_id, 2).await;
    let second_thread = runs[0].thread_id.clone().expect("second thread");
    assert_ne!(first_thread, second_thread);
    assert!(wait_idle(&fixture.manager, &second_thread).await);
    assert_eq!(
        fixture
            .manager
            .thread_meta(&second_thread)
            .await
            .expect("meta")
            .harness,
        HarnessKind::Codex,
        "the next run uses the harness the task now has"
    );
}

#[tokio::test]
async fn a_paused_once_task_cannot_run_the_same_slot_twice() {
    let clock = Arc::new(ManualClock::at(
        "2026-10-24T12:00:00Z"
            .parse::<DateTime<Utc>>()
            .expect("clock"),
    ));
    let fixture = fixture(FakeHarness::new(), clock).await;
    let task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            ScheduleSpec::Once {
                date: "2026-10-25".into(),
                time: "02:30".into(),
            },
            ScheduleDestination::NewThread,
        ))
        .await
        .expect("create");
    // Berlin's fall-back day: 02:30 happens at 00:30 UTC first, then 01:30 UTC.
    let first_slot = "2026-10-25T00:30:00Z"
        .parse::<DateTime<Utc>>()
        .expect("slot");
    assert_eq!(task.next_run_at, Some(first_slot));
    fixture
        .clock
        .advance(chrono::Duration::minutes(12 * 60 + 31));

    // Admit the slot, then hold its write lock so the run is still starting
    // when the pause lands: the window the bug lived in.
    let (snapshot, reservation) = fixture
        .service
        .admit(&task.task_id, Trigger::Scheduled, Some(first_slot))
        .await
        .expect("admit");
    // The slot is consumed as part of its own identity when it is admitted,
    // before a harness is asked to start and before the pause can land.
    assert_eq!(
        fixture
            .service
            .task(&task.task_id)
            .await
            .expect("task")
            .once_consumed_slot
            .as_deref(),
        Some("2026-10-25T02:30@Europe/Berlin")
    );
    assert_eq!(
        fixture
            .journal
            .schedule(&task.task_id)
            .await
            .expect("journal row")
            .expect("task row")
            .once_consumed_slot
            .as_deref(),
        Some("2026-10-25T02:30@Europe/Berlin"),
        "the spent slot survives a restart from the moment of admission"
    );
    let write = fixture.service.write_lock(&task.task_id).await;
    let running = {
        let service = fixture.service.clone();
        let prompt = snapshot.prompt.clone();
        tokio::spawn(async move {
            service
                .execute(reservation, snapshot, Trigger::Scheduled, prompt)
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        while fixture
            .manager
            .list_threads(&Default::default())
            .await
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the run should have started its thread");
    let mut paused = fixture.service.task(&task.task_id).await.expect("task");
    paused.enabled = false;
    paused.next_run_at = None;
    paused.updated_at = fixture.clock.now();
    fixture
        .service
        .inner
        .tasks
        .lock()
        .await
        .insert(paused.task_id.clone(), paused.clone());
    fixture
        .journal
        .upsert_schedule(&paused)
        .await
        .expect("save the pause");
    drop(write);
    let run = running
        .await
        .expect("the run task ends")
        .expect("the run is recorded");
    assert_eq!(run.outcome, ScheduleRunOutcome::Started);
    assert_eq!(run.scheduled_for, Some(first_slot));

    let resumed = fixture.service.set_enabled(&task.task_id, true).await;
    assert!(
        resumed.is_err(),
        "a spent once slot cannot be armed again by a pause and resume"
    );

    // The second 02:30 (CET) is 01:30 UTC. One slot, one thread.
    fixture.clock.advance(chrono::Duration::hours(1));
    fixture.service.run_due().await;
    assert_eq!(
        fixture
            .service
            .runs(&task.task_id)
            .await
            .expect("runs")
            .len(),
        1
    );
    assert_eq!(
        fixture
            .manager
            .list_threads(&Default::default())
            .await
            .len(),
        1
    );
}

#[tokio::test]
async fn a_once_slot_whose_marker_cannot_be_written_runs_only_after_it_is_durable() {
    let clock = Arc::new(ManualClock::at(
        "2026-10-24T12:00:00Z"
            .parse::<DateTime<Utc>>()
            .expect("clock"),
    ));
    let fixture = fixture(FakeHarness::new(), clock).await;
    let task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            ScheduleSpec::Once {
                date: "2026-10-25".into(),
                time: "02:30".into(),
            },
            ScheduleDestination::NewThread,
        ))
        .await
        .expect("create");
    fixture
        .clock
        .advance(chrono::Duration::minutes(12 * 60 + 31));

    // The spent marker cannot be written, so the run is refused before a
    // thread starts.
    fixture.service.fail_next_upsert();
    fixture.service.run_due().await;
    assert_eq!(
        fixture
            .manager
            .list_threads(&Default::default())
            .await
            .len(),
        0,
        "a run is refused until its slot marker is durable"
    );
    assert!(
        fixture
            .service
            .runs(&task.task_id)
            .await
            .expect("runs")
            .is_empty()
    );
    let blocked = fixture.service.task(&task.task_id).await.expect("task");
    assert!(
        !blocked.once_consumed,
        "the in-memory marker was rolled back"
    );
    assert!(
        blocked
            .next_run_at
            .is_some_and(|at| at > fixture.clock.now()),
        "the refused slot is retried on a backoff instead of spinning"
    );

    // A restart forgets the refused admission; the slot is still unspent, so it
    // runs once after the retry deadline. When that run's own write fails, only
    // the write is retried: the slot is already spent, so the retry cannot
    // start a second thread.
    let restarted = ScheduleService::open(
        Arc::clone(&fixture.journal),
        Arc::clone(&fixture.manager),
        HostId::new("host_schedule_test"),
        fixture.clock.clone(),
    )
    .await
    .expect("restart");
    fixture.clock.advance(chrono::Duration::seconds(31));
    restarted.fail_next_commit();
    restarted.run_due().await;
    assert!(
        restarted
            .runs(&task.task_id)
            .await
            .expect("runs")
            .is_empty(),
        "the run waits for its own retry"
    );
    assert_eq!(
        fixture
            .manager
            .list_threads(&Default::default())
            .await
            .len(),
        1,
        "the started thread is the only one"
    );
    fixture.clock.advance(chrono::Duration::seconds(31));
    restarted.run_due().await;
    let runs = wait_runs(&restarted, &task.task_id, 1).await;
    assert_eq!(runs[0].outcome, ScheduleRunOutcome::Started);
    assert_eq!(
        fixture
            .manager
            .list_threads(&Default::default())
            .await
            .len(),
        1
    );

    fixture.clock.advance(chrono::Duration::days(90));
    restarted.run_due().await;
    assert_eq!(
        restarted.runs(&task.task_id).await.expect("runs").len(),
        1,
        "the slot runs at most once"
    );
    assert_eq!(
        fixture
            .manager
            .list_threads(&Default::default())
            .await
            .len(),
        1
    );
}

#[tokio::test]
async fn a_used_once_slot_rearms_when_its_zone_names_another_instant() {
    let clock = Arc::new(ManualClock::at(
        "2026-10-24T12:00:00Z"
            .parse::<DateTime<Utc>>()
            .expect("clock"),
    ));
    let fixture = fixture(FakeHarness::new(), clock).await;
    let task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            ScheduleSpec::Once {
                date: "2026-10-25".into(),
                time: "02:30".into(),
            },
            ScheduleDestination::NewThread,
        ))
        .await
        .expect("create");
    fixture
        .clock
        .advance(chrono::Duration::minutes(12 * 60 + 31));
    fixture.service.run_due().await;
    wait_runs(&fixture.service, &task.task_id, 1).await;
    let spent = fixture.service.task(&task.task_id).await.expect("task");
    assert_eq!(
        spent.once_consumed_slot.as_deref(),
        Some("2026-10-25T02:30@Europe/Berlin")
    );

    // Renaming the task names the same instant, so the slot stays spent.
    let mut edit = spent.draft();
    edit.name = "Renamed once".into();
    let renamed = fixture
        .service
        .update(&task.task_id, edit)
        .await
        .expect("rename");
    assert!(renamed.once_consumed);
    assert!(renamed.next_run_at.is_none());

    // The same wall clock in another zone is another instant, so it is a new
    // slot and it runs once.
    let mut moved = renamed.draft();
    moved.timezone = "UTC".into();
    let armed = fixture
        .service
        .update(&task.task_id, moved)
        .await
        .expect("move the zone");
    assert!(!armed.once_consumed);
    assert_eq!(
        armed.next_run_at,
        Some(
            "2026-10-25T02:30:00Z"
                .parse::<DateTime<Utc>>()
                .expect("slot")
        )
    );
    fixture.clock.advance(chrono::Duration::hours(2));
    fixture.service.run_due().await;
    let runs = wait_runs(&fixture.service, &task.task_id, 2).await;
    assert_eq!(runs[0].outcome, ScheduleRunOutcome::Started);
    assert_eq!(runs[1].outcome, ScheduleRunOutcome::Started);
    assert_eq!(
        fixture
            .service
            .task(&task.task_id)
            .await
            .expect("task")
            .once_consumed_slot
            .as_deref(),
        Some("2026-10-25T02:30@UTC")
    );
}

#[tokio::test]
async fn a_late_skip_does_not_erase_the_busy_thread() {
    let (harness, gate) = FakeHarness::new().with_completion_gate();
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture(harness, clock).await;
    let task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            ScheduleSpec::Webhook,
            ScheduleDestination::NewThread,
        ))
        .await
        .expect("create");
    let path = task.webhook_path.clone().expect("path");
    let secret = path
        .strip_prefix(WEBHOOK_PREFIX)
        .expect("prefix")
        .to_owned();
    let request = || WebhookRequest {
        method: "POST".into(),
        path: path.clone(),
        query: Default::default(),
        headers: Default::default(),
        body: "{}".into(),
        secret: Some(secret.clone()),
    };
    assert_eq!(
        deliver(&fixture.service, &secret, request()).await,
        WebhookOutcome::Accepted
    );
    let runs = wait_runs(&fixture.service, &task.task_id, 1).await;
    let thread_id = runs[0].thread_id.clone().expect("thread");
    assert!(fixture.manager.thread_is_busy(&thread_id).await);

    // A skip recorded after the started run committed, from a task read taken
    // before it: its own history entry carries no thread id.
    let snapshot = fixture.service.task(&task.task_id).await.expect("task");
    let late_skip = ScheduleRun {
        run_id: 0,
        task_id: task.task_id.clone(),
        scheduled_for: None,
        started_at: fixture.clock.now(),
        thread_id: None,
        outcome: ScheduleRunOutcome::Skipped,
        detail: Some("Another delivery for this task was already starting.".into()),
    };
    fixture
        .service
        .record(
            &task.task_id,
            &snapshot,
            late_skip,
            Trigger::Webhook,
            fixture.clock.now(),
            AfterRun::Advance,
        )
        .await
        .expect("record the late skip");

    assert_eq!(
        deliver(&fixture.service, &secret, request()).await,
        WebhookOutcome::Skipped,
        "the busy thread is still the reference after a late skip"
    );
    assert_eq!(
        fixture
            .manager
            .list_threads(&Default::default())
            .await
            .len(),
        1,
        "no second thread starts while the first is working"
    );
    gate.release();
}

#[tokio::test]
async fn run_now_works_on_a_paused_task() {
    let clock = Arc::new(ManualClock::at(Utc::now()));
    let fixture = fixture(FakeHarness::new(), clock).await;
    let mut paused = draft(
        &fixture.workspace_path,
        every_minute(),
        ScheduleDestination::NewThread,
    );
    paused.enabled = false;
    let task = fixture.service.create(paused).await.expect("create paused");
    assert!(task.next_run_at.is_none());

    let run = fixture
        .service
        .run_now(&task.task_id)
        .await
        .expect("manual runs are allowed while paused");
    assert_eq!(run.outcome, ScheduleRunOutcome::Started);
    let stored = fixture.service.task(&task.task_id).await.expect("task");
    assert!(!stored.enabled, "the manual run leaves the task paused");
    assert!(stored.next_run_at.is_none());
    assert_eq!(
        fixture
            .service
            .runs(&task.task_id)
            .await
            .expect("runs")
            .len(),
        1
    );
}

#[tokio::test]
async fn a_daily_task_runs_once_in_the_repeated_fall_back_hour() {
    let clock = Arc::new(ManualClock::at(
        "2026-10-24T12:00:00Z"
            .parse::<DateTime<Utc>>()
            .expect("clock"),
    ));
    let fixture = fixture(FakeHarness::new(), clock).await;
    let task = fixture
        .service
        .create(draft(
            &fixture.workspace_path,
            ScheduleSpec::Daily {
                time: "02:30".into(),
            },
            ScheduleDestination::NewThread,
        ))
        .await
        .expect("create");
    assert_eq!(
        task.next_run_at,
        Some(
            "2026-10-25T00:30:00Z"
                .parse::<DateTime<Utc>>()
                .expect("slot")
        )
    );
    fixture
        .clock
        .advance(chrono::Duration::minutes(12 * 60 + 31));
    fixture.service.run_due().await;
    let runs = wait_runs(&fixture.service, &task.task_id, 1).await;
    assert_eq!(runs[0].outcome, ScheduleRunOutcome::Started);
    let after = fixture.service.task(&task.task_id).await.expect("task");
    assert_eq!(
        after.next_run_at,
        Some(
            "2026-10-26T01:30:00Z"
                .parse::<DateTime<Utc>>()
                .expect("next")
        ),
        "the second 02:30 is not a second run"
    );

    fixture.clock.advance(chrono::Duration::hours(1));
    fixture.service.run_due().await;
    assert_eq!(
        fixture
            .service
            .runs(&task.task_id)
            .await
            .expect("runs")
            .len(),
        1
    );
    assert_eq!(
        fixture
            .manager
            .list_threads(&Default::default())
            .await
            .len(),
        1
    );
}
