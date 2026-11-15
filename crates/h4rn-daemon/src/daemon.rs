//! Daemon assembly: builder, bound listeners and graceful shutdown.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use h4rn_harness::Harness;
use h4rn_protocol::{MachineHostInput, MachineHostKind};
use h4rn_transport::keys::{DeviceIdentity, FileKeyStore, KEY_LEN, load_or_create};
use h4rn_transport::server::{SecureOptions, SecureService};
use h4rn_transport::wire::TOKEN_LEN;
use tokio::net::TcpListener;

use crate::auth::TokenStore;
use crate::blobs::{AttachmentResolver, BlobStore};
use crate::config::Config;
use crate::forge::ForgeService;
use crate::fs::FsService;
use crate::harnesses::{HarnessCatalog, StaticHarnessCatalog};
use crate::hosts::HostInventory;
use crate::journal::{Journal, SqliteJournal};
use crate::mcp::McpService;
use crate::remote::{
    ConnectClient, DeviceRegistry, RemoteState, TailscaleMode, advertised_addresses,
    pairing::{invalidate_direct_pairing, read_direct_pairing, remove_direct_pairing},
};
use crate::server::{self, AppState};
use crate::subagents::{SubagentManager, SubagentOptions, SubagentProvider};
use crate::supervisor::ProcessSupervisor;
use crate::terminal::{FileAudit, TerminalManager};
use crate::threads::{McpSessionProvider, ThreadManager, ThreadManagerOptions};

