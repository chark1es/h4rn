//! Harness CLI version reporting and updates (ADR-043).
//!
//! The daemon owns every fact the Settings → Harnesses screen shows: which
//! harness CLIs are installed here, what version they run, what the npm registry
//! publishes, and how this host's copy would be updated. The view is a pure
//! renderer; the npm registry is read behind an injectable [`RegistryClient`] and
//! update commands run behind an injectable [`UpdateRunner`] (no shell, argv
//! arrays), so tests never touch the network or a real global CLI.
//!
//! Registry answers are cached. An offline registry is reported as "unknown"
//! for every harness — never as an error — and remembered for a short while so a
//! sleeping laptop does not retry on every screen open.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use h4rn_harness::HarnessInstall;
use h4rn_harness::versions::{
    CACHE_TTL, REGISTRY_TIMEOUT, RegistryClient, UNKNOWN_TTL, UpdateLine, UpdateRunner,
    detect_update_method, is_newer, npm_package, update_command,
};
use h4rn_protocol::{
    HarnessKind, HarnessUpdateEvent, HarnessUpdateMethod, HarnessVersionInfo, ProtoError,
    ProtoErrorCode,
};
use tokio::sync::{Mutex, broadcast};
use tokio::time::Instant;

use crate::harnesses::HarnessCatalog;
use crate::threads::ThreadManager;

/// Asks whether a harness currently has a turn in flight.
///
/// Kept as a trait so the service can be exercised without a full thread
/// manager; production adapts [`ThreadManager`] directly.
#[async_trait]
pub trait TurnBusy: Send + Sync {
    /// Whether any thread on `kind` has an active turn.
    async fn harness_busy(&self, kind: HarnessKind) -> bool;
}

#[async_trait]
impl TurnBusy for ThreadManager {
    async fn harness_busy(&self, kind: HarnessKind) -> bool {
        ThreadManager::harness_busy(self, kind).await
    }
}

struct CachedListing {
    harnesses: Vec<HarnessVersionInfo>,
    valid_until: Instant,
}

/// Version reporting and update execution for the four harness CLIs.
pub struct HarnessVersionService {
    catalog: Arc<dyn HarnessCatalog>,
    registry: Arc<dyn RegistryClient>,
    runner: Arc<dyn UpdateRunner>,
    busy: Arc<dyn TurnBusy>,
    cache: Mutex<Option<CachedListing>>,
    /// Bumped whenever the cache is cleared so a refresh that started before
    /// the invalidation cannot write its pre-invalidation snapshot back.
    generation: AtomicU64,
    refresh: Mutex<()>,
    updates: broadcast::Sender<HarnessUpdateEvent>,
    running: Mutex<HashSet<HarnessKind>>,
}

impl HarnessVersionService {
    /// Builds the service over a catalog, registry and process runner.
    pub fn new(
        catalog: Arc<dyn HarnessCatalog>,
        registry: Arc<dyn RegistryClient>,
        runner: Arc<dyn UpdateRunner>,
        busy: Arc<dyn TurnBusy>,
    ) -> Self {
        let (updates, _) = broadcast::channel(256);
        Self {
            catalog,
            registry,
            runner,
            busy,
            cache: Mutex::new(None),
            generation: AtomicU64::new(0),
            refresh: Mutex::new(()),
            updates,
            running: Mutex::new(HashSet::new()),
        }
    }

    /// Subscribes to update output and completion events.
    pub fn subscribe(&self) -> broadcast::Receiver<HarnessUpdateEvent> {
        self.updates.subscribe()
    }

