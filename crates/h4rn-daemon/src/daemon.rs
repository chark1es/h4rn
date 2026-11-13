//! Daemon assembly: builder, bound listeners and graceful shutdown.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use h4rn_harness::Harness;
use tokio::net::TcpListener;

use crate::auth::TokenStore;
use crate::config::Config;
use crate::fs::FsService;
use crate::harnesses::{HarnessCatalog, StaticHarnessCatalog};
use crate::hosts::HostInventory;
use crate::journal::{Journal, SqliteJournal};
use crate::server::{self, AppState};
use crate::supervisor::ProcessSupervisor;
use crate::terminal::{FileAudit, TerminalManager};
use crate::threads::{ThreadManager, ThreadManagerOptions};

/// Builds a daemon with injectable storage, harnesses and lifecycle timings.
pub struct DaemonBuilder {
    config: Config,
    catalog: Option<Arc<dyn HarnessCatalog>>,
    extra_harnesses: Vec<Arc<dyn Harness>>,
    journal: Option<Arc<dyn Journal>>,
    options: ThreadManagerOptions,
    supervisor: Arc<ProcessSupervisor>,
}

impl DaemonBuilder {
    /// Starts from a resolved configuration.
    pub fn new(config: Config) -> Self {
        Self {
            config,
            catalog: None,
            extra_harnesses: Vec::new(),
            journal: None,
            options: ThreadManagerOptions::default(),
            supervisor: Arc::new(ProcessSupervisor::new()),
        }
    }

    /// Replaces the harness catalog wholesale.
    pub fn with_catalog(mut self, catalog: Arc<dyn HarnessCatalog>) -> Self {
        self.catalog = Some(catalog);
        self
    }

    /// Registers one harness adapter.
    pub fn with_harness(mut self, harness: Arc<dyn Harness>) -> Self {
        self.extra_harnesses.push(harness);
        self
    }

    /// Replaces durable storage (tests use the in-memory SQLite journal).
    pub fn with_journal(mut self, journal: Arc<dyn Journal>) -> Self {
        self.journal = Some(journal);
        self
    }

    /// Overrides lifecycle timings.
    pub fn with_lifecycle(mut self, options: ThreadManagerOptions) -> Self {
        self.options = options;
        self
    }

    /// Overrides the background ticker interval.
    pub fn with_tick_interval(mut self, interval: std::time::Duration) -> Self {
        self.options.tick_interval = interval;
        self
    }

    /// Replaces the process supervisor (diagnostics and tests).
    pub fn with_supervisor(mut self, supervisor: Arc<ProcessSupervisor>) -> Self {
        self.supervisor = supervisor;
        self
    }

    /// Builds the daemon, opening storage and restoring thread projections.
    pub async fn build(self) -> anyhow::Result<Daemon> {
        self.config.ensure_data_dir()?;

        let catalog: Arc<dyn HarnessCatalog> = match self.catalog {
            Some(catalog) => catalog,
            None => {
                let mut catalog = StaticHarnessCatalog::new();
                for harness in h4rn_harness::registry::builtin_adapters() {
                    catalog.register(harness);
                }
                // The fake adapter is available to the UI e2e through an
                // explicit environment switch and never in a normal daemon.
                if std::env::var("H4RN_FAKE_HARNESS").as_deref() == Ok("1") {
                    catalog.register(Arc::new(h4rn_harness::FakeHarness::new()));
                }
                for harness in self.extra_harnesses {
                    catalog.register(harness);
                }
                Arc::new(catalog)
            }
        };

        let journal: Arc<dyn Journal> = match self.journal {
            Some(journal) => journal,
            None => Arc::new(
                SqliteJournal::connect(self.config.event_db_path())
                    .await
                    .map_err(|error| anyhow::anyhow!("open event journal: {error}"))?,
            ),
        };

        let hosts = Arc::new(HostInventory::detect(Arc::clone(&catalog)));
        let terminals = Arc::new(TerminalManager::new(
            hosts.local_id(),
            Arc::new(FileAudit::new(&self.config.data_dir)),
        ));
        let manager = Arc::new(
            ThreadManager::open(
                journal,
                Arc::clone(&catalog),
                hosts.local_id(),
                self.options,
            )
            .await
            .map_err(|error| anyhow::anyhow!("open thread manager: {error}"))?,
        );
        manager.start_ticker();

        let fs = Arc::new(
            FsService::new(&self.config.data_dir)
                .map_err(|error| anyhow::anyhow!("open filesystem service: {error}"))?,
        );
        fs.allow_configured_roots();
        for workspace in manager.workspace_paths().await {
            if let Err(error) = fs.allow_root(&workspace) {
                tracing::warn!(workspace, %error, "skipping unauthorised thread workspace");
            }
        }
        manager.set_snapshot_store(fs.snapshots());

        let tokens = TokenStore::new(&self.config.data_dir);
        // Provision on first serve so a client has a token to read.
        let _ = tokens.load_or_create()?;

        Ok(Daemon {
            config: self.config,
            manager,
            hosts,
            terminals,
            tokens,
            fs,
            supervisor: self.supervisor,
        })
    }
}