/// Builds a daemon with injectable storage, harnesses and lifecycle timings.
pub struct DaemonBuilder {
    config: Config,
    catalog: Option<Arc<dyn HarnessCatalog>>,
    extra_harnesses: Vec<Arc<dyn Harness>>,
    journal: Option<Arc<dyn Journal>>,
    options: ThreadManagerOptions,
    supervisor: Arc<ProcessSupervisor>,
    secure: Option<Arc<SecureService>>,
    mcp: Option<Arc<McpService>>,
    mcp_gate: Option<Arc<dyn h4rn_mcp::sharing::ConfirmationGate>>,
    subagent_options: SubagentOptions,
    devices: Option<Arc<DeviceRegistry>>,
    local_host: Option<(h4rn_protocol::HostId, String)>,
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
            secure: None,
            mcp: None,
            mcp_gate: None,
            subagent_options: SubagentOptions::default(),
            devices: None,
            local_host: None,
        }
    }

    /// Overrides the detected local host id and display name.
    pub fn with_local_host(mut self, id: h4rn_protocol::HostId, name: impl Into<String>) -> Self {
        self.local_host = Some((id, name.into()));
        self
    }

    /// Replaces the MCP hub (tests and embedders that bring their own storage).
    pub fn with_mcp(mut self, mcp: Arc<McpService>) -> Self {
        self.mcp = Some(mcp);
        self
    }

    /// Replaces the confirmation gate used for shared tool calls.
    pub fn with_mcp_confirmation_gate(
        mut self,
        gate: Arc<dyn h4rn_mcp::sharing::ConfirmationGate>,
    ) -> Self {
        self.mcp_gate = Some(gate);
        self
    }

    /// Enables the Noise-encrypted multipath transport (ADR-009, ADR-010).
    ///
    /// Without this (and without `H4RN_SECURE=1`) `/secure` answers 404 and
    /// remote clients are expected to use an SSH tunnel or Tailscale.
    pub fn with_secure(mut self, secure: Arc<SecureService>) -> Self {
        self.secure = Some(secure);
        self
    }

    /// Supplies the device registry instead of opening `devices.json` in the
    /// data directory (tests and embedders). Remote access is authorised by
    /// this registry alone.
    pub fn with_devices(mut self, devices: Arc<DeviceRegistry>) -> Self {
        self.devices = Some(devices);
        self
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

    /// Overrides subagent policy (nesting depth, default budgets, tick).
    pub fn with_subagent_options(mut self, options: SubagentOptions) -> Self {
        self.subagent_options = options;
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

        let mut inventory = HostInventory::detect(Arc::clone(&catalog));
        if let Some((id, name)) = self.local_host {
            inventory = inventory.with_local(id, name);
        }
        let hosts = Arc::new(inventory);
        let terminals = Arc::new(TerminalManager::new(
            hosts.local_id(),
            Arc::new(FileAudit::new(&self.config.data_dir)),
        ));
        let blobs = Arc::new(
            BlobStore::open(
                self.config.blob_dir(),
                self.config.attachments_dir(),
                self.config.max_blob_bytes(),
                self.config.blob_gc_grace(),
            )
            .await
            .map_err(|error| anyhow::anyhow!("open attachment store: {error}"))?,
        );
        crate::blobs::spawn_gc(&blobs, self.config.blob_gc_interval());
        let resolver: Arc<dyn AttachmentResolver> =
            Arc::clone(&blobs) as Arc<dyn AttachmentResolver>;
        let manager = Arc::new(
            ThreadManager::open(
                journal,
                Arc::clone(&catalog),
                hosts.local_id(),
                self.options,
                Some(resolver),
            )
            .await
            .map_err(|error| anyhow::anyhow!("open thread manager: {error}"))?,
        );
        manager.start_ticker();

        // Profiles and their keys are daemon-global; sessions pick them up
        // through `ThreadManager::set_proxy` at start time (ADR-015).
        let proxy = Arc::new(
            h4rn_proxy::ProxyService::open(
                self.config.proxy_dir(),
                Arc::new(crate::secrets::DaemonSecretStore::open(
                    self.config.proxy_secret_dir(),
                )?),
                h4rn_proxy::DEFAULT_CAPABILITY_TTL,
            )
            .map_err(|error| anyhow::anyhow!("open CLIProxyAPI profiles: {error}"))?,
        );
        manager.set_proxy(Arc::clone(&proxy));

        // Durable metering and auto-resume share the daemon data directory but
        // own a separate SQLite file so ledger writes never block the journal.
        let meter = Arc::new(
            h4rn_meter::MeterHub::open(self.config.meter_db_path())
                .await
                .map_err(|error| anyhow::anyhow!("open usage ledger: {error}"))?,
        );
        manager.set_meter(Arc::clone(&meter));
        manager.resync_meter_state().await;

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

        let forge = ForgeService::open(&self.config.data_dir)
            .map_err(|error| anyhow::anyhow!("open the forge service: {error}"))?;

        let tokens = TokenStore::new(&self.config.data_dir);
        // Provision on first serve so a client has a token to read.
        let _ = tokens.load_or_create()?;

        // One identity serves the Noise listener, the connect device proof and
        // every pairing payload, so a device is the same device on every path.
        let identity = load_or_create(&FileKeyStore::new(self.config.transport_identity_path()))
            .map_err(|error| anyhow::anyhow!("load the transport identity: {error}"))?;

        // A signed-in daemon loads its connect session before deciding whether
        // the secure listener is needed: remote access implies `/secure`.
        let devices = self
            .devices
            .clone()
            .unwrap_or_else(|| Arc::new(DeviceRegistry::open(self.config.devices_path())));
        let remote = RemoteState::open(
            &self.config.data_dir,
            identity.clone(),
            Arc::clone(&devices),
        )?;

        // The skill registry lives beside the journal and is shared by every
        // client of this daemon. Sync is only wired when connect is signed in;
        // the service reports `Offline` otherwise instead of hiding the screen.
        let sync_api: Option<Arc<dyn h4rn_skills::SkillSyncApi>> = remote
            .as_ref()
            .map(|remote| Arc::clone(&remote.client) as Arc<dyn h4rn_skills::SkillSyncApi>);
        let skills = Arc::new(
            h4rn_skills::SkillsService::open(&self.config.data_dir, hosts.local_id(), sync_api)
                .await
                .map_err(|error| anyhow::anyhow!("open the skill registry: {error}"))?,
        );

        let secure = match self.secure {
            Some(service) => Some(service),
            None => secure_from_env(identity.clone(), remote.is_some())?,
        };

        if let Some(service) = &secure {
            // The registry is the only source of authorisation, so the
            // transport allowlist is rebuilt from it. A key the old allowlist
            // file holds but the registry does not (a pairing from before
            // scopes were recorded) is not admitted: it must pair again.
            let known: std::collections::HashSet<_> = devices.admitted_keys().into_iter().collect();
            for key in &known {
                service.allow(*key);
            }
            match h4rn_transport::keys::read_allowlist(&self.config.allowlist_path()) {
                Ok(keys) => {
                    let orphaned = keys.iter().filter(|key| !known.contains(*key)).count();
                    if orphaned > 0 {
                        tracing::warn!(
                            orphaned,
                            "ignoring paired devices with no recorded authorisation; pair them again"
                        );
                    }
                }
                Err(error) => tracing::debug!(%error, "no legacy allowlist to compare"),
            }
            service.set_allowlist_path(self.config.allowlist_path());
            // A device that pairs through the one-time secret gets the scopes
            // the pairing was armed with, recorded before it is allowed.
            let registry = Arc::clone(&devices);
            service.set_pairing_hook(Arc::new(move |key| registry.complete_direct(key)));
            // Invalidate the durable pairing file the moment its secret is
            // consumed, before the device is authorised: a restart must never
            // re-arm a used QR.
            let pairing_dir = self.config.data_dir.clone();
            service.set_psk_consumed_hook(Arc::new(move |psk| {
                match invalidate_direct_pairing(&pairing_dir, &psk) {
                    Ok(true) => tracing::info!("deleted the consumed direct pairing file"),
                    Ok(false) => {}
                    Err(error) => {
                        tracing::warn!(%error, "could not delete the consumed direct pairing file")
                    }
                }
            }));
            tracing::info!(
                devices = service.allowed_devices().len(),
                "secure transport enabled"
            );
        }

        // Without an injected gate the service installs its own approval gate,
        // which raises a card and waits for the owner.
        let gate = self.mcp_gate.clone();
        let mcp = match self.mcp {
            Some(mcp) => mcp,
            None => Arc::new(
                McpService::open(
                    &self.config,
                    hosts.local_id().clone(),
                    remote.as_ref().map(|remote| Arc::clone(&remote.audit)),
                    gate,
                )
                .await?,
            ),
        };
        mcp.set_devices(Arc::clone(&devices));
        mcp.peers().attach_devices(Arc::clone(&devices));
        // Peer daemons are admitted on, and revoked from, the same transport
        // that serves `/secure-mcp`.
        if let Some(service) = &secure {
            mcp.peers().attach_secure(Arc::clone(service));
        }
        // The endpoint is unknown until `bind`; until then `prepare` returns
        // `None` and a session starts without MCP rather than with a bogus URL.
        manager.set_mcp_provider(Arc::clone(&mcp) as Arc<dyn McpSessionProvider>);

        // Delegation rides the same hub and namespace as the diagnostic tools;
        // each session's bearer token scopes a call to its own thread.
        let subagents = Arc::new(
            SubagentManager::open(
                Arc::clone(&manager),
                Arc::clone(&catalog),
                self.config.data_dir.clone(),
                self.subagent_options,
            )
            .await
            .map_err(|error| anyhow::anyhow!("open the subagent supervisor: {error}"))?,
        );
        mcp.hub()
            .register_provider(Arc::new(SubagentProvider::new(Arc::clone(&subagents))));
        subagents.start();

        Ok(Daemon {
            config: self.config,
            manager,
            subagents,
            hosts,
            terminals,
            blobs,
            tokens,
            fs,
            forge,
            skills,
            secure,
            remote,
            devices,
            mcp,
            relay_dialer: Arc::new(crate::remote::RelayDialer::new()),
            supervisor: self.supervisor,
            proxy,
            meter,
        })
    }
}