    /// Reports every harness, using the cache while it is fresh.
    ///
    /// A cache miss refreshes at most once at a time and never holds the cache
    /// lock across a network await, so a black-holed registry costs one bounded
    /// wait ([`REGISTRY_TIMEOUT`]) instead of stalling every other request on
    /// the connection. Offline rows are `latest: None`, never an error.
    pub async fn list(&self) -> Vec<HarnessVersionInfo> {
        if let Some(cached) = self.cached().await {
            return cached;
        }

        // A single in-flight refresh serves concurrent callers; anyone who
        // waited finds the fresh cache instead of probing again.
        let _refreshing = self.refresh.lock().await;
        if let Some(cached) = self.cached().await {
            return cached;
        }

        let generation = self.generation.load(Ordering::SeqCst);
        let harnesses = self.refresh().await;
        let known = harnesses.iter().any(|row| row.latest.is_some());
        let ttl = if known { CACHE_TTL } else { UNKNOWN_TTL };
        {
            let mut cache = self.cache.lock().await;
            // An update that finished while this refresh was probing cleared
            // the cache on purpose; storing the pre-update snapshot here would
            // show the old version (and a phantom update) until the TTL ends.
            if self.generation.load(Ordering::SeqCst) == generation {
                *cache = Some(CachedListing {
                    harnesses: harnesses.clone(),
                    valid_until: Instant::now() + ttl,
                });
            }
        }
        harnesses
    }

    /// Clears the cached listing for the next caller.
    async fn invalidate(&self) {
        let mut cache = self.cache.lock().await;
        *cache = None;
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    async fn cached(&self) -> Option<Vec<HarnessVersionInfo>> {
        let cache = self.cache.lock().await;
        cache
            .as_ref()
            .filter(|cached| Instant::now() < cached.valid_until)
            .map(|cached| cached.harnesses.clone())
    }

    async fn refresh(&self) -> Vec<HarnessVersionInfo> {
        // Detect the local installations first: this part is filesystem-only and
        // must still produce a full listing when the registry is unreachable.
        let mut entries = Vec::with_capacity(4);
        for kind in crate::harnesses::all_harness_kinds() {
            let install = self.catalog.detect(kind);
            let method = match &install {
                Some(install) => detect_update_method(kind, &install.path),
                None => HarnessUpdateMethod::Unavailable,
            };
            entries.push((kind, install, method, npm_package(kind)));
        }

        // Probe every npm-distributed harness concurrently, each behind its own
        // deadline. One stalled lookup must not discard the answers that did
        // arrive: an unreachable package is one unknown row.
        let packages: Vec<&'static str> = entries
            .iter()
            .filter_map(|(_, _, _, package)| *package)
            .collect();
        let latest = futures::future::join_all(packages.into_iter().map(|package| async move {
            match tokio::time::timeout(REGISTRY_TIMEOUT, self.registry.latest(package)).await {
                Ok(latest) => latest,
                Err(_) => {
                    tracing::debug!(
                        package,
                        "harness registry lookup timed out; reporting an unknown version"
                    );
                    None
                }
            }
        }))
        .await;

        let mut latest = latest.into_iter();
        entries
            .into_iter()
            .map(|(kind, install, method, package)| {
                let latest = if package.is_some() {
                    latest.next().flatten()
                } else {
                    None
                };
                let update_available = match (&install, &latest) {
                    (Some(install), Some(latest)) => is_newer(latest, &install.version),
                    _ => false,
                };
                HarnessVersionInfo {
                    harness: kind,
                    installed: install.as_ref().map(|install| install.version.clone()),
                    latest,
                    update_available,
                    method,
                    path: install.map(|install| install.path.display().to_string()),
                }
            })
            .collect()
    }

    /// Starts one harness update, refusing when it is unsafe.
    ///
    /// Output and the result arrive on [`Self::subscribe`]; the request itself
    /// answers as soon as the command is spawned.
    pub async fn start_update(self: &Arc<Self>, kind: HarnessKind) -> Result<(), ProtoError> {
        let install = self
            .catalog
            .detect(kind)
            .ok_or_else(|| not_installed(kind))?;

        {
            let mut running = self.running.lock().await;
            if !running.insert(kind) {
                return Err(ProtoError::new(
                    ProtoErrorCode::Conflict,
                    format!("{} is already updating.", display_name(kind)),
                    "Wait for the running update to finish.",
                ));
            }
        }

        // Release the guard before any await so an active turn is not queued
        // behind a lock held across the busy check.
        let busy = self.busy.harness_busy(kind).await;
        if busy {
            self.running.lock().await.remove(&kind);
            return Err(ProtoError::new(
                ProtoErrorCode::Conflict,
                format!("{} is working on a turn.", display_name(kind)),
                "Finish or stop the turn, then try again.",
            ));
        }

        let method = detect_update_method(kind, &install.path);
        let Some((program, args)) = update_command(&method) else {
            self.running.lock().await.remove(&kind);
            return Err(manual_or_unknown(kind, &method));
        };

        self.spawn_update(kind, install.version, program, args);
        Ok(())
    }

    fn spawn_update(
        self: &Arc<Self>,
        kind: HarnessKind,
        from_version: String,
        program: String,
        args: Vec<String>,
    ) {
        let service = Arc::clone(self);
        tokio::spawn(async move {
            let sender = service.updates.clone();
            let result = service
                .runner
                .run(&program, &args, &move |line: UpdateLine| {
                    let _ = sender.send(HarnessUpdateEvent::Output {
                        harness: kind,
                        stream: line.stream,
                        line: line.text,
                    });
                })
                .await;

            // Re-detect after the command so the person sees the version that
            // is actually on disk, not the one the registry promised.
            let to_version = service
                .catalog
                .detect(kind)
                .map(|install: HarnessInstall| install.version);
            let (ok, message) = match result {
                Ok(0) => (
                    true,
                    success_message(kind, &from_version, to_version.as_deref()),
                ),
                Ok(code) => (
                    false,
                    format!(
                        "The {} update command exited with code {code}.",
                        display_name(kind)
                    ),
                ),
                Err(error) => (
                    false,
                    format!("{} could not be updated: {error}", display_name(kind)),
                ),
            };

            service.invalidate().await;
            service.running.lock().await.remove(&kind);
            let _ = service.updates.send(HarnessUpdateEvent::Finished {
                harness: kind,
                ok,
                message,
                from_version: Some(from_version),
                to_version,
            });
        });
    }
}

fn display_name(kind: HarnessKind) -> &'static str {
    match kind {
        HarnessKind::ClaudeCode => "Claude Code",
        HarnessKind::Codex => "Codex",
        HarnessKind::Opencode => "OpenCode",
        HarnessKind::Antigravity => "Antigravity",
    }
}

