//! Runner, persistence and validation tests for scheduled tasks.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use h4rn_harness::FakeHarness;
use h4rn_protocol::{
    HarnessKind, HostId, IntervalUnit, RuntimeMode, ScheduleDestination, ScheduleDraft,
    ScheduleRunOutcome, ScheduleSpec, ScheduleTaskId, ThreadId,
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
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_path = workspace.path().to_path_buf();
    let journal = match journal {
        Some(journal) => journal,
        None => Arc::new(SqliteJournal::in_memory().await.expect("journal")),
    };
    let host = HostId::new("host_schedule_test");
    let manager = Arc::new(
        ThreadManager::open(
            Arc::clone(&journal),
            Arc::new(StaticHarnessCatalog::with(Arc::new(harness))),
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
    };
    assert_eq!(
        fixture.service.deliver_webhook(&old_secret, request).await,
        WebhookOutcome::Accepted
    );
    let runs = wait_runs(&fixture.service, &task.task_id, 1).await;
    assert_eq!(runs[0].outcome, ScheduleRunOutcome::Started);

    let rotated = fixture
        .service
        .rotate_webhook(&task.task_id)
        .await
        .expect("rotate");
    assert_ne!(rotated.webhook_path, Some(old_path));
    let request = WebhookRequest {
        method: "POST".into(),
        path: "".into(),
        query: Default::default(),
        headers: Default::default(),
        body: String::new(),
    };
    assert_eq!(
        fixture.service.deliver_webhook(&old_secret, request).await,
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