/// A built daemon that has not bound sockets yet.
pub struct Daemon {
    config: Config,
    manager: Arc<ThreadManager>,
    hosts: Arc<HostInventory>,
    terminals: Arc<TerminalManager>,
    tokens: TokenStore,
    fs: Arc<FsService>,
    supervisor: Arc<ProcessSupervisor>,
}

impl Daemon {
    /// Bound configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Thread manager handle (tests and diagnostics).
    pub fn manager(&self) -> Arc<ThreadManager> {
        Arc::clone(&self.manager)
    }

    /// Terminal manager handle (tests and diagnostics).
    pub fn terminals(&self) -> Arc<TerminalManager> {
        Arc::clone(&self.terminals)
    }

    /// Binds the primary and optional extra listener.
    pub async fn bind(self) -> anyhow::Result<BoundDaemon> {
        let mut listeners = Vec::new();
        let mut addrs = Vec::new();

        let primary = TcpListener::bind(self.config.bind)
            .await
            .map_err(|error| anyhow::anyhow!("bind {}: {error}", self.config.bind))?;
        addrs.push(primary.local_addr()?);
        listeners.push(primary);

        if let Some(extra) = self.config.extra_bind
            && extra != self.config.bind
        {
            let listener = TcpListener::bind(extra)
                .await
                .map_err(|error| anyhow::anyhow!("bind extra address {extra}: {error}"))?;
            addrs.push(listener.local_addr()?);
            listeners.push(listener);
        }

        let state = AppState {
            manager: Arc::clone(&self.manager),
            hosts: self.hosts,
            terminals: Arc::clone(&self.terminals),
            tokens: self.tokens,
            fs: self.fs,
            started: std::time::Instant::now(),
        };

        Ok(BoundDaemon {
            app: server::router(state),
            listeners,
            addrs,
            manager: self.manager,
            terminals: self.terminals,
            supervisor: self.supervisor,
            data_dir: self.config.data_dir,
        })
    }
}

/// A daemon with sockets bound, ready for [`BoundDaemon::serve`].
pub struct BoundDaemon {
    app: axum::Router,
    listeners: Vec<TcpListener>,
    addrs: Vec<SocketAddr>,
    manager: Arc<ThreadManager>,
    terminals: Arc<TerminalManager>,
    supervisor: Arc<ProcessSupervisor>,
    data_dir: PathBuf,
}

impl BoundDaemon {
    /// Primary bound address (resolves port `0` for tests).
    pub fn local_addr(&self) -> SocketAddr {
        self.addrs[0]
    }

    /// Every bound address, primary first.
    pub fn addrs(&self) -> &[SocketAddr] {
        &self.addrs
    }

    /// Thread manager for in-process tests.
    pub fn manager(&self) -> Arc<ThreadManager> {
        Arc::clone(&self.manager)
    }

    /// Serves until `shutdown` resolves, then interrupts turns and kills
    /// registered harness process groups (ADR-021).
    pub async fn serve(
        self,
        shutdown: impl std::future::Future<Output = ()>,
    ) -> anyhow::Result<()> {
        let Self {
            app,
            listeners,
            addrs,
            manager,
            terminals,
            supervisor,
            data_dir,
        } = self;

        tracing::info!(
            addresses = ?addrs,
            data_dir = %data_dir.display(),
            "h4rn daemon listening"
        );

        server::serve_all(listeners, app, shutdown).await?;
        manager.shutdown().await;
        terminals.shutdown().await;
        supervisor.shutdown().await;
        tracing::info!("h4rn daemon stopped");
        Ok(())
    }
}

/// Starts a production daemon with the default catalog.
///
/// The harness registry from `h4rn-harness` plugs in here once the Claude
/// adapter lands; this function deliberately contains no fake adapter.
pub async fn serve(config: Config) -> anyhow::Result<()> {
    let daemon = DaemonBuilder::new(config).build().await?;
    let bound = daemon.bind().await?;
    tracing::info!(addr = %bound.local_addr(), "h4rnd ready");
    bound.serve(crate::supervisor::shutdown_signal()).await
}