/// A built daemon that has not bound sockets yet.
pub struct Daemon {
    config: Config,
    manager: Arc<ThreadManager>,
    subagents: Arc<SubagentManager>,
    hosts: Arc<HostInventory>,
    terminals: Arc<TerminalManager>,
    blobs: Arc<BlobStore>,
    tokens: TokenStore,
    fs: Arc<FsService>,
    forge: Arc<ForgeService>,
    skills: Arc<h4rn_skills::SkillsService>,
    secure: Option<Arc<SecureService>>,
    remote: Option<Arc<RemoteState>>,
    devices: Arc<DeviceRegistry>,
    mcp: Arc<McpService>,
    /// Multiplexes peer MCP sessions over the daemon's one relay socket.
    relay_dialer: Arc<crate::remote::RelayDialer>,
    supervisor: Arc<ProcessSupervisor>,
    proxy: Arc<h4rn_proxy::ProxyService>,
    meter: Arc<h4rn_meter::MeterHub>,
}

impl Daemon {
    /// Bound configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The secure transport service, when enabled.
    pub fn secure(&self) -> Option<Arc<SecureService>> {
        self.secure.clone()
    }

    /// Thread manager handle (tests and diagnostics).
    pub fn manager(&self) -> Arc<ThreadManager> {
        Arc::clone(&self.manager)
    }

