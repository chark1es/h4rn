//! Forge accounts, credentials and pull-request operations for the daemon.
//!
//! One [`h4rn_forge::Forge`] client is built per configured account and kept
//! for the lifetime of the daemon. Tokens never enter the wire contract: this
//! module stores them in the OS keychain when one is available and falls back
//! to the transport crate's encrypted `0600` secret file otherwise (ADR-016).
//!
//! The service also owns the small amount of state the protocol cannot express:
//! the set of PRs the UI is watching, their last checks signature, and a
//! background poller that emits [`h4rn_protocol::Event::ForgeChecksUpdated`]
//! with exponential backoff. Writes are appended to `forge-audit.jsonl` so a
//! user can see what the daemon did on their behalf.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use chrono::Utc;
use h4rn_forge::{
    Forge, ForgeError, ForgeHosts, ForgeViewer, PrPage, auth, client_for_with_budget, detect_repos,
    pr_context_for_agent, remote::validate_base_url, remote::validate_base_url_for,
    remote::validate_cloud_base_url,
};
use h4rn_protocol::{
    ForgeAccount, ForgeAgentTarget, ForgeCapabilityEntry, ForgeChecks, ForgeComment,
    ForgeCommentPage, ForgeCredentialSource, ForgeDeviceFlow, ForgeDeviceFlowStatus, ForgeDiffPage,
    ForgeHandoff, ForgeKind, ForgeNewPr, ForgePrFilter, ForgePullRequest, ForgeRemote,
    ForgeRepoRef, ForgeReviewComment, ProtoError, ProtoErrorCode, RuntimeMode, ThreadId,
};
use serde_json::json;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::config::create_private_dir;
use crate::threads::{ManagerError, NewThread, ThreadManager};

/// File holding public account metadata (never a token).
pub const ACCOUNTS_FILE: &str = "forge-accounts.json";
/// File holding custom host mappings (`forge-hosts.json`).
pub const HOSTS_FILE: &str = "forge-hosts.json";
/// Append-only local audit trail for forge writes.
pub const AUDIT_FILE: &str = "forge-audit.jsonl";
/// OS keychain service name for forge tokens.
const KEYCHAIN_SERVICE: &str = "h4rn-forge";
/// Default watched-PR poll interval.
const DEFAULT_POLL_SECS: u64 = 30;
/// Maximum poll backoff after repeated failures.
const MAX_BACKOFF_SECS: u64 = 300;

/// One checks snapshot broadcast to connected clients.
#[derive(Clone, Debug)]
pub struct ForgeChecksUpdate {
    /// Repository.
    pub repo: ForgeRepoRef,
    /// PR number.
    pub number: u32,
    /// Latest snapshot.
    pub checks: ForgeChecks,
}

/// A forge operation failure at the daemon boundary.
#[derive(Debug, thiserror::Error)]
pub enum ForgeServiceError {
    /// The forge layer failed.
    #[error(transparent)]
    Forge(#[from] ForgeError),
    /// The thread manager failed.
    #[error(transparent)]
    Threads(#[from] ManagerError),
    /// Local storage failed.
    #[error("{0}")]
    Local(String),
    /// An I/O error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Account JSON failed to parse or serialise.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl ForgeServiceError {
    /// Maps the failure onto the protocol error class and a user hint.
    pub fn into_proto(self) -> ProtoError {
        match self {
            Self::Threads(error) => error.into_proto(),
            Self::Forge(error) => forge_proto_error(error),
            other => ProtoError::new(
                ProtoErrorCode::Internal,
                other.to_string(),
                "Check the daemon log for details",
            ),
        }
    }
}

fn forge_proto_error(error: ForgeError) -> ProtoError {
    let message = error.to_string();
    let (code, hint) = match &error {
        ForgeError::NotConfigured(_) => (
            ProtoErrorCode::NotFound,
            "Add the account in Settings → Forges, or set forge-hosts.json for a custom host",
        ),
        ForgeError::Unauthorized { .. } => (
            ProtoErrorCode::Unauthorized,
            "Replace the token for this account; it was rejected or has expired",
        ),
        ForgeError::Forbidden { .. } => (
            ProtoErrorCode::Forbidden,
            "The token lacks a required scope; see docs/forges.md for the minimal scopes",
        ),
        ForgeError::NotFound(_) => (
            ProtoErrorCode::NotFound,
            "Check the repository, PR number and that the token can see it",
        ),
        ForgeError::Conflict(_) => (
            ProtoErrorCode::Conflict,
            "The forge changed state; refresh and try again",
        ),
        ForgeError::InvalidRequest(_) => (
            ProtoErrorCode::InvalidRequest,
            "Fix the request; the message names the field the forge rejected",
        ),
        ForgeError::RateLimited { .. } => (
            ProtoErrorCode::RateLimited,
            "Wait for the reset, or add a second account with its own quota",
        ),
        ForgeError::Unsupported(_) => (
            ProtoErrorCode::Unsupported,
            "This forge cannot do that; use a general comment instead",
        ),
        ForgeError::Network(_) | ForgeError::Parse(_) => (
            ProtoErrorCode::Internal,
            "Check the daemon's network access and retry",
        ),
        ForgeError::Http { status, .. } if *status >= 500 => (
            ProtoErrorCode::Internal,
            "The forge is failing; retry later",
        ),
        ForgeError::Http { .. } => (
            ProtoErrorCode::InvalidRequest,
            "The forge rejected the request; the message has its answer",
        ),
        ForgeError::Local(_) => (
            ProtoErrorCode::Internal,
            "Check the workspace and the daemon log",
        ),
    };
    ProtoError::new(code, message, hint)
}

/// Account token storage: OS keychain first, encrypted file otherwise.
struct TokenStore {
    data_dir: PathBuf,
    keychain: bool,
}

impl TokenStore {
    fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            keychain: keyring::Entry::store_status().is_ok(),
        }
    }

