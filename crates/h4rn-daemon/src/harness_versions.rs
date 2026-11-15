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
use std::time::Instant;

use async_trait::async_trait;
use h4rn_harness::HarnessInstall;
use h4rn_harness::versions::{
    CACHE_TTL, RegistryClient, UNKNOWN_TTL, UpdateLine, UpdateRunner, detect_update_method,
    is_newer, npm_package, update_command,
};
use h4rn_protocol::{
    HarnessKind, HarnessUpdateEvent, HarnessUpdateMethod, HarnessVersionInfo, ProtoError,
    ProtoErrorCode,
};
use tokio::sync::{Mutex, broadcast};

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
            updates,
            running: Mutex::new(HashSet::new()),
        }
    }

    /// Subscribes to update output and completion events.
    pub fn subscribe(&self) -> broadcast::Receiver<HarnessUpdateEvent> {
        self.updates.subscribe()
    }

    /// Reports every harness, using the cache while it is fresh.
    pub async fn list(&self) -> Vec<HarnessVersionInfo> {
        let mut cache = self.cache.lock().await;
        if let Some(cached) = cache.as_ref()
            && Instant::now() < cached.valid_until
        {
            return cached.harnesses.clone();
        }

        let harnesses = self.refresh().await;
        let known = harnesses.iter().any(|row| row.latest.is_some());
        let ttl = if known { CACHE_TTL } else { UNKNOWN_TTL };
        *cache = Some(CachedListing {
            harnesses: harnesses.clone(),
            valid_until: Instant::now() + ttl,
        });
        harnesses
    }

    async fn refresh(&self) -> Vec<HarnessVersionInfo> {
        let mut rows = Vec::new();
        for kind in crate::harnesses::all_harness_kinds() {
            let install = self.catalog.detect(kind);
            let method = match &install {
                Some(install) => detect_update_method(kind, &install.path),
                None => HarnessUpdateMethod::Unavailable,
            };
            let latest = match npm_package(kind) {
                // One registry failure is one unknown row, never a failed list.
                Some(package) => self.registry.latest(package).await,
                None => None,
            };
            let update_available = match (&install, &latest) {
                (Some(install), Some(latest)) => is_newer(latest, &install.version),
                _ => false,
            };
            rows.push(HarnessVersionInfo {
                harness: kind,
                installed: install.as_ref().map(|install| install.version.clone()),
                latest,
                update_available,
                method,
                path: install.map(|install| install.path.display().to_string()),
            });
        }
        rows
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

            *service.cache.lock().await = None;
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