    /// Subagent supervisor handle (tests and diagnostics).
    pub fn subagents(&self) -> Arc<SubagentManager> {
        Arc::clone(&self.subagents)
    }

    /// Terminal manager handle (tests and diagnostics).
    pub fn terminals(&self) -> Arc<TerminalManager> {
        Arc::clone(&self.terminals)
    }

    /// CLIProxyAPI profile service (tests and diagnostics).
    pub fn proxy(&self) -> Arc<h4rn_proxy::ProxyService> {
        Arc::clone(&self.proxy)
    }

    /// Skill registry handle (tests and diagnostics).
    pub fn skills(&self) -> Arc<h4rn_skills::SkillsService> {
        Arc::clone(&self.skills)
    }

    /// MCP hub service handle (tests and diagnostics).
    pub fn mcp(&self) -> Arc<McpService> {
        Arc::clone(&self.mcp)
    }

    /// Durable metering hub (tests and diagnostics).
    pub fn meter(&self) -> Arc<h4rn_meter::MeterHub> {
        Arc::clone(&self.meter)
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

        // The secure transport is the only remote path worth exposing on a
        // tailnet, so the extra listener shares the primary port. A bind
        // failure is not fatal: the relay and any LAN address still work.
        if self.config.tailscale == TailscaleMode::Auto
            && let Some(ip) = crate::remote::tailscale::tailnet_ip()
        {
            let addr = SocketAddr::new(ip, addrs[0].port());
            if !addrs.contains(&addr) {
                match TcpListener::bind(addr).await {
                    Ok(listener) => {
                        addrs.push(listener.local_addr()?);
                        listeners.push(listener);
                        tracing::info!(%addr, "listening on the tailnet address");
                    }
                    Err(error) => tracing::warn!(
                        %addr,
                        %error,
                        "could not bind the tailnet address; relay and LAN remain available"
                    ),
                }
            }
        }

        // The hub endpoint is always loopback: harness children run on this
        // machine and must not expose the token over a LAN interface.
        let mcp_url = format!(
            "http://{}/mcp",
            SocketAddr::from(([127, 0, 0, 1], addrs[0].port()))
        );
        self.mcp.set_endpoint(mcp_url).await;
        // With a secure transport this daemon can reach its peers' shared MCP
        // servers, authenticating as its own daemon key.
        if let Some(service) = &self.secure {
            self.mcp.start_peering(
                service.options().identity.clone(),
                Some(Arc::clone(&self.relay_dialer)),
            );
        }

        let hosts = Arc::clone(&self.hosts);
        let remote = self.remote;
        let devices = self.devices;
        let secure = self.secure;
        let mcp = Arc::clone(&self.mcp);
        let config = self.config.clone();
        let state = AppState {
            manager: Arc::clone(&self.manager),
            subagents: Arc::clone(&self.subagents),
            hosts,
            terminals: Arc::clone(&self.terminals),
            blobs: Arc::clone(&self.blobs),
            tokens: self.tokens,
            fs: self.fs,
            forge: Arc::clone(&self.forge),
            skills: Arc::clone(&self.skills),
            secure: secure.clone(),
            remote: remote.as_ref().map(|remote| Arc::clone(&remote.audit)),
            devices: Arc::clone(&devices),
            mcp: Arc::clone(&mcp),
            started: std::time::Instant::now(),
            proxy: Arc::clone(&self.proxy),
            meter: Arc::clone(&self.meter),
        };

        let mut remote_tasks = Vec::new();
        remote_tasks.push(tokio::spawn(proxy_usage_ingest_loop(
            Arc::clone(&self.proxy),
            Arc::clone(&self.meter),
            self.hosts.local_id().as_str().to_owned(),
        )));
        if let Some(remote) = &remote {
            remote_tasks.push(tokio::spawn(usage_sync_loop(
                Arc::clone(&self.meter),
                Arc::clone(&remote.client),
            )));
        }
        if let Some(remote) = &remote
            && let Some(service) = &secure
        {
            remote_tasks.push(tokio::spawn(crate::remote::peers::run_peer_sync(
                Arc::clone(&remote.client),
                mcp.peers(),
                Arc::clone(&remote.devices),
            )));
            remote_tasks.push(tokio::spawn(crate::remote::run_relay(
                state.clone(),
                Arc::clone(service),
                Arc::clone(&remote.client),
                Arc::clone(&remote.devices),
                Arc::clone(&self.relay_dialer),
            )));
            remote_tasks.push(tokio::spawn(publish_presence(
                state.clone(),
                Arc::clone(&remote.client),
                config.clone(),
            )));
        }

        if let Some(service) = &secure {
            remote_tasks.push(tokio::spawn(watch_direct_pairing(
                Arc::clone(service),
                Arc::clone(&devices),
                config.data_dir.clone(),
            )));
        }

        Ok(BoundDaemon {
            app: server::router(state),
            listeners,
            addrs,
            manager: self.manager,
            subagents: self.subagents,
            terminals: self.terminals,
            forge: self.forge,
            mcp: self.mcp,
            supervisor: self.supervisor,
            data_dir: self.config.data_dir,
            remote_tasks,
        })
    }
}