    fn path(&self, account_id: &str) -> PathBuf {
        self.data_dir.join(format!("forge-token-{account_id}"))
    }

    fn save(&self, account_id: &str, token: &str) -> Result<(), ForgeServiceError> {
        if self.keychain {
            if let Ok(entry) = keyring::Entry::new(KEYCHAIN_SERVICE, account_id) {
                if entry.set_password(token).is_ok() {
                    return Ok(());
                }
            }
        }
        h4rn_transport::keys::write_secret_file(&self.path(account_id), token.as_bytes())
            .map_err(|error| ForgeServiceError::Local(format!("store forge token: {error}")))
    }

    fn load(&self, account_id: &str) -> Result<Option<String>, ForgeServiceError> {
        if self.keychain {
            if let Ok(entry) = keyring::Entry::new(KEYCHAIN_SERVICE, account_id) {
                if let Ok(token) = entry.get_password() {
                    if !token.is_empty() {
                        return Ok(Some(token));
                    }
                }
            }
        }
        let bytes = h4rn_transport::keys::read_secret_file(&self.path(account_id))
            .map_err(|error| ForgeServiceError::Local(format!("read forge token: {error}")))?;
        match bytes {
            Some(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).trim().to_owned())),
            None => Ok(None),
        }
    }

    fn remove(&self, account_id: &str) {
        if self.keychain
            && let Ok(entry) = keyring::Entry::new(KEYCHAIN_SERVICE, account_id)
        {
            let _ = entry.delete_credential();
        }
        if let Err(error) = h4rn_transport::keys::remove_secret_file(&self.path(account_id)) {
            tracing::warn!(%error, "could not remove a forge token file");
        }
    }
}

/// Local, append-only audit trail for forge writes.
struct ForgeAudit {
    path: PathBuf,
    lock: Mutex<()>,
}

impl ForgeAudit {
    fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join(AUDIT_FILE),
            lock: Mutex::new(()),
        }
    }

    fn record(
        &self,
        action: &str,
        account_id: &str,
        repo: &ForgeRepoRef,
        number: u32,
        metadata: serde_json::Value,
    ) {
        let entry = json!({
            "at": Utc::now().to_rfc3339(),
            "action": action,
            "account": account_id,
            "repo": repo.full_name(),
            "forge": repo.forge.slug(),
            "base_url": repo.base_url,
            "number": number,
            "metadata": metadata,
        });
        let Ok(_guard) = self.lock.lock() else { return };
        let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        else {
            return;
        };
        use std::io::Write as _;
        let _ = writeln!(file, "{entry}");
    }
}

/// One watched PR's last observed checks.
struct WatchState {
    signature: Option<String>,
}

/// A device authorization the daemon is holding between polls.
struct DeviceFlowState {
    device_code: String,
    base_url: String,
    expires_at: chrono::DateTime<Utc>,
    interval: u64,
}

/// The daemon's forge service.
pub struct ForgeService {
    accounts_path: PathBuf,
    hosts: ForgeHosts,
    tokens: TokenStore,
    accounts: RwLock<Vec<ForgeAccount>>,
    clients: RwLock<HashMap<String, Arc<dyn Forge>>>,
    audit: ForgeAudit,
    updates: broadcast::Sender<ForgeChecksUpdate>,
    watches: RwLock<HashMap<(String, ForgeRepoRef, u32), WatchState>>,
    poller: Mutex<Option<JoinHandle<()>>>,
    /// Device authorizations in flight, keyed by the id handed to the client.
    flows: Mutex<HashMap<String, DeviceFlowState>>,
    requests_per_minute: u32,
}

impl ForgeService {
    /// Opens the service, reading accounts and host mappings from `data_dir`.
    pub fn open(data_dir: &Path) -> Result<Arc<Self>, ForgeServiceError> {
        create_private_dir(data_dir)
            .map_err(|error| ForgeServiceError::Local(format!("prepare data dir: {error}")))?;
        let hosts = ForgeHosts::load(&data_dir.join(HOSTS_FILE))?;
        let accounts = load_accounts(&data_dir.join(ACCOUNTS_FILE))?;
        let (updates, _) = broadcast::channel(64);
        let requests_per_minute = std::env::var("H4RN_FORGE_RPM")
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(h4rn_forge::DEFAULT_REQUESTS_PER_MINUTE);
        let service = Arc::new(Self {
            accounts_path: data_dir.join(ACCOUNTS_FILE),
            hosts,
            tokens: TokenStore::new(data_dir),
            accounts: RwLock::new(accounts),
            clients: RwLock::new(HashMap::new()),
            audit: ForgeAudit::new(data_dir),
            updates,
            watches: RwLock::new(HashMap::new()),
            poller: Mutex::new(None),
            flows: Mutex::new(HashMap::new()),
            requests_per_minute,
        });
        service.start_poller();
        Ok(service)
    }

    /// Subscribes to watched-PR checks updates.
    pub fn subscribe(&self) -> broadcast::Receiver<ForgeChecksUpdate> {
        self.updates.subscribe()
    }

    /// Stops the background poller.
    pub fn shutdown(&self) {
        if let Ok(mut poller) = self.poller.lock()
            && let Some(handle) = poller.take()
        {
            handle.abort();
        }
    }

    /// Configured accounts plus one capability row per forge.
    pub fn list_accounts(&self) -> (Vec<ForgeAccount>, Vec<ForgeCapabilityEntry>) {
        let accounts = self
            .accounts
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        (accounts, h4rn_forge::capability_entries())
    }