fn success_message(kind: HarnessKind, from: &str, to: Option<&str>) -> String {
    match to {
        Some(to) if to != from => {
            format!("Updated {} from {from} to {to}.", display_name(kind))
        }
        Some(to) => format!("{} is up to date at {to}.", display_name(kind)),
        None => format!("Updated {}.", display_name(kind)),
    }
}

fn not_installed(kind: HarnessKind) -> ProtoError {
    ProtoError::new(
        ProtoErrorCode::NotFound,
        format!("{} is not installed on this host.", display_name(kind)),
        "Install it, then check again.",
    )
}

fn manual_or_unknown(kind: HarnessKind, method: &HarnessUpdateMethod) -> ProtoError {
    match method {
        HarnessUpdateMethod::Manual { command } => ProtoError::new(
            ProtoErrorCode::Unsupported,
            format!(
                "h4rn cannot update {} automatically on this host.",
                display_name(kind)
            ),
            format!("Run `{command}` in a terminal."),
        ),
        _ => ProtoError::new(
            ProtoErrorCode::Unsupported,
            format!(
                "h4rn does not know an update source for {}.",
                display_name(kind)
            ),
            "Update it with the vendor's own installer.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    use crate::harnesses::StaticHarnessCatalog;
    use async_trait::async_trait;
    use h4rn_harness::{Harness, HarnessError, HarnessSession, SessionSpec};
    use h4rn_protocol::HarnessOutputStream;

    use super::*;

    struct ProbeHarness {
        kind: HarnessKind,
        install: Option<HarnessInstall>,
    }

    #[async_trait]
    impl Harness for ProbeHarness {
        fn kind(&self) -> HarnessKind {
            self.kind
        }

        fn detect(&self) -> Option<HarnessInstall> {
            self.install.clone()
        }

        async fn start_session(
            &self,
            _spec: SessionSpec,
        ) -> Result<Box<dyn HarnessSession>, HarnessError> {
            Err(HarnessError::Unsupported {
                harness: self.kind,
                feature: "sessions".into(),
            })
        }
    }

    fn catalog_with(kind: HarnessKind, install: Option<HarnessInstall>) -> Arc<dyn HarnessCatalog> {
        Arc::new(StaticHarnessCatalog::with(Arc::new(ProbeHarness {
            kind,
            install,
        })))
    }

    /// An npm-global copy, which h4rn is allowed to update itself.
    fn install(version: &str) -> HarnessInstall {
        HarnessInstall {
            path: PathBuf::from("/usr/lib/node_modules/@openai/codex/bin/codex"),
            version: version.into(),
        }
    }

    struct FixedRegistry(Option<&'static str>);

    #[async_trait]
    impl RegistryClient for FixedRegistry {
        async fn latest(&self, _package: &str) -> Option<String> {
            self.0.map(str::to_string)
        }
    }

    struct ScriptedRunner {
        code: i32,
        calls: StdMutex<Vec<(String, Vec<String>)>>,
    }

    #[async_trait]
    impl UpdateRunner for ScriptedRunner {
        async fn run(
            &self,
            program: &str,
            args: &[String],
            on_line: &(dyn Fn(UpdateLine) + Send + Sync),
        ) -> Result<i32, String> {
            self.calls
                .lock()
                .expect("calls")
                .push((program.to_string(), args.to_vec()));
            on_line(UpdateLine {
                stream: HarnessOutputStream::Stdout,
                text: "updated".into(),
            });
            Ok(self.code)
        }
    }

    struct Busy(bool);

    #[async_trait]
    impl TurnBusy for Busy {
        async fn harness_busy(&self, _kind: HarnessKind) -> bool {
            self.0
        }
    }

    fn service(
        install: Option<HarnessInstall>,
        latest: Option<&'static str>,
        busy: bool,
    ) -> (
        Arc<HarnessVersionService>,
        Arc<ScriptedRunner>,
        broadcast::Receiver<HarnessUpdateEvent>,
    ) {
        let runner = Arc::new(ScriptedRunner {
            code: 0,
            calls: StdMutex::new(Vec::new()),
        });
        let service = Arc::new(HarnessVersionService::new(
            catalog_with(HarnessKind::Codex, install),
            Arc::new(FixedRegistry(latest)),
            runner.clone(),
            Arc::new(Busy(busy)),
        ));
        let events = service.subscribe();
        (service, runner, events)
    }

    #[tokio::test]
    async fn reports_installed_latest_and_update_availability() {
        let (service, _, _) = service(Some(install("0.168.0")), Some("0.169.0"), false);
        let rows = service.list().await;
        let codex = rows
            .iter()
            .find(|row| row.harness == HarnessKind::Codex)
            .expect("codex row");
        assert_eq!(codex.installed.as_deref(), Some("0.168.0"));
        assert_eq!(codex.latest.as_deref(), Some("0.169.0"));
        assert!(codex.update_available);
        assert_eq!(
            codex.path.as_deref(),
            Path::new("/usr/lib/node_modules/@openai/codex/bin/codex").to_str()
        );
    }

    #[tokio::test]
    async fn a_missing_harness_is_reported_not_hidden() {
        let (service, _, _) = service(None, Some("0.169.0"), false);
        let rows = service.list().await;
        assert_eq!(rows.len(), 4);
        let codex = rows
            .iter()
            .find(|row| row.harness == HarnessKind::Codex)
            .expect("codex row");
        assert_eq!(codex.installed, None);
        assert!(!codex.update_available);
        assert_eq!(codex.method, HarnessUpdateMethod::Unavailable);
    }

    #[tokio::test]
    async fn an_offline_registry_is_unknown_not_an_error() {
        let (service, _, _) = service(Some(install("0.168.0")), None, false);
        let rows = service.list().await;
        let codex = rows
            .iter()
            .find(|row| row.harness == HarnessKind::Codex)
            .expect("codex row");
        assert_eq!(codex.installed.as_deref(), Some("0.168.0"));
        assert_eq!(codex.latest, None);
        assert!(!codex.update_available);
    }

    #[tokio::test]
    async fn the_registry_answer_is_cached() {
        let registry = Arc::new(CountingRegistry::default());
        let service = Arc::new(HarnessVersionService::new(
            catalog_with(HarnessKind::Codex, Some(install("0.168.0"))),
            registry.clone(),
            Arc::new(ScriptedRunner {
                code: 0,
                calls: StdMutex::new(Vec::new()),
            }),
            Arc::new(Busy(false)),
        ));
        service.list().await;
        service.list().await;
        // One registry read per npm-distributed harness, and none on the
        // second listing.
        assert_eq!(registry.calls(), 3);
    }

    #[tokio::test]
    async fn an_in_flight_refresh_cannot_write_back_after_an_update() {
        struct GatedRegistry {
            started: tokio::sync::mpsc::UnboundedSender<()>,
            release: tokio::sync::Semaphore,
            calls: std::sync::atomic::AtomicUsize,
        }

        #[async_trait]
        impl RegistryClient for GatedRegistry {
            async fn latest(&self, _package: &str) -> Option<String> {
                let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if call < 3 {
                    let _ = self.started.send(());
                    // Block until the update has invalidated the cache.
                    let _permit = self.release.acquire().await.expect("release");
                    return Some("1.0.0".to_owned());
                }
                Some("2.0.0".to_owned())
            }
        }

        let (started, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let registry = Arc::new(GatedRegistry {
            started,
            release: tokio::sync::Semaphore::new(0),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let service = Arc::new(HarnessVersionService::new(
            catalog_with(HarnessKind::Codex, Some(install("0.168.0"))),
            registry.clone(),
            Arc::new(ScriptedRunner {
                code: 0,
                calls: StdMutex::new(Vec::new()),
            }),
            Arc::new(Busy(false)),
        ));

        // A refresh starts and all three lookups block inside the registry.
        let listing = tokio::spawn({
            let service = Arc::clone(&service);
            async move { service.list().await }
        });
        for _ in 0..3 {
            started_rx.recv().await.expect("a lookup started");
        }

        // An update finishes while the refresh is still in flight.
        service.invalidate().await;

        registry.release.add_permits(3);
        let stale = listing.await.expect("listing");
        // The in-flight caller still receives the rows it asked for...
        assert!(
            stale
                .iter()
                .any(|row| row.latest.as_deref() == Some("1.0.0")),
            "the in-flight listing lost its rows"
        );

        // ...but they were not cached: the next list re-probes and sees the
        // version published after the update.
        let rows = service.list().await;
        assert!(
            rows.iter()
                .any(|row| row.latest.as_deref() == Some("2.0.0")),
            "the stale refresh was written back over the invalidation"
        );
        assert_eq!(registry.calls.load(std::sync::atomic::Ordering::SeqCst), 6);
    }

    #[tokio::test(start_paused = true)]
    async fn an_all_unknown_listing_is_retried_soon() {
        #[derive(Default)]
        struct CountingMissingRegistry {
            calls: StdMutex<u32>,
        }

        #[async_trait]
        impl RegistryClient for CountingMissingRegistry {
            async fn latest(&self, _package: &str) -> Option<String> {
                *self.calls.lock().expect("calls") += 1;
                None
            }
        }

        let registry = Arc::new(CountingMissingRegistry::default());
        let service = Arc::new(HarnessVersionService::new(
            catalog_with(HarnessKind::Codex, Some(install("0.168.0"))),
            registry.clone(),
            Arc::new(ScriptedRunner {
                code: 0,
                calls: StdMutex::new(Vec::new()),
            }),
            Arc::new(Busy(false)),
        ));

        service.list().await;
        service.list().await;
        // The unknown answer is cached, so opening Settings twice is cheap.
        assert_eq!(*registry.calls.lock().expect("calls"), 3);

        // But a person who reconnects and presses "Check again" does not wait
        // minutes for the stale answer to expire.
        tokio::time::advance(UNKNOWN_TTL + Duration::from_secs(1)).await;
        service.list().await;
        assert_eq!(*registry.calls.lock().expect("calls"), 6);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_lookup_keeps_the_completed_answers() {
        struct PartlyHangingRegistry;

        #[async_trait]
        impl RegistryClient for PartlyHangingRegistry {
            async fn latest(&self, package: &str) -> Option<String> {
                match package {
                    "@openai/codex" => {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        None
                    }
                    "@anthropic-ai/claude-code" => Some("2.1.0".into()),
                    "opencode-ai" => Some("2.2.0".into()),
                    _ => None,
                }
            }
        }

        let service = Arc::new(HarnessVersionService::new(
            catalog_with(HarnessKind::Codex, Some(install("0.168.0"))),
            Arc::new(PartlyHangingRegistry),
            Arc::new(ScriptedRunner {
                code: 0,
                calls: StdMutex::new(Vec::new()),
            }),
            Arc::new(Busy(false)),
        ));

        let rows = service.list().await;
        let known: Vec<_> = rows
            .iter()
            .filter(|row| row.latest.is_some())
            .map(|row| (row.harness, row.latest.clone()))
            .collect();
        assert_eq!(
            known,
            vec![
                (HarnessKind::ClaudeCode, Some("2.1.0".to_owned())),
                (HarnessKind::Opencode, Some("2.2.0".to_owned())),
            ],
            "a stalled lookup discarded the completed answers"
        );
        let codex = rows
            .iter()
            .find(|row| row.harness == HarnessKind::Codex)
            .expect("codex row");
        assert_eq!(codex.latest, None);
    }

    #[tokio::test]
    async fn a_black_holed_registry_cannot_stall_the_listing() {
        struct HangingRegistry;

        #[async_trait]
        impl RegistryClient for HangingRegistry {
            async fn latest(&self, _package: &str) -> Option<String> {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                None
            }
        }

        let service = Arc::new(HarnessVersionService::new(
            catalog_with(HarnessKind::Codex, Some(install("0.168.0"))),
            Arc::new(HangingRegistry),
            Arc::new(ScriptedRunner {
                code: 0,
                calls: StdMutex::new(Vec::new()),
            }),
            Arc::new(Busy(false)),
        ));

        let started = std::time::Instant::now();
        let rows = service.list().await;
        assert!(
            started.elapsed() <= REGISTRY_TIMEOUT + std::time::Duration::from_secs(2),
            "list waited {:?}",
            started.elapsed()
        );

        let codex = rows
            .iter()
            .find(|row| row.harness == HarnessKind::Codex)
            .expect("codex row");
        assert_eq!(codex.installed.as_deref(), Some("0.168.0"));
        assert_eq!(codex.latest, None);
        assert!(!codex.update_available);
    }

    #[tokio::test]
    async fn registry_lookups_run_concurrently() {
        #[derive(Default)]
        struct SlowRegistry {
            starts: StdMutex<Vec<std::time::Instant>>,
        }

        #[async_trait]
        impl RegistryClient for SlowRegistry {
            async fn latest(&self, _package: &str) -> Option<String> {
                self.starts
                    .lock()
                    .expect("starts")
                    .push(std::time::Instant::now());
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                Some("0.169.0".into())
            }
        }

        let registry = Arc::new(SlowRegistry::default());
        let service = Arc::new(HarnessVersionService::new(
            catalog_with(HarnessKind::Codex, Some(install("0.168.0"))),
            registry.clone(),
            Arc::new(ScriptedRunner {
                code: 0,
                calls: StdMutex::new(Vec::new()),
            }),
            Arc::new(Busy(false)),
        ));

        let started = std::time::Instant::now();
        let rows = service.list().await;
        // Three serial 300ms probes would take 900ms; concurrent ones ~300ms.
        assert!(
            started.elapsed() < std::time::Duration::from_millis(700),
            "list took {:?}",
            started.elapsed()
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.latest.as_deref() == Some("0.169.0"))
                .count(),
            3
        );

        let starts = registry.starts.lock().expect("starts");
        assert_eq!(starts.len(), 3);
        let first = *starts.iter().min().expect("a start");
        let spread = starts
            .iter()
            .map(|start| start.duration_since(first))
            .max()
            .expect("a spread");
        assert!(
            spread < std::time::Duration::from_millis(100),
            "lookups started {spread:?} apart"
        );
    }

    #[derive(Default)]
    struct CountingRegistry {
        calls: StdMutex<u32>,
    }

    impl CountingRegistry {
        fn calls(&self) -> u32 {
            *self.calls.lock().expect("calls")
        }
    }

    #[async_trait]
    impl RegistryClient for CountingRegistry {
        async fn latest(&self, _package: &str) -> Option<String> {
            *self.calls.lock().expect("calls") += 1;
            Some("0.169.0".into())
        }
    }

    #[tokio::test]
    async fn updates_stream_output_and_finish() {
        let (service, runner, mut events) =
            service(Some(install("0.168.0")), Some("0.169.0"), false);
        service
            .start_update(HarnessKind::Codex)
            .await
            .expect("update starts");
        let mut lines = Vec::new();
        let finished = loop {
            match events.recv().await.expect("event") {
                HarnessUpdateEvent::Output { line, .. } => lines.push(line),
                finished @ HarnessUpdateEvent::Finished { .. } => break finished,
            }
        };
        assert_eq!(lines, vec!["updated".to_string()]);
        match finished {
            HarnessUpdateEvent::Finished {
                ok, from_version, ..
            } => {
                assert!(ok);
                assert_eq!(from_version.as_deref(), Some("0.168.0"));
            }
            other => panic!("expected a finish, got {other:?}"),
        }
        assert_eq!(
            runner.calls.lock().expect("calls").as_slice(),
            &[(
                "npm".to_string(),
                vec![
                    "i".to_string(),
                    "-g".to_string(),
                    "@openai/codex@latest".to_string()
                ]
            )]
        );
    }

    #[tokio::test]
    async fn an_active_turn_refuses_the_update() {
        let (service, runner, _) = service(Some(install("0.168.0")), Some("0.169.0"), true);
        let error = service
            .start_update(HarnessKind::Codex)
            .await
            .expect_err("busy harness is refused");
        assert_eq!(error.code, ProtoErrorCode::Conflict);
        assert!(error.message.contains("working on a turn"));
        assert!(runner.calls.lock().expect("calls").is_empty());
    }

    #[tokio::test]
    async fn a_manual_install_names_the_command_instead_of_running_it() {
        let (service, runner, _) = service(
            Some(HarnessInstall {
                path: PathBuf::from("/opt/homebrew/bin/codex"),
                version: "0.168.0".into(),
            }),
            Some("0.169.0"),
            false,
        );
        let error = service
            .start_update(HarnessKind::Codex)
            .await
            .expect_err("manual install is not run");
        assert_eq!(error.code, ProtoErrorCode::Unsupported);
        assert!(
            error
                .hint
                .as_deref()
                .is_some_and(|hint| hint.contains("npm i -g @openai/codex@latest"))
        );
        assert!(runner.calls.lock().expect("calls").is_empty());
    }

    #[tokio::test]
    async fn an_uninstalled_harness_cannot_be_updated() {
        let (service, _, _) = service(None, None, false);
        let error = service
            .start_update(HarnessKind::Codex)
            .await
            .expect_err("missing harness");
        assert_eq!(error.code, ProtoErrorCode::NotFound);
    }

    #[tokio::test]
    async fn a_second_update_is_refused_while_one_runs() {
        let (service, _, _) = service(Some(install("0.168.0")), Some("0.169.0"), false);
        service
            .start_update(HarnessKind::Codex)
            .await
            .expect("first update starts");
        let error = service
            .start_update(HarnessKind::Codex)
            .await
            .expect_err("second update is refused");
        assert_eq!(error.code, ProtoErrorCode::Conflict);
    }
}