/// A daemon with sockets bound, ready for [`BoundDaemon::serve`].
pub struct BoundDaemon {
    app: axum::Router,
    listeners: Vec<TcpListener>,
    addrs: Vec<SocketAddr>,
    manager: Arc<ThreadManager>,
    subagents: Arc<SubagentManager>,
    terminals: Arc<TerminalManager>,
    forge: Arc<ForgeService>,
    mcp: Arc<McpService>,
    supervisor: Arc<ProcessSupervisor>,
    data_dir: PathBuf,
    remote_tasks: Vec<tokio::task::JoinHandle<()>>,
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

    /// Subagent supervisor for in-process tests and diagnostics.
    pub fn subagents(&self) -> Arc<SubagentManager> {
        Arc::clone(&self.subagents)
    }

    /// MCP hub service for in-process tests and diagnostics.
    pub fn mcp(&self) -> Arc<McpService> {
        Arc::clone(&self.mcp)
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
            subagents,
            terminals,
            forge,
            mcp,
            supervisor,
            data_dir,
            remote_tasks,
        } = self;

        tracing::info!(
            addresses = ?addrs,
            data_dir = %data_dir.display(),
            "h4rn daemon listening"
        );

        server::serve_all(listeners, app, shutdown).await?;
        // Stop the relay connector and pairing watcher before tearing down the
        // thread manager so no new session starts during shutdown.
        for task in remote_tasks {
            task.abort();
        }
        forge.shutdown();
        mcp.shutdown().await;
        subagents.shutdown().await;
        manager.shutdown().await;
        terminals.shutdown().await;
        supervisor.shutdown().await;
        tracing::info!("h4rn daemon stopped");
        Ok(())
    }
}

/// Builds the secure service when remote access needs it.
///
/// It is enabled by `H4RN_SECURE=1`, by a signed-in connect session and by
/// `H4RN_PAIRING_PSK`. The identity lives in the data directory as a `0600`
/// file; the same identity is the connect device proof and the pairing key.
fn secure_from_env(
    identity: DeviceIdentity,
    remote: bool,
) -> anyhow::Result<Option<Arc<SecureService>>> {
    let forced = std::env::var("H4RN_SECURE").as_deref() == Ok("1");
    let env_psk = std::env::var("H4RN_PAIRING_PSK").ok();
    if !forced && !remote && env_psk.is_none() {
        return Ok(None);
    }
    let mut options = SecureOptions::new(identity);
    if let Some(value) = env_psk {
        let value = value.trim();
        let bytes = hex::decode(value)
            .map_err(|error| anyhow::anyhow!("H4RN_PAIRING_PSK is not hex: {error}"))?;
        if bytes.len() != KEY_LEN {
            anyhow::bail!("H4RN_PAIRING_PSK must be {KEY_LEN} bytes of hex");
        }
        let mut psk = [0u8; KEY_LEN];
        psk.copy_from_slice(&bytes);
        options = options.with_psk(psk);
    }
    Ok(Some(Arc::new(SecureService::new(options))))
}