    /// Validates a token with the forge, then stores the account.
    pub async fn add_account(
        &self,
        forge: ForgeKind,
        base_url: &str,
        token: &str,
        username: Option<String>,
        source: ForgeCredentialSource,
        repository: Option<ForgeRepoRef>,
    ) -> Result<ForgeAccount, ForgeServiceError> {
        let token = token.trim();
        if token.is_empty() {
            return Err(ForgeError::InvalidRequest("the token is empty".into()).into());
        }
        let base_url = validate_cloud_base_url(forge, base_url)
            .and_then(|()| validate_base_url_for(forge, base_url))?;
        let account_id = format!("acct_{}", uuid::Uuid::now_v7().simple());
        let username = username
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        // Bitbucket's auth scheme is decided once, here, and recorded so a
        // restart does not turn a Bearer token into a Basic credential.
        let (source, username) = bitbucket_auth_source(forge, source, username);
        let account = ForgeAccount {
            account_id: account_id.clone(),
            forge,
            base_url,
            username,
            display_name: None,
            scopes: Vec::new(),
            source,
            created_at: Utc::now(),
        };

        // Validate before storing anything so a typo never leaves a dead
        // credential behind.
        let client = client_for_with_budget(&account, token, self.requests_per_minute)?;
        self.finish_add_account(account, token, client, repository.as_ref())
            .await
    }

    /// Validates and stores an account whose client is already built.
    ///
    /// Split from [`Self::add_account`] so a test can point a real client at a
    /// local server and exercise the repository hint without a cloud URL.
    async fn finish_add_account(
        &self,
        mut account: ForgeAccount,
        token: &str,
        client: Arc<dyn Forge>,
        repository: Option<&ForgeRepoRef>,
    ) -> Result<ForgeAccount, ForgeServiceError> {
        // A repository, project or workspace access token for Bitbucket Cloud
        // cannot read `/user` or `/user/workspaces`, so the daemon proves it
        // against the repository the form detected instead.
        let viewer: ForgeViewer = client.viewer_for(repository).await?;
        // Bitbucket keeps the username the user supplied for an API token
        // (Basic needs the Atlassian account email). An access token must not
        // carry one: a username would make the client send Basic instead of
        // Bearer.
        let keep_bitbucket_username = account.forge == ForgeKind::Bitbucket
            && matches!(
                account.source,
                ForgeCredentialSource::BitbucketApiToken
                    | ForgeCredentialSource::BitbucketAccessToken
            );
        if !keep_bitbucket_username {
            account.username = Some(viewer.login.clone());
        }
        account.display_name = viewer.display_name;
        account.scopes = viewer.scopes;

        self.tokens.save(&account.account_id, token)?;
        if let Err(error) = self.insert_account(account.clone()) {
            self.tokens.remove(&account.account_id);
            return Err(error);
        }
        self.clients
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(account.account_id.clone(), client);
        self.audit.record(
            "account.added",
            &account.account_id,
            &ForgeRepoRef {
                forge: account.forge,
                base_url: account.base_url.clone(),
                owner: String::new(),
                repo: String::new(),
            },
            0,
            json!({"source": account.source.label(), "username": account.username}),
        );
        Ok(account)
    }

    /// Starts a GitHub device authorization.
    ///
    /// The device code stays in the daemon; the client only receives the flow
    /// id, the short code and the verification URL.
    pub async fn start_device_flow(
        &self,
        base_url: Option<String>,
    ) -> Result<ForgeDeviceFlow, ForgeServiceError> {
        let client_id = auth::github_client_id().ok_or_else(|| {
            ForgeError::NotConfigured(format!(
                "set {} before starting the daemon to use GitHub sign-in",
                auth::GITHUB_CLIENT_ID_ENV
            ))
        })?;
        let base_url = validate_base_url(base_url.as_deref().unwrap_or("https://github.com"))?;
        if !base_url.eq_ignore_ascii_case("https://github.com") {
            return Err(ForgeError::Unsupported(format!(
                "the device flow is only available on github.com, not {base_url}"
            ))
            .into());
        }
        let start = auth::start_github_device_flow(&client_id, auth::GITHUB_DEVICE_SCOPES).await?;
        let flow_id = format!("flow_{}", uuid::Uuid::now_v7().simple());
        let expires_at = Utc::now() + chrono::Duration::seconds(start.expires_in as i64);
        self.flows
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                flow_id.clone(),
                DeviceFlowState {
                    device_code: start.device_code,
                    base_url,
                    expires_at,
                    interval: start.interval,
                },
            );
        Ok(ForgeDeviceFlow {
            flow_id,
            user_code: start.user_code,
            verification_uri: start.verification_uri,
            expires_in: start.expires_in,
            interval: start.interval,
        })
    }

    /// Polls one device authorization and stores the account when approved.
    pub async fn poll_device_flow(
        &self,
        flow_id: &str,
    ) -> Result<ForgeDeviceFlowStatus, ForgeServiceError> {
        let client_id = auth::github_client_id().ok_or_else(|| {
            ForgeError::NotConfigured(format!(
                "set {} before starting the daemon to use GitHub sign-in",
                auth::GITHUB_CLIENT_ID_ENV
            ))
        })?;
        let (device_code, base_url, expires_at, interval) = {
            let flows = self
                .flows
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let flow = flows.get(flow_id).ok_or_else(|| {
                ForgeError::NotFound("this device authorization is no longer active".into())
            })?;
            (
                flow.device_code.clone(),
                flow.base_url.clone(),
                flow.expires_at,
                flow.interval,
            )
        };
        if Utc::now() >= expires_at {
            self.forget_flow(flow_id);
            return Ok(ForgeDeviceFlowStatus::Expired);
        }
        let outcome = auth::poll_github_device_once(&client_id, &device_code).await?;
        match outcome {
            auth::DevicePoll::Pending { .. } => Ok(ForgeDeviceFlowStatus::Pending { interval }),
            auth::DevicePoll::SlowDown { interval } => {
                if let Some(flow) = self
                    .flows
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .get_mut(flow_id)
                {
                    flow.interval = interval;
                }
                Ok(ForgeDeviceFlowStatus::SlowDown { interval })
            }
            auth::DevicePoll::Authorized(token) => {
                let account = self
                    .add_account(
                        ForgeKind::GitHub,
                        &base_url,
                        &token.access_token,
                        None,
                        ForgeCredentialSource::OAuthDevice,
                        None,
                    )
                    .await?;
                self.forget_flow(flow_id);
                Ok(ForgeDeviceFlowStatus::Complete { account })
            }
            auth::DevicePoll::Expired => {
                self.forget_flow(flow_id);
                Ok(ForgeDeviceFlowStatus::Expired)
            }
            auth::DevicePoll::Denied { message } => {
                self.forget_flow(flow_id);
                Ok(ForgeDeviceFlowStatus::Denied { message })
            }
        }
    }

    /// Imports a token the user already stored with `gh` or `glab`.
    pub async fn import_cli_account(
        &self,
        forge: ForgeKind,
        base_url: Option<String>,
    ) -> Result<ForgeAccount, ForgeServiceError> {
        let (token, source, base_url) = match forge {
            ForgeKind::GitHub => {
                let base_url =
                    validate_base_url(base_url.as_deref().unwrap_or("https://github.com"))?;
                let hostname = (base_url != "https://github.com").then(|| base_url.clone());
                (
                    auth::import_gh_token(hostname.as_deref())?,
                    ForgeCredentialSource::GhCli,
                    base_url,
                )
            }
            ForgeKind::GitLab => {
                let base_url =
                    validate_base_url(base_url.as_deref().unwrap_or("https://gitlab.com"))?;
                let hostname = (base_url != "https://gitlab.com").then(|| base_url.clone());
                (
                    auth::import_glab_token(hostname.as_deref())?,
                    ForgeCredentialSource::GlabCli,
                    base_url,
                )
            }
            other => {
                return Err(ForgeError::Unsupported(format!(
                    "{} has no CLI to import from; add a personal access token",
                    other.slug()
                ))
                .into());
            }
        };
        self.add_account(forge, &base_url, &token, None, source, None)
            .await
    }

    fn forget_flow(&self, flow_id: &str) {
        self.flows
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(flow_id);
    }

    /// Removes an account and its token.
    pub fn remove_account(&self, account_id: &str) -> Result<(), ForgeServiceError> {
        let removed = {
            let mut accounts = self
                .accounts
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let position = accounts
                .iter()
                .position(|account| account.account_id == account_id)
                .ok_or_else(|| ForgeError::NotFound(format!("no forge account {account_id}")))?;
            let removed = accounts.remove(position);
            save_accounts(&self.accounts_path, &accounts)?;
            removed
        };
        self.clients
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(account_id);
        self.tokens.remove(account_id);
        self.audit.record(
            "account.removed",
            account_id,
            &ForgeRepoRef {
                forge: removed.forge,
                base_url: removed.base_url,
                owner: String::new(),
                repo: String::new(),
            },
            0,
            json!({}),
        );
        Ok(())
    }

    /// Detects forge remotes in a workspace.
    pub fn detect_repo(&self, workspace: &str) -> Result<Vec<ForgeRemote>, ForgeServiceError> {
        Ok(detect_repos(Path::new(workspace), &self.hosts)?)
    }

    /// Lists PRs for one repository.
    pub async fn list_prs(
        &self,
        account_id: Option<&str>,
        repo: &ForgeRepoRef,
        filter: &ForgePrFilter,
        page: u32,
    ) -> Result<PrPage, ForgeServiceError> {
        let (_account, client) = self.resolve_client(account_id, repo)?;
        Ok(client.list_prs(repo, filter, page.max(1)).await?)
    }

    /// Fetches one PR and starts watching its checks.
    pub async fn get_pr(
        &self,
        account_id: Option<&str>,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<ForgePullRequest, ForgeServiceError> {
        self.get_pr_inner(account_id, repo, number, true).await
    }

    /// Fetches one PR without enrolling it in the checks poller.
    ///
    /// The PR watch reads every two minutes on its own and must not leave the
    /// PR in the 30-second checks poll set. A client surface (the PR page)
    /// still uses [`Self::get_pr`], so its registration is untouched.
    pub async fn get_pr_quiet(
        &self,
        account_id: Option<&str>,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<ForgePullRequest, ForgeServiceError> {
        self.get_pr_inner(account_id, repo, number, false).await
    }

    async fn get_pr_inner(
        &self,
        account_id: Option<&str>,
        repo: &ForgeRepoRef,
        number: u32,
        register: bool,
    ) -> Result<ForgePullRequest, ForgeServiceError> {
        let (account, client) = self.resolve_client(account_id, repo)?;
        let pr = client.get_pr(repo, number).await?;
        if register {
            self.watch(&account, repo, number);
        }
        Ok(pr)
    }

    /// The account id a repository resolves to, for callers that store it.
    pub fn resolve_account_id(
        &self,
        account_id: Option<&str>,
        repo: &ForgeRepoRef,
    ) -> Result<String, ForgeServiceError> {
        self.resolve_client(account_id, repo)
            .map(|(account_id, _client)| account_id)
    }

    /// Fetches one page of a PR diff.
    pub async fn get_diff(
        &self,
        account_id: Option<&str>,
        repo: &ForgeRepoRef,
        number: u32,
        page: u32,
        per_page: Option<u32>,
    ) -> Result<ForgeDiffPage, ForgeServiceError> {
        let (_account, client) = self.resolve_client(account_id, repo)?;
        Ok(client
            .get_diff(
                repo,
                number,
                page.max(1),
                per_page.unwrap_or(30).clamp(1, 100),
            )
            .await?)
    }

    /// Fetches comments, review threads and reviews.
    pub async fn list_comments(
        &self,
        account_id: Option<&str>,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<ForgeCommentPage, ForgeServiceError> {
        self.list_comments_inner(account_id, repo, number, true)
            .await
    }

    /// Fetches comments without enrolling the PR in the checks poller.
    pub async fn list_comments_quiet(
        &self,
        account_id: Option<&str>,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<ForgeCommentPage, ForgeServiceError> {
        self.list_comments_inner(account_id, repo, number, false)
            .await
    }

    async fn list_comments_inner(
        &self,
        account_id: Option<&str>,
        repo: &ForgeRepoRef,
        number: u32,
        register: bool,
    ) -> Result<ForgeCommentPage, ForgeServiceError> {
        let (account, client) = self.resolve_client(account_id, repo)?;
        let page = client.list_comments(repo, number).await?;
        if register {
            self.watch(&account, repo, number);
        }
        Ok(page)
    }

    /// Fetches the head commit's checks and starts watching them.
    pub async fn list_checks(
        &self,
        account_id: Option<&str>,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<ForgeChecks, ForgeServiceError> {
        self.list_checks_inner(account_id, repo, number, true).await
    }

    /// Fetches checks without enrolling the PR in the checks poller.
    pub async fn list_checks_quiet(
        &self,
        account_id: Option<&str>,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<ForgeChecks, ForgeServiceError> {
        self.list_checks_inner(account_id, repo, number, false)
            .await
    }

    async fn list_checks_inner(
        &self,
        account_id: Option<&str>,
        repo: &ForgeRepoRef,
        number: u32,
        register: bool,
    ) -> Result<ForgeChecks, ForgeServiceError> {
        let (account, client) = self.resolve_client(account_id, repo)?;
        let checks = client.list_checks(repo, number).await?;
        if register {
            self.remember_checks(&account, repo, number, &checks);
        }
        Ok(checks)
    }

    /// Lists a PR's commits, oldest first.
    pub async fn list_commits(
        &self,
        account_id: Option<&str>,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<h4rn_protocol::ForgeCommits, ForgeServiceError> {
        let (_account, client) = self.resolve_client(account_id, repo)?;
        Ok(client.list_commits(repo, number).await?)
    }

    /// Re-runs a PR's failed checks and returns the refreshed checks.
    ///
    /// Forges without a re-run API answer `unsupported` with the reason the
    /// checks snapshot already carries.
    pub async fn rerun_checks(
        &self,
        account_id: Option<&str>,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<ForgeChecks, ForgeServiceError> {
        let (account, client) = self.resolve_client(account_id, repo)?;
        let requested = client.rerun_failed_checks(repo, number).await?;
        self.audit.record(
            "checks.rerun",
            &account,
            repo,
            number,
            json!({"requested": requested}),
        );
        // The forge needs a moment to flip the runs back to queued; a fresh
        // read right away usually still shows the old result, so the caller
        // keeps polling through the normal checks watcher.
        let checks = client.list_checks(repo, number).await?;
        self.remember_checks(&account, repo, number, &checks);
        Ok(checks)
    }

    /// Replies in a thread or posts a general comment.
    pub async fn reply(
        &self,
        account_id: Option<&str>,
        repo: &ForgeRepoRef,
        number: u32,
        thread_id: Option<&str>,
        comment_id: &str,
        body: &str,
    ) -> Result<ForgeReviewComment, ForgeServiceError> {
        let (account, client) = self.resolve_client(account_id, repo)?;
        let comment = client
            .reply(repo, number, thread_id, comment_id, body)
            .await?;
        self.audit.record(
            "comment.replied",
            &account,
            repo,
            number,
            json!({"thread": thread_id, "comment": comment_id, "chars": body.len()}),
        );
        Ok(comment)
    }

    /// Posts a general PR comment.
    pub async fn create_comment(
        &self,
        account_id: Option<&str>,
        repo: &ForgeRepoRef,
        number: u32,
        body: &str,
    ) -> Result<ForgeComment, ForgeServiceError> {
        let (account, client) = self.resolve_client(account_id, repo)?;
        let comment = client.create_comment(repo, number, body).await?;
        self.audit.record(
            "comment.created",
            &account,
            repo,
            number,
            json!({"chars": body.len()}),
        );
        Ok(comment)
    }

    /// Pushes a thread's branch and opens a PR.
    pub async fn open_pr(
        &self,
        manager: &Arc<ThreadManager>,
        thread_id: &ThreadId,
        repo: Option<ForgeRepoRef>,
        title: &str,
        body: &str,
        base: &str,
    ) -> Result<ForgePullRequest, ForgeServiceError> {
        let snapshot = manager.get_thread(thread_id, 0).await?;
        let workspace = PathBuf::from(&snapshot.summary.workspace_path);
        let remotes = detect_repos(&workspace, &self.hosts)?;
        let repo = match repo {
            Some(repo) => repo,
            None => remotes
                .first()
                .map(|remote| remote.repo.clone())
                .ok_or_else(|| {
                    ForgeError::NotConfigured("no supported forge remote in this workspace".into())
                })?,
        };
        let branch = crate::fs::git::branch(&workspace).ok_or_else(|| {
            ForgeError::Local("the workspace has a detached HEAD; check out a branch".into())
        })?;
        let remote = remotes
            .iter()
            .find(|remote| remote.repo == repo)
            .or_else(|| remotes.first())
            .map(|remote| remote.remote.clone())
            .ok_or_else(|| ForgeError::NotConfigured("the workspace has no git remote".into()))?;

        push_branch(&workspace, &remote, &branch).await?;
        let (account, client) = self.resolve_client(None, &repo)?;
        let pr = client
            .open_pr(
                &repo,
                &ForgeNewPr {
                    title: title.to_owned(),
                    body: body.to_owned(),
                    head: branch,
                    base: base.to_owned(),
                },
            )
            .await?;
        self.watch(&account, &repo, pr.summary.number);
        self.audit.record(
            "pr.opened",
            &account,
            &repo,
            pr.summary.number,
            json!({"base": base, "thread": thread_id.as_ref()}),
        );
        Ok(pr)
    }

    /// Renders a PR's context and hands it to an existing or new agent thread.
    pub async fn hand_pr_to_agent(
        &self,
        manager: &Arc<ThreadManager>,
        account_id: Option<&str>,
        repo: &ForgeRepoRef,
        number: u32,
        thread_id: Option<&ThreadId>,
        target: Option<ForgeAgentTarget>,
    ) -> Result<ForgeHandoff, ForgeServiceError> {
        let (account, client) = self.resolve_client(account_id, repo)?;
        let pr = client.get_pr(repo, number).await?;
        let comments = client.list_comments(repo, number).await?;
        let checks = client.list_checks(repo, number).await?;
        let context = pr_context_for_agent(&pr, &comments.threads, &checks);
        self.watch(&account, repo, number);
        self.remember_checks(&account, repo, number, &checks);

        let (thread_id, turn_id) = match thread_id {
            Some(thread_id) => {
                let accepted = manager
                    .send_message(thread_id, context.clone(), Vec::new())
                    .await?;
                (accepted.thread_id, Some(accepted.turn_id))
            }
            None => {
                let target = target.ok_or_else(|| {
                    ForgeError::InvalidRequest(
                        "a target is required when no thread is linked".into(),
                    )
                })?;
                let created = manager
                    .create_thread(NewThread {
                        thread_id: None,
                        parent_thread_id: None,
                        subagent_id: None,
                        host: target.host,
                        harness: target.harness,
                        agent_id: None,
                        workspace_path: target.workspace_path,
                        model: target.model,
                        prompt: context.clone(),
                        attachments: Vec::new(),
                        runtime_mode: RuntimeMode::ApprovalRequired,
                        worktree: None,
                    })
                    .await?;
                (created.thread_id, Some(created.turn_id))
            }
        };

        self.audit.record(
            "pr.handed_to_agent",
            &account,
            repo,
            number,
            json!({"thread": thread_id.as_ref()}),
        );
        Ok(ForgeHandoff {
            thread_id,
            turn_id,
            context,
            pr: pr.summary,
        })
    }

    fn insert_account(&self, account: ForgeAccount) -> Result<(), ForgeServiceError> {
        let mut accounts = self
            .accounts
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        accounts.push(account);
        save_accounts(&self.accounts_path, &accounts)
    }

    /// Resolves the account and client for a repository.
    fn resolve_client(
        &self,
        account_id: Option<&str>,
        repo: &ForgeRepoRef,
    ) -> Result<(String, Arc<dyn Forge>), ForgeServiceError> {
        let account = match account_id {
            Some(account_id) => {
                let accounts = self
                    .accounts
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                accounts
                    .iter()
                    .find(|account| account.account_id == account_id)
                    .cloned()
                    .ok_or_else(|| {
                        ForgeError::NotConfigured(format!("no forge account {account_id}"))
                    })?
            }
            None => {
                let accounts = self
                    .accounts
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let mut matches = accounts
                    .iter()
                    .filter(|account| account_matches_repo(account, repo));
                let first = matches.next().cloned().ok_or_else(|| {
                    ForgeError::NotConfigured(format!(
                        "no {} account for {}",
                        repo.forge.display_name(),
                        repo.base_url
                    ))
                })?;
                if matches.next().is_some() {
                    return Err(ForgeError::InvalidRequest(format!(
                        "several {} accounts match {}; name account_id",
                        repo.forge.display_name(),
                        repo.base_url
                    ))
                    .into());
                }
                first
            }
        };
        if !account_matches_repo(&account, repo) {
            return Err(ForgeError::InvalidRequest(format!(
                "account {} is for {} at {}, not {} at {}",
                account.account_id,
                account.forge.display_name(),
                account.base_url,
                repo.forge.display_name(),
                repo.base_url
            ))
            .into());
        }
        let client = self.client(&account)?;
        Ok((account.account_id, client))
    }

    /// Returns the cached client, building it from the stored token if needed.
    fn client(&self, account: &ForgeAccount) -> Result<Arc<dyn Forge>, ForgeServiceError> {
        if let Some(client) = self
            .clients
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&account.account_id)
            .cloned()
        {
            return Ok(client);
        }
        let token =
            self.tokens
                .load(&account.account_id)?
                .ok_or_else(|| ForgeError::Unauthorized {
                    forge: account.forge,
                    message: "the stored token is missing; add the account again".into(),
                })?;
        let client = client_for_with_budget(account, &token, self.requests_per_minute)?;
        self.clients
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(account.account_id.clone(), Arc::clone(&client));
        Ok(client)
    }

    fn watch(&self, account_id: &str, repo: &ForgeRepoRef, number: u32) {
        self.watches
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry((account_id.to_owned(), repo.clone(), number))
            .or_insert(WatchState { signature: None });
    }

    /// The PRs whose checks the 30-second poller reads, for daemon tests.
    #[cfg(test)]
    pub(crate) fn watched_prs(&self) -> Vec<(String, ForgeRepoRef, u32)> {
        self.watches
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    fn remember_checks(
        &self,
        account_id: &str,
        repo: &ForgeRepoRef,
        number: u32,
        checks: &ForgeChecks,
    ) {
        self.watches
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                (account_id.to_owned(), repo.clone(), number),
                WatchState {
                    signature: Some(checks_signature(checks)),
                },
            );
    }

    fn start_poller(self: &Arc<Self>) {
        let service = Arc::clone(self);
        let handle = tokio::spawn(async move { service.poll_loop().await });
        if let Ok(mut poller) = self.poller.lock() {
            *poller = Some(handle);
        }
    }

    async fn poll_loop(self: Arc<Self>) {
        let base = std::env::var("H4RN_FORGE_POLL_SECS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|value| *value >= 5)
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(DEFAULT_POLL_SECS));
        let mut backoff = base;
        loop {
            let watches: Vec<(String, ForgeRepoRef, u32)> = self
                .watches
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .keys()
                .cloned()
                .collect();
            let mut failed = false;
            for (account_id, repo, number) in watches {
                let checks = match self.client_for_account(&account_id, &repo) {
                    Ok((_account, client)) => match client.list_checks(&repo, number).await {
                        Ok(checks) => checks,
                        Err(error) => {
                            failed = true;
                            tracing::debug!(%error, pr = number, "forge checks poll failed");
                            continue;
                        }
                    },
                    Err(error) => {
                        failed = true;
                        tracing::debug!(%error, "forge checks poll skipped an account");
                        continue;
                    }
                };
                let signature = checks_signature(&checks);
                let changed = {
                    let mut watches = self
                        .watches
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    match watches.get_mut(&(account_id, repo.clone(), number)) {
                        Some(state) => {
                            let changed = state.signature.as_deref() != Some(signature.as_str());
                            state.signature = Some(signature);
                            changed
                        }
                        None => false,
                    }
                };
                if changed {
                    let _ = self.updates.send(ForgeChecksUpdate {
                        repo: repo.clone(),
                        number,
                        checks,
                    });
                }
            }
            let delay = if failed {
                backoff = (backoff * 2).min(Duration::from_secs(MAX_BACKOFF_SECS));
                backoff
            } else {
                backoff = base;
                base
            };
            tokio::time::sleep(delay).await;
        }
    }

    fn client_for_account(
        &self,
        account_id: &str,
        _repo: &ForgeRepoRef,
    ) -> Result<(String, Arc<dyn Forge>), ForgeServiceError> {
        let account = {
            let accounts = self
                .accounts
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            accounts
                .iter()
                .find(|account| account.account_id == account_id)
                .cloned()
                .ok_or_else(|| {
                    ForgeError::NotConfigured(format!("no forge account {account_id}"))
                })?
        };
        let client = self.client(&account)?;
        Ok((account.account_id, client))
    }
}

fn load_accounts(path: &Path) -> Result<Vec<ForgeAccount>, ForgeServiceError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(ForgeServiceError::Local(format!(
            "read forge accounts {}: {error}",
            path.display()
        ))),
    }
}

fn save_accounts(path: &Path, accounts: &[ForgeAccount]) -> Result<(), ForgeServiceError> {
    let bytes = serde_json::to_vec_pretty(accounts)?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&temporary)?.permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(&temporary, permissions)?;
    }
    std::fs::rename(&temporary, path)?;
    Ok(())
}

async fn push_branch(
    workspace: &Path,
    remote: &str,
    branch: &str,
) -> Result<(), ForgeServiceError> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["push", "-u", remote, branch])
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|error| ForgeServiceError::Local(format!("run git push: {error}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ForgeServiceError::Local(format!(
            "git push failed: {}",
            h4rn_forge::redact(stderr.trim())
        )));
    }
    Ok(())
}