/// Periodically ingests the fork's per-call performance records.
///
/// Rows get a stable proxy-derived id, so polling the same window twice is
/// idempotent. Dedupe against h4rn's own observation lives in the ledger, not
/// here: this loop never decides to skip a record on its own.
async fn proxy_usage_ingest_loop(
    proxy: Arc<h4rn_proxy::ProxyService>,
    meter: Arc<h4rn_meter::MeterHub>,
    machine: String,
) {
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let until = chrono::Utc::now();
        let since = until - chrono::Duration::hours(1);
        for profile in proxy.profiles() {
            let supports = profile
                .capabilities
                .as_ref()
                .is_some_and(|capabilities| capabilities.performance);
            if !supports {
                continue;
            }
            match proxy.performance(&profile.id, since, until, 500).await {
                Ok(calls) => {
                    for call in calls {
                        let model = call.model.clone().unwrap_or_default();
                        let estimate = meter.prices().estimate(
                            &model,
                            call.output_tokens.saturating_mul(3),
                            call.output_tokens,
                            0,
                            0,
                        );
                        let row = h4rn_meter::UsageRow {
                            row_id: format!("proxy_{}_{}", profile.id.as_str(), call.stable_id()),
                            thread_id: None,
                            parent_thread_id: None,
                            run_id: None,
                            workspace: None,
                            machine: Some(machine.clone()),
                            account: None,
                            harness: None,
                            model: call.model.clone(),
                            provider: provider_label(call.provider.as_deref(), &model),
                            credential: None,
                            source: h4rn_protocol::UsageProvenance::ProxyReported,
                            correlation_id: Some(call.stable_id()),
                            tokens_in: 0,
                            tokens_out: call.output_tokens,
                            tokens_cached_read: 0,
                            tokens_cached_creation: 0,
                            cost_micros: estimate.cost_micros,
                            cost_source: estimate.source,
                            price_table: estimate.price_table,
                            ttft_ms: call.ttft_ms,
                            tps: call.tps,
                            generation_ms: call.duration_ms.unwrap_or(0),
                            wall_ms: call.duration_ms.unwrap_or(0),
                            started_at: call.requested_at,
                            completed_at: call.completed_at(),
                            ts: call.completed_at(),
                            raw: serde_json::to_value(&call).ok(),
                            deduped: false,
                            synced: false,
                        };
                        if let Err(error) = meter.record_proxy_row(row).await {
                            tracing::warn!(%error, "could not record a proxy performance row");
                        }
                    }
                }
                Err(error) => {
                    tracing::debug!(
                        profile = %profile.id,
                        %error,
                        "proxy performance ingest skipped"
                    );
                }
            }
        }
    }
}

/// A normalised provider label, preferring the model family so a proxy row and
/// a self-metered row agree even when one side reports a vendor alias.
fn provider_label(proxy_provider: Option<&str>, model: &str) -> Option<String> {
    let model = model.to_ascii_lowercase();
    if model.contains("claude") || model.contains("sonnet") || model.contains("opus") {
        Some("anthropic".into())
    } else if model.starts_with("gpt") || model.contains("o3") || model.contains("o4") {
        Some("openai".into())
    } else if model.contains("gemini") {
        Some("google".into())
    } else if model.contains("grok") {
        Some("xai".into())
    } else {
        proxy_provider.map(str::to_owned)
    }
}

/// Pushes unsynced ledger rows to h4rn-connect in idempotent batches.
async fn usage_sync_loop(meter: Arc<h4rn_meter::MeterHub>, client: Arc<ConnectClient>) {
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        if !client.is_logged_in() {
            continue;
        }
        let rows = match meter.unsynced(200).await {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, "could not read unsynced usage rows");
                continue;
            }
        };
        if rows.is_empty() {
            continue;
        }
        let request = h4rn_protocol::UsageSyncRequest {
            rows: rows.iter().map(|row| row.to_sync_row()).collect(),
        };
        match client.post_usage(&request).await {
            Ok(response) => {
                let ids: Vec<String> = rows.iter().map(|row| row.row_id.clone()).collect();
                if let Err(error) = meter.mark_synced(&ids).await {
                    tracing::warn!(%error, "could not mark synced usage rows");
                }
                tracing::debug!(
                    accepted = response.accepted,
                    duplicates = response.duplicates,
                    "synced the usage ledger"
                );
            }
            Err(error) => tracing::warn!(%error, "usage sync failed; the rows stay queued"),
        }
    }
}