/// Chooses the Bitbucket credential scheme from what the user supplied.
///
/// A username (the Atlassian account email) means an API token (HTTP Basic).
/// Without one the token is a workspace, project or repository access token
/// (Bearer). The chosen source is stored so later client construction makes
/// the same decision.
fn bitbucket_auth_source(
    forge: ForgeKind,
    source: ForgeCredentialSource,
    username: Option<String>,
) -> (ForgeCredentialSource, Option<String>) {
    match (forge, source, username) {
        (ForgeKind::Bitbucket, ForgeCredentialSource::Pat, Some(username)) => {
            (ForgeCredentialSource::BitbucketApiToken, Some(username))
        }
        (ForgeKind::Bitbucket, ForgeCredentialSource::Pat, None) => {
            (ForgeCredentialSource::BitbucketAccessToken, None)
        }
        (_, source, username) => (source, username),
    }
}

/// Whether a stored account serves a repository.
///
/// Azure DevOps also matches the legacy `DefaultCollection` form, so a remote
/// that spells `https://<org>.visualstudio.com/DefaultCollection` resolves the
/// account added as `https://<org>.visualstudio.com` (and the other way
/// round).
fn account_matches_repo(account: &ForgeAccount, repo: &ForgeRepoRef) -> bool {
    account.forge == repo.forge
        && h4rn_forge::base_urls_match(account.forge, &account.base_url, &repo.base_url)
}