/// Reports hosts and address candidates, then keeps presence fresh.
///
/// The first tick fires immediately so a freshly started daemon appears online
/// as soon as the relay connects. A failure is only logged: connect may be
/// restarting, and the next tick retries.
async fn publish_presence(state: AppState, client: Arc<ConnectClient>, config: Config) {
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let hosts = machine_hosts(&state.hosts);
        if let Err(error) = client.update_hosts(hosts).await {
            tracing::debug!(%error, "could not publish hosts to connect");
        }
        let relay = client
            .credentials()
            .map(|credentials| credentials.connect_url);
        let addresses = advertised_addresses(&config, relay.as_deref());
        if let Err(error) = client.update_addresses(addresses).await {
            tracing::debug!(%error, "could not publish addresses to connect");
        }
    }
}

/// Converts the daemon's host inventory into the connect report.
pub(crate) fn machine_hosts(hosts: &HostInventory) -> Vec<MachineHostInput> {
    hosts
        .summaries()
        .into_iter()
        .map(|summary| {
            let distro = summary.platform.strip_prefix("wsl:").map(str::to_owned);
            MachineHostInput {
                name: summary.name,
                kind: if distro.is_some() {
                    MachineHostKind::Wsl
                } else {
                    MachineHostKind::Host
                },
                distro,
            }
        })
        .collect()
}

/// Watches `<data_dir>/direct-pairing.json` and arms the one-time PSK.
///
/// The CLI writes the file; the daemon applies it within a couple of seconds.
/// The transport consumes the secret on the first successful pairing and calls
/// the consumed hook, which deletes the file, so restart can never re-arm a
/// used QR. An expired file is deleted too. The watcher only ever clears a
/// secret it installed itself: an operator's `H4RN_PAIRING_PSK` stays armed,
/// and a direct file that would replace it is ignored with a warning.
async fn watch_direct_pairing(
    service: Arc<SecureService>,
    devices: Arc<DeviceRegistry>,
    data_dir: PathBuf,
) {
    // The secret this watcher installed, hex, and the file it came from.
    let mut applied: Option<String> = None;
    // A live file that was not armed because another secret already was.
    let mut blocked: Option<String> = None;
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(2));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        match read_direct_pairing(&data_dir) {
            Ok(Some(pairing)) if !pairing.expired() => {
                if applied.as_deref() == Some(pairing.psk.as_str())
                    || blocked.as_deref() == Some(pairing.psk.as_str())
                {
                    continue;
                }
                blocked = None;
                match pairing.psk() {
                    Ok(psk) => match service.psk() {
                        Some(current) if current != psk => {
                            // Never replace a secret someone else armed. An
                            // operator PSK or a concurrent pairing keeps
                            // precedence; the file stays for operator review
                            // until it expires.
                            tracing::warn!(
                                label = ?pairing.label,
                                "a direct pairing is waiting but another pairing secret is already armed; leaving it untouched"
                            );
                            blocked = Some(pairing.psk.clone());
                        }
                        _ => {
                            // Arm the scopes first: the device that pairs with
                            // this secret is recorded with exactly these.
                            devices.arm_direct(pairing.scopes.clone());
                            service.set_psk(psk);
                            applied = Some(pairing.psk.clone());
                            tracing::info!(
                                label = ?pairing.label,
                                expires_at = pairing.expires_at,
                                "armed a direct pairing; scan the QR now"
                            );
                        }
                    },
                    Err(error) => {
                        tracing::warn!(%error, "ignoring a malformed direct pairing file")
                    }
                }
            }
            Ok(_) => {
                if let Some(hex) = applied.take() {
                    // Clear only the secret this watcher installed. Comparing
                    // by value keeps an operator rotation intact.
                    let ours = hex::decode(&hex)
                        .ok()
                        .and_then(|bytes| <[u8; TOKEN_LEN]>::try_from(bytes.as_slice()).ok());
                    if ours.is_some() && service.psk() == ours {
                        service.clear_psk();
                    }
                    devices.clear_direct();
                    tracing::info!("direct pairing expired, was used or was closed");
                }
                blocked = None;
                // A consumed or expired secret must not survive a restart.
                if let Ok(Some(pairing)) = read_direct_pairing(&data_dir)
                    && pairing.expired()
                    && let Err(error) = remove_direct_pairing(&data_dir)
                {
                    tracing::debug!(%error, "could not delete the expired direct pairing");
                }
            }
            Err(error) => tracing::debug!(%error, "could not read the direct pairing file"),
        }
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