fn checks_signature(checks: &ForgeChecks) -> String {
    let mut signature = format!(
        "{:?}:{}:{}:{}:{}",
        checks.state, checks.total, checks.passed, checks.failed, checks.pending
    );
    for check in &checks.checks {
        signature.push('|');
        signature.push_str(&check.name);
        signature.push(':');
        signature.push_str(&format!("{:?}", check.status));
    }
    signature
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(ACCOUNTS_FILE);
        let account = ForgeAccount {
            account_id: "acct_1".into(),
            forge: ForgeKind::GitHub,
            base_url: "https://github.com".into(),
            username: Some("alice".into()),
            display_name: Some("Alice".into()),
            scopes: vec!["repo".into()],
            source: ForgeCredentialSource::Pat,
            created_at: Utc::now(),
        };
        save_accounts(&path, std::slice::from_ref(&account)).unwrap();
        assert_eq!(load_accounts(&path).unwrap(), vec![account]);
    }

    #[test]
    fn missing_account_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            load_accounts(&dir.path().join(ACCOUNTS_FILE))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn checks_signature_tracks_statuses() {
        let repo = ForgeRepoRef {
            forge: ForgeKind::GitHub,
            base_url: "https://github.com".into(),
            owner: "o".into(),
            repo: "r".into(),
        };
        let checks = h4rn_forge::finish_checks(&repo, 1, "sha".into(), Vec::new());
        let first = checks_signature(&checks);
        let mut changed = checks.clone();
        changed.checks.push(h4rn_protocol::ForgeCheck {
            name: "ci".into(),
            status: h4rn_protocol::ForgeCheckStatus::Failure,
            details_url: None,
            description: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            required: None,
        });
        let second = checks_signature(&changed);
        assert_ne!(first, second);
    }

    #[test]
    fn bitbucket_auth_scheme_is_chosen_from_the_username() {
        use h4rn_protocol::ForgeCredentialSource as Source;
        assert_eq!(
            bitbucket_auth_source(ForgeKind::Bitbucket, Source::Pat, Some("alice".into())),
            (Source::BitbucketApiToken, Some("alice".into()))
        );
        assert_eq!(
            bitbucket_auth_source(ForgeKind::Bitbucket, Source::Pat, None),
            (Source::BitbucketAccessToken, None)
        );
        // Other forges keep the source the client sent.
        assert_eq!(
            bitbucket_auth_source(ForgeKind::GitHub, Source::Pat, Some("alice".into())),
            (Source::Pat, Some("alice".into()))
        );
    }

    #[tokio::test]
    async fn a_repository_token_is_validated_against_the_repository_hint() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Both account endpoints reject a repository access token; only the
        // repository it was granted for can prove it.
        let server = MockServer::start().await;
        for endpoint in ["/2.0/user", "/2.0/user/workspaces"] {
            Mock::given(method("GET"))
                .and(path(endpoint))
                .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                    "type": "error",
                    "error": {"message": "Unauthorized"}
                })))
                .mount(&server)
                .await;
        }
        Mock::given(method("GET"))
            .and(path("/2.0/repositories/acme/widget"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "type": "repository",
                "name": "widget",
                "full_name": "acme/widget"
            })))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let service = ForgeService::open(dir.path()).unwrap();
        service.shutdown();
        let repo = ForgeRepoRef {
            forge: ForgeKind::Bitbucket,
            base_url: server.uri(),
            owner: "acme".into(),
            repo: "widget".into(),
        };
        let account = ForgeAccount {
            account_id: "acct_repo_token".into(),
            forge: ForgeKind::Bitbucket,
            base_url: server.uri(),
            username: None,
            display_name: None,
            scopes: Vec::new(),
            source: ForgeCredentialSource::BitbucketAccessToken,
            created_at: Utc::now(),
        };
        let client: Arc<dyn Forge> = Arc::new(
            h4rn_forge::BitbucketClient::new(&server.uri(), "ATBB-repo", None, 120).unwrap(),
        );
        let saved = service
            .finish_add_account(account, "ATBB-repo", client, Some(&repo))
            .await
            .unwrap();
        assert_eq!(saved.username, None, "a username would select Basic auth");
        assert!(
            service
                .list_accounts()
                .0
                .iter()
                .any(|account| account.account_id == "acct_repo_token")
        );
        let paths: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|request| request.url.path().to_owned())
            .collect();
        assert!(paths.iter().any(|path| path == "/2.0/user"), "{paths:?}");
        assert!(
            paths
                .iter()
                .any(|path| path == "/2.0/repositories/acme/widget"),
            "{paths:?}"
        );
        // Leave no stored credential behind in the developer's keychain.
        service.remove_account("acct_repo_token").unwrap();
    }

    #[test]
    fn default_collection_remote_finds_the_plain_account() {
        let account = ForgeAccount {
            account_id: "acct_azure".into(),
            forge: ForgeKind::AzureDevOps,
            base_url: "https://acme.visualstudio.com".into(),
            username: None,
            display_name: None,
            scopes: Vec::new(),
            source: ForgeCredentialSource::Pat,
            created_at: Utc::now(),
        };
        let repo = ForgeRepoRef {
            forge: ForgeKind::AzureDevOps,
            base_url: "https://acme.visualstudio.com/DefaultCollection".into(),
            owner: "platform".into(),
            repo: "widget".into(),
        };
        assert!(account_matches_repo(&account, &repo));
        let reversed = ForgeAccount {
            base_url: repo.base_url.clone(),
            ..account.clone()
        };
        let plain = ForgeRepoRef {
            base_url: account.base_url.clone(),
            ..repo.clone()
        };
        assert!(account_matches_repo(&reversed, &plain));

        // Another organization is still a different account.
        let other = ForgeRepoRef {
            base_url: "https://other.visualstudio.com".into(),
            ..repo
        };
        assert!(!account_matches_repo(&account, &other));
    }

    #[test]
    fn service_error_maps_to_protocol_codes() {
        let error = ForgeServiceError::Forge(ForgeError::RateLimited {
            forge: ForgeKind::GitHub,
            retry_after_secs: Some(30),
            retry_at: None,
            message: "slow down".into(),
        });
        assert_eq!(error.into_proto().code, ProtoErrorCode::RateLimited);
    }
}
