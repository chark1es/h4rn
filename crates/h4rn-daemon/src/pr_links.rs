//! Thread ↔ pull request links and the PR watch.
//!
//! A link is durable metadata owned by the thread journal: the forge account,
//! repository and PR number. The state shown beside a link always comes from a
//! fresh forge read; a failed read is reported as such, never cached as if it
//! were true.
//!
//! A watch is daemon memory. While the thread is active, the daemon polls the
//! PR every two minutes — one poll shared by every thread watching the same
//! PR — and appends a plain-language message that starts a turn when
//! something the reviewer would care about changed.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use h4rn_forge::ForgeError;
use h4rn_protocol::{
    ForgeAccount, ForgeCheck, ForgeChecks, ForgeChecksState, ForgeCommentPage, ForgeMergeable,
    ForgePrState, ForgePrSummary, ForgePullRequest, ForgeRepoRef, ForgeReviewState, PrLinkedThread,
    ProtoError, ProtoErrorCode, RuntimeMode, ThreadId, ThreadPullRequest, ThreadTab,
};
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

use crate::forge::ForgeServiceError;
use crate::journal::{Journal, JournalError};
use crate::mcp_server::OAuthStore;
use crate::threads::ThreadManager;

/// How often a watched PR is polled.
pub const WATCH_POLL_SECS: u64 = 120;
/// Consecutive wakes that brought only comments before a watch ends.
const MAX_COMMENT_ONLY_WAKES: u32 = 10;
/// Consecutive failed reads before a watch ends.
const MAX_FAILED_READS: u32 = 8;
/// Consecutive wakes the daemon could not deliver before a watch ends.
const MAX_FAILED_WAKES: u32 = 8;
/// Shortest pause applied when a forge reports a rate limit without a reset.
const DEFAULT_RATE_LIMIT_PAUSE_SECS: u64 = 120;

/// One durable thread → pull request link.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrLinkRecord {
    /// Thread that holds the link.
    pub thread_id: ThreadId,
    /// Account the daemon reads the PR with.
    pub account_id: String,
    /// Repository.
    pub repo: ForgeRepoRef,
    /// PR/MR number.
    pub number: u32,
    /// When the link was created.
    pub linked_at: DateTime<Utc>,
}

/// A failure while linking or reading a linked PR.
#[derive(Debug, thiserror::Error)]
pub enum PrLinkError {
    /// The thread does not exist on this daemon.
    #[error("thread {0} was not found")]
    ThreadNotFound(String),
    /// No link matches the request.
    #[error("{0}")]
    NotLinked(String),
    /// The forge failed.
    #[error("{0}")]
    Forge(String),
    /// The journal failed.
    #[error("{0}")]
    Journal(String),
}

impl PrLinkError {
    /// Maps the failure onto the protocol error class and a user hint.
    pub fn into_proto(self) -> ProtoError {
        match self {
            Self::ThreadNotFound(message) => ProtoError::new(
                ProtoErrorCode::NotFound,
                message,
                "Refresh the thread list and try again",
            ),
            Self::NotLinked(message) => ProtoError::new(
                ProtoErrorCode::InvalidRequest,
                message,
                "Link the pull request before watching it",
            ),
            Self::Forge(message) => ProtoError::new(
                ProtoErrorCode::Internal,
                message,
                "Check the forge account and try again",
            ),
            Self::Journal(message) => ProtoError::new(
                ProtoErrorCode::Internal,
                message,
                "Check the daemon log for journal details",
            ),
        }
    }
}

impl From<JournalError> for PrLinkError {
    fn from(error: JournalError) -> Self {
        Self::Journal(error.to_string())
    }
}

/// A forge read the watch needs.
#[derive(Debug, Clone)]
pub enum PrSourceError {
    /// The account's quota is exhausted; the watch pauses and resumes later.
    RateLimited {
        /// Seconds the forge asked the caller to wait, when it said.
        retry_after: Option<u64>,
    },
    /// Any other read failure.
    Other(String),
}

impl PrSourceError {
    fn message(&self) -> String {
        match self {
            Self::RateLimited { .. } => "the forge rate limit paused the watch".into(),
            Self::Other(message) => message.clone(),
        }
    }
}

impl From<ForgeServiceError> for PrSourceError {
    fn from(error: ForgeServiceError) -> Self {
        match error {
            ForgeServiceError::Forge(ForgeError::RateLimited {
                retry_after_secs, ..
            }) => Self::RateLimited {
                retry_after: retry_after_secs,
            },
            other => Self::Other(other.to_string()),
        }
    }
}

/// The forge reads the service performs.
///
/// A trait rather than a concrete [`crate::forge::ForgeService`] so the watch
/// state machine is tested against a scripted forge without a socket.
#[async_trait]
pub trait PrSource: Send + Sync {
    /// Picks the account for a repository and returns its id.
    async fn resolve_account(
        &self,
        account_id: Option<&str>,
        repo: &ForgeRepoRef,
    ) -> Result<String, PrSourceError>;

    /// The authenticated login for an account, used to ignore the person's own
    /// comments and reviews.
    fn account_login(&self, account_id: &str) -> Option<String>;

    /// Reads one PR's detail.
    ///
    /// This is the watch's only change probe: on a forge whose HTTP client
    /// revalidates with `If-None-Match`, an unchanged PR comes back as a `304`
    /// served from the account client's cache without spending a second
    /// request. The snapshot is built from the body that came back — never
    /// from this watch's previous snapshot — so another reader refreshing the
    /// shared cache cannot hide a merge or a close from the watch.
    async fn get_pr(
        &self,
        account_id: &str,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<ForgePullRequest, PrSourceError>;

    /// Reads comments, review threads and reviews.
    async fn list_comments(
        &self,
        account_id: &str,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<ForgeCommentPage, PrSourceError>;

    /// Reads the head commit's checks.
    async fn list_checks(
        &self,
        account_id: &str,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<ForgeChecks, PrSourceError>;
}

/// The outside grant that asked for a watched PR's turn.
///
/// A watch outlives the turn that created it, so it keeps both halves of the
/// grant's authority: which grant it is (checked again at wake time) and the
/// most autonomy that turn was allowed. Nothing secret is stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchGrant {
    /// The grant's client id.
    pub client_id: String,
    /// The ceiling the creating turn ran under, when it had one.
    pub ceiling: Option<RuntimeMode>,
}

/// Why a wake did not start a turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WakeError {
    /// The grant that asked for the watch is gone. The watch can never deliver
    /// anything, so it is dropped instead of retried.
    Revoked(String),
    /// A transient failure; the next poll retries this wake.
    Retry(String),
}

/// What the watch needs from the thread manager.
#[async_trait]
pub trait PrWakeSink: Send + Sync {
    /// The thread's title, or `None` when it does not exist.
    async fn thread_title(&self, thread_id: &ThreadId) -> Option<String>;

    /// Whether the thread is still active: present and not settled (archived).
    async fn is_active(&self, thread_id: &ThreadId) -> bool;

    /// Starts a turn in the thread like a queued prompt.
    ///
    /// `grant` is the outside grant the watch was created under. The sink
    /// re-checks it at wake time and starts the turn at no more than its
    /// ceiling.
    async fn wake(
        &self,
        thread_id: &ThreadId,
        message: String,
        grant: Option<WatchGrant>,
    ) -> Result<(), WakeError>;
}

/// The daemon's thread manager as a wake sink.
pub struct ThreadWakeSink {
    manager: Arc<ThreadManager>,
    grants: Arc<OAuthStore>,
}

impl ThreadWakeSink {
    /// Builds the sink for one manager and the grant store it checks.
    pub fn new(manager: Arc<ThreadManager>, grants: Arc<OAuthStore>) -> Self {
        Self { manager, grants }
    }
}

#[async_trait]
impl PrWakeSink for ThreadWakeSink {
    async fn thread_title(&self, thread_id: &ThreadId) -> Option<String> {
        self.manager
            .thread_summary(thread_id)
            .await
            .map(|summary| summary.title)
    }

    async fn is_active(&self, thread_id: &ThreadId) -> bool {
        self.manager
            .thread_summary(thread_id)
            .await
            .is_some_and(|summary| summary.tab != ThreadTab::Settled)
    }

    async fn wake(
        &self,
        thread_id: &ThreadId,
        message: String,
        grant: Option<WatchGrant>,
    ) -> Result<(), WakeError> {
        let mut ceiling = None;
        if let Some(grant) = grant {
            // Checked at the moment the work would start: a grant revoked after
            // the watch was made must not start a turn.
            if !self.grants.grant_is_live(&grant.client_id) {
                return Err(WakeError::Revoked(
                    "the grant that started watching this pull request was revoked".into(),
                ));
            }
            ceiling = grant.ceiling;
        }
        self.manager
            .send_message_at_most(thread_id, message, Vec::new(), ceiling)
            .await
            .map(|_| ())
            .map_err(|error| WakeError::Retry(error.to_string()))
    }
}

/// The service behind the link and watch requests.
pub struct PrLinkService {
    journal: Arc<dyn Journal>,
    source: Arc<dyn PrSource>,
    sink: Arc<dyn PrWakeSink>,
    watches: Mutex<WatchRegistry>,
    poller: Mutex<Option<JoinHandle<()>>>,
}

impl PrLinkService {
    /// Builds the service. Call [`Self::start`] to begin polling.
    pub fn new(
        journal: Arc<dyn Journal>,
        source: Arc<dyn PrSource>,
        sink: Arc<dyn PrWakeSink>,
    ) -> Arc<Self> {
        Arc::new(Self {
            journal,
            source,
            sink,
            watches: Mutex::new(WatchRegistry::default()),
            poller: Mutex::new(None),
        })
    }

    /// Starts the two-minute poll loop.
    pub fn start(self: &Arc<Self>) {
        let service = Arc::clone(self);
        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(WATCH_POLL_SECS));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first tick fires immediately; poll right away so a watch
            // registered just before startup is not idle for two minutes.
            loop {
                interval.tick().await;
                service.poll_once().await;
            }
        });
        if let Ok(mut poller) = self.poller.lock() {
            *poller = Some(handle);
        }
    }

    /// Stops the poll loop. Watches in memory go with it.
    pub fn shutdown(&self) {
        if let Ok(mut poller) = self.poller.lock()
            && let Some(handle) = poller.take()
        {
            handle.abort();
        }
    }

    fn lock_watches(&self) -> std::sync::MutexGuard<'_, WatchRegistry> {
        self.watches
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Links a PR after reading it once, so the caller gets its current state.
    pub async fn link(
        &self,
        thread_id: &ThreadId,
        account_id: Option<&str>,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<ThreadPullRequest, PrLinkError> {
        if self.sink.thread_title(thread_id).await.is_none() {
            return Err(PrLinkError::ThreadNotFound(thread_id.as_str().to_owned()));
        }
        let account_id = self
            .source
            .resolve_account(account_id, repo)
            .await
            .map_err(source_error)?;
        let pr = self
            .source
            .get_pr(&account_id, repo, number)
            .await
            .map_err(source_error)?;
        let record = PrLinkRecord {
            thread_id: thread_id.clone(),
            account_id,
            repo: repo.clone(),
            number,
            linked_at: Utc::now(),
        };
        self.journal.upsert_pr_link(&record).await?;
        Ok(self.thread_pull(record, Some(pr.summary), None))
    }

    /// Records a link the caller already read (the PR it just opened).
    pub async fn link_known(
        &self,
        thread_id: &ThreadId,
        account_id: &str,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<(), PrLinkError> {
        let record = PrLinkRecord {
            thread_id: thread_id.clone(),
            account_id: account_id.to_owned(),
            repo: repo.clone(),
            number,
            linked_at: Utc::now(),
        };
        self.journal.upsert_pr_link(&record).await?;
        Ok(())
    }

    /// Links the PR a thread just opened with the account that opened it.
    ///
    /// The create path already has the PR from the forge, so this costs no
    /// read; it is one call so the handler cannot forget the link.
    pub async fn link_opened(
        &self,
        thread_id: &ThreadId,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<(), PrLinkError> {
        let account_id = self
            .source
            .resolve_account(None, repo)
            .await
            .map_err(source_error)?;
        self.link_known(thread_id, &account_id, repo, number).await
    }

    /// Removes one link and ends any watch on it.
    pub async fn unlink(
        &self,
        thread_id: &ThreadId,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<(), PrLinkError> {
        self.journal.remove_pr_link(thread_id, repo, number).await?;
        let mut watches = self.lock_watches();
        for key in matching_keys(&watches, repo, number) {
            if let Some(watched) = watches.prs.get_mut(&key) {
                watched.watchers.remove(thread_id.as_str());
                if watched.watchers.is_empty() {
                    watches.prs.remove(&key);
                }
            }
        }
        Ok(())
    }

    /// The links a thread holds, without reading the forge.
    ///
    /// The agent tools use this to resolve a repository for unlink/unwatch and
    /// to answer "what is linked" without a forge round trip per link.
    pub async fn records(&self, thread_id: &ThreadId) -> Result<Vec<PrLinkRecord>, PrLinkError> {
        self.journal
            .pr_links_for_thread(thread_id)
            .await
            .map_err(PrLinkError::from)
    }

    /// Lists a thread's links with the latest state from the forge.
    pub async fn list(&self, thread_id: &ThreadId) -> Result<Vec<ThreadPullRequest>, PrLinkError> {
        if self.sink.thread_title(thread_id).await.is_none() {
            return Err(PrLinkError::ThreadNotFound(thread_id.as_str().to_owned()));
        }
        let records = self.journal.pr_links_for_thread(thread_id).await?;
        let mut pulls = Vec::with_capacity(records.len());
        for record in records {
            let (summary, error) = match self
                .source
                .get_pr(&record.account_id, &record.repo, record.number)
                .await
            {
                Ok(pr) => (Some(pr.summary), None),
                Err(error) => (None, Some(error.message())),
            };
            pulls.push(self.thread_pull(record, summary, error));
        }
        Ok(pulls)
    }

    /// Threads that link to one PR, for the PR page.
    pub async fn list_for_pr(
        &self,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<Vec<PrLinkedThread>, PrLinkError> {
        let records = self.journal.pr_links_for_pr(repo, number).await?;
        let watching = self.watching_threads(repo, number);
        let mut threads = Vec::with_capacity(records.len());
        for record in records {
            let Some(title) = self.sink.thread_title(&record.thread_id).await else {
                continue;
            };
            threads.push(PrLinkedThread {
                thread_id: record.thread_id.clone(),
                title,
                watching: watching.contains(record.thread_id.as_str()),
            });
        }
        Ok(threads)
    }

    /// Starts watching a linked PR and takes the baseline snapshot now.
    pub async fn watch(
        &self,
        thread_id: &ThreadId,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<(), PrLinkError> {
        self.watch_with_grant(thread_id, repo, number, None).await
    }

    /// Starts watching a linked PR for work an outside grant asked for.
    ///
    /// The grant and its ceiling are kept on the watcher: every wake it causes
    /// re-checks the grant and starts its turn at no more than this ceiling.
    pub async fn watch_with_grant(
        &self,
        thread_id: &ThreadId,
        repo: &ForgeRepoRef,
        number: u32,
        grant: Option<WatchGrant>,
    ) -> Result<(), PrLinkError> {
        let record = self
            .journal
            .pr_links_for_thread(thread_id)
            .await?
            .into_iter()
            .find(|record| record.repo == *repo && record.number == number)
            .ok_or_else(|| {
                PrLinkError::NotLinked(format!(
                    "{}#{number} is not linked to this thread",
                    repo.full_name()
                ))
            })?;
        let key = PrKey::from_record(&record);
        let generation = {
            let mut watches = self.lock_watches();
            let fresh = watches.allocate_generation();
            let watched = watches.prs.entry(key.clone()).or_default();
            let watch = watched
                .watchers
                .entry(thread_id.as_str().to_owned())
                .or_insert_with(|| ThreadWatch {
                    generation: fresh,
                    ..ThreadWatch::default()
                });
            watch.generation = fresh;
            watch.grant = grant.clone();
            watch.generation
        };

        // A rate limit belongs to the account and the PR, so the watch must
        // not spend three requests while the poller is waiting out the pause.
        // The first poll after the pause takes the baseline instead.
        if self.paused(&key) {
            let mut watches = self.lock_watches();
            let fresh = watches.allocate_generation();
            if let Some(watch) = watches
                .prs
                .get_mut(&key)
                .and_then(|watched| watched.watchers.get_mut(thread_id.as_str()))
            {
                watch.baseline = None;
                watch.failure_seen = false;
                watch.comment_only_wakes = 0;
                watch.failed_reads = 0;
                watch.generation = fresh;
                watch.grant = grant.clone();
            }
            tracing::debug!(
                repo = %repo.full_name(),
                pr = number,
                "PR watch waits out a rate-limit pause before its first read"
            );
            return Ok(());
        }

        // The first read is this watcher's baseline: events that happened
        // before it started watching are not news. Other watchers keep their
        // own baseline, and a pause the poller recorded survives this
        // watcher's arrival. A transient failure leaves the watch running;
        // its first successful read becomes the baseline.
        match self.read_full(&record.account_id, repo, number).await {
            Ok(snapshot)
                if matches!(
                    snapshot.summary.state,
                    ForgePrState::Merged | ForgePrState::Closed
                ) =>
            {
                // A merged or closed PR will never change in a way the watch
                // reports, so there is nothing to watch: end it now rather
                // than polling it forever. Unless a poll applied a newer
                // state while this read was in flight — an undelivered merge
                // wake may be waiting to retry, and this read must not delete
                // the watcher that will deliver it.
                let current = {
                    let watches = self.lock_watches();
                    watches
                        .prs
                        .get(&key)
                        .and_then(|watched| watched.watchers.get(thread_id.as_str()))
                        .map(|watch| watch.generation)
                };
                match current {
                    Some(current) if current == generation => {
                        self.remove_watch(&key, thread_id);
                    }
                    Some(_) => {
                        tracing::debug!(
                            repo = %repo.full_name(),
                            pr = number,
                            "discarding a terminal PR watch read a poll overtook"
                        );
                    }
                    None => {}
                }
            }
            Ok(snapshot) => {
                let mut watches = self.lock_watches();
                let WatchRegistry {
                    prs,
                    next_generation,
                } = &mut *watches;
                if let Some(watch) = prs
                    .get_mut(&key)
                    .and_then(|watched| watched.watchers.get_mut(thread_id.as_str()))
                {
                    if watch.generation != generation {
                        // A poll applied a fresher snapshot while this read
                        // was in flight. Writing this one would move the
                        // baseline backwards and make the next poll report
                        // the same change again.
                        tracing::debug!(
                            repo = %repo.full_name(),
                            pr = number,
                            "discarding a PR watch read a poll overtook"
                        );
                        return Ok(());
                    }
                    watch.failure_seen = snapshot.checks.pending > 0 && snapshot.checks.failed > 0;
                    watch.comment_only_wakes = 0;
                    watch.failed_reads = 0;
                    watch.baseline = Some(snapshot);
                    watch.generation = next_generation.fetch_add(1, Ordering::Relaxed);
                }
            }
            Err(error) => {
                tracing::debug!(
                    repo = %repo.full_name(),
                    pr = number,
                    error = %error.message(),
                    "initial PR watch read failed"
                );
            }
        }
        Ok(())
    }

    /// Stops watching one PR.
    pub fn unwatch(&self, thread_id: &ThreadId, repo: &ForgeRepoRef, number: u32) {
        let mut watches = self.lock_watches();
        for key in matching_keys(&watches, repo, number) {
            if let Some(watched) = watches.prs.get_mut(&key) {
                watched.watchers.remove(thread_id.as_str());
                if watched.watchers.is_empty() {
                    watches.prs.remove(&key);
                }
            }
        }
    }

    fn watching_threads(&self, repo: &ForgeRepoRef, number: u32) -> HashSet<String> {
        let watches = self.lock_watches();
        watches
            .prs
            .iter()
            .filter(|(key, _)| key.repo == *repo && key.number == number)
            .flat_map(|(_, watched)| watched.watchers.keys().cloned())
            .collect()
    }

    fn thread_pull(
        &self,
        record: PrLinkRecord,
        summary: Option<ForgePrSummary>,
        error: Option<String>,
    ) -> ThreadPullRequest {
        let watching = self.watching_threads(&record.repo, record.number);
        let is_watching = watching.contains(record.thread_id.as_str());
        let watch_notice = is_watching
            .then(|| self.missing_login_notice(&record.account_id))
            .flatten();
        ThreadPullRequest {
            watching: is_watching,
            repo: record.repo,
            number: record.number,
            account_id: record.account_id,
            summary,
            error,
            watch_notice,
            linked_at: record.linked_at,
        }
    }

    /// What the watch cannot cover, when the account has no known login.
    fn missing_login_notice(&self, account_id: &str) -> Option<String> {
        self.source.account_login(account_id).is_none().then(|| {
            "Comments and reviews are not watched: this account has no login on the forge, so \
             the daemon cannot tell your own words from someone else's."
                .to_owned()
        })
    }

    /// Reads a fresh snapshot for every poll.
    ///
    /// The PR detail is always requested, not probed separately: `get_pr`
    /// already revalidates with the account client's shared `ETag` cache, so
    /// an unchanged PR costs no full read, while a `304` is built from the
    /// body the cache returns. A separate "did it change?" probe would answer
    /// for whoever last read the PR, not for this watch's snapshot, and could
    /// report a merge or close late or not at all.
    async fn read_full(
        &self,
        account_id: &str,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<PrSnapshot, PrSourceError> {
        let pr = self.source.get_pr(account_id, repo, number).await?;
        let comments = self.source.list_comments(account_id, repo, number).await?;
        let checks = self.source.list_checks(account_id, repo, number).await?;
        Ok(PrSnapshot {
            summary: pr.summary,
            mergeable: pr.mergeable,
            checks,
            comments,
        })
    }

    /// One poll of every watched PR, shared by all its threads.
    pub async fn poll_once(&self) {
        let keys: Vec<PrKey> = {
            let watches = self.lock_watches();
            watches
                .prs
                .iter()
                .filter(|(_, watched)| !watched.watchers.is_empty())
                .filter(|(_, watched)| {
                    watched
                        .paused_until
                        .is_none_or(|until| until <= Instant::now())
                })
                .map(|(key, _)| key.clone())
                .collect()
        };

        for key in keys {
            let viewer = self.source.account_login(&key.account_id);

            // Each watcher's generation at the moment this read starts. A
            // re-watch or a failed-wake retry that completes while the read
            // is in flight bumps it, and this snapshot must not be applied
            // over that newer state in either direction.
            let generations: HashMap<String, u64> = {
                let watches = self.lock_watches();
                watches
                    .prs
                    .get(&key)
                    .map(|watched| {
                        watched
                            .watchers
                            .iter()
                            .map(|(thread, watch)| (thread.clone(), watch.generation))
                            .collect()
                    })
                    .unwrap_or_default()
            };

            let snapshot = match self.read_full(&key.account_id, &key.repo, key.number).await {
                Ok(snapshot) => snapshot,
                Err(PrSourceError::RateLimited { retry_after }) => {
                    let pause = retry_after
                        .filter(|seconds| *seconds > 0)
                        .unwrap_or(DEFAULT_RATE_LIMIT_PAUSE_SECS)
                        .max(5);
                    if let Some(watched) = self.lock_watches().prs.get_mut(&key) {
                        watched.paused_until = Some(Instant::now() + Duration::from_secs(pause));
                    }
                    continue;
                }
                Err(error) => {
                    self.record_failed_read(&key, &error.message());
                    continue;
                }
            };

            let threads: Vec<ThreadId> = {
                let watches = self.lock_watches();
                watches
                    .prs
                    .get(&key)
                    .map(|watched| {
                        watched
                            .watchers
                            .keys()
                            .map(|thread| ThreadId::new(thread.clone()))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let mut inactive = HashSet::new();
            for thread_id in &threads {
                if !self.sink.is_active(thread_id).await {
                    inactive.insert(thread_id.as_str().to_owned());
                }
            }

            let wakes = {
                let mut watches = self.lock_watches();
                let WatchRegistry {
                    prs,
                    next_generation,
                } = &mut *watches;
                let Some(watched) = prs.get_mut(&key) else {
                    continue;
                };
                for thread in &inactive {
                    watched.watchers.remove(thread);
                }
                if watched.watchers.is_empty() {
                    continue;
                }
                watched.observe(snapshot, viewer.as_deref(), &generations, next_generation)
            };

            for wake in wakes {
                let Wake {
                    thread_id,
                    message,
                    ends_watch,
                    generation,
                    previous,
                    previous_failure_seen,
                    previous_comment_only_wakes,
                    grant,
                } = wake;
                match self.sink.wake(&thread_id, message, grant).await {
                    Ok(()) => {
                        self.note_wake_delivered(&key, &thread_id, generation);
                        if ends_watch {
                            // Remove only the watcher this wake was computed
                            // for. A re-watch can complete while delivery is
                            // in flight; that newer watcher was not part of
                            // this event and must not be deleted.
                            self.remove_watch_if(&key, &thread_id, generation);
                        }
                    }
                    Err(WakeError::Revoked(reason)) => {
                        // The grant is gone, so this watch can never deliver.
                        // Retrying would keep polling a PR for an agent the
                        // person already disconnected.
                        tracing::info!(
                            thread = %thread_id,
                            %reason,
                            "ending a PR watch whose grant was revoked"
                        );
                        self.remove_watch_if(&key, &thread_id, generation);
                    }
                    Err(WakeError::Retry(error)) => {
                        // The agent never saw these events. Keep the previous
                        // baseline so the next poll recomputes and retries
                        // them; a terminal watch stays alive for that retry.
                        tracing::warn!(
                            thread = %thread_id,
                            %error,
                            "PR watch wake failed; retrying on the next poll"
                        );
                        self.restore_failed_wake(
                            &key,
                            &thread_id,
                            generation,
                            previous,
                            previous_failure_seen,
                            previous_comment_only_wakes,
                        );
                    }
                }
            }
        }

        // Forget PRs whose last watcher went away.
        let mut watches = self.lock_watches();
        watches
            .prs
            .retain(|_, watched| !watched.watchers.is_empty());
    }

    fn remove_watch(&self, key: &PrKey, thread_id: &ThreadId) {
        let mut watches = self.lock_watches();
        let Some(watched) = watches.prs.get_mut(key) else {
            return;
        };
        watched.watchers.remove(thread_id.as_str());
        if watched.watchers.is_empty() {
            watches.prs.remove(key);
        }
    }

    /// Removes a watcher only while it is still the one the caller saw.
    ///
    /// A wake spans the whole `send_message`, which can take a while. The
    /// thread may unwatch and watch again in that window, and the new watcher
    /// did not see this wake's events; it must survive the delivery.
    fn remove_watch_if(&self, key: &PrKey, thread_id: &ThreadId, generation: u64) {
        let mut watches = self.lock_watches();
        let Some(watched) = watches.prs.get_mut(key) else {
            return;
        };
        let same = watched
            .watchers
            .get(thread_id.as_str())
            .is_some_and(|watch| watch.generation == generation);
        if !same {
            tracing::debug!(
                thread = %thread_id,
                pr = key.number,
                "keeping a re-watched PR watch that a delivered wake did not belong to"
            );
            return;
        }
        watched.watchers.remove(thread_id.as_str());
        if watched.watchers.is_empty() {
            watches.prs.remove(key);
        }
    }

    /// Whether the poller is waiting out a rate limit for this PR.
    fn paused(&self, key: &PrKey) -> bool {
        self.lock_watches()
            .prs
            .get(key)
            .and_then(|watched| watched.paused_until)
            .is_some_and(|until| until > Instant::now())
    }

    /// Ends a rate-limit pause early, for tests that cannot wait it out.
    #[cfg(test)]
    fn clear_pause(&self, repo: &ForgeRepoRef, number: u32) {
        let mut watches = self.lock_watches();
        for key in matching_keys(&watches, repo, number) {
            if let Some(watched) = watches.prs.get_mut(&key) {
                watched.paused_until = None;
            }
        }
    }

    fn note_wake_delivered(&self, key: &PrKey, thread_id: &ThreadId, generation: u64) {
        let mut watches = self.lock_watches();
        let Some(watch) = watches
            .prs
            .get_mut(key)
            .and_then(|watched| watched.watchers.get_mut(thread_id.as_str()))
        else {
            return;
        };
        if watch.generation == generation {
            watch.failed_wakes = 0;
        }
    }

    /// Puts a watcher back where it was so an undelivered wake is retried.
    ///
    /// The baseline moves only when the daemon knows the agent heard the
    /// events. A wake that never reached the thread leaves the previous
    /// snapshot in place, so the next poll sees the same change and tries
    /// again. Retries are bounded like failed reads.
    fn restore_failed_wake(
        &self,
        key: &PrKey,
        thread_id: &ThreadId,
        generation: u64,
        previous: PrSnapshot,
        previous_failure_seen: bool,
        previous_comment_only_wakes: u32,
    ) {
        let mut watches = self.lock_watches();
        let WatchRegistry {
            prs,
            next_generation,
        } = &mut *watches;
        let Some(watched) = prs.get_mut(key) else {
            return;
        };
        let Some(watch) = watched.watchers.get_mut(thread_id.as_str()) else {
            return;
        };
        if watch.generation != generation {
            // The thread re-watched while this wake was being delivered. Its
            // new baseline is fresher than the snapshot this poll was about,
            // so there is nothing to restore or retry for that watcher.
            tracing::debug!(
                thread = %thread_id,
                pr = key.number,
                "discarding a failed PR wake a re-watch overtook"
            );
            return;
        }
        watch.baseline = Some(previous);
        watch.failure_seen = previous_failure_seen;
        watch.comment_only_wakes = previous_comment_only_wakes;
        watch.generation = next_generation.fetch_add(1, Ordering::Relaxed);
        watch.failed_wakes += 1;
        if watch.failed_wakes >= MAX_FAILED_WAKES {
            tracing::debug!(
                thread = %thread_id,
                pr = key.number,
                "PR watch ended after repeated failed wakes"
            );
            watched.watchers.remove(thread_id.as_str());
        }
    }

    fn record_failed_read(&self, key: &PrKey, message: &str) {
        let mut watches = self.lock_watches();
        let Some(watched) = watches.prs.get_mut(key) else {
            return;
        };
        let mut ended = Vec::new();
        for (thread, watch) in &mut watched.watchers {
            watch.failed_reads += 1;
            if watch.failed_reads >= MAX_FAILED_READS {
                ended.push(thread.clone());
            }
        }
        if !ended.is_empty() {
            tracing::debug!(
                pr = key.number,
                %message,
                threads = ?ended,
                "PR watch ended after repeated failed reads"
            );
            for thread in ended {
                watched.watchers.remove(&thread);
            }
        }
    }
}

fn source_error(error: PrSourceError) -> PrLinkError {
    PrLinkError::Forge(error.message())
}

fn matching_keys(watches: &WatchRegistry, repo: &ForgeRepoRef, number: u32) -> Vec<PrKey> {
    watches
        .prs
        .keys()
        .filter(|key| key.repo == *repo && key.number == number)
        .cloned()
        .collect()
}

/// Identity of one watched PR.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PrKey {
    account_id: String,
    repo: ForgeRepoRef,
    number: u32,
}

impl PrKey {
    fn from_record(record: &PrLinkRecord) -> Self {
        Self {
            account_id: record.account_id.clone(),
            repo: record.repo.clone(),
            number: record.number,
        }
    }
}

#[derive(Default)]
struct WatchRegistry {
    prs: HashMap<PrKey, WatchedPr>,
    /// Every watcher generation, and every bump to one, is drawn from this one
    /// service-wide counter. A generation therefore identifies one watcher's
    /// state at one moment and is never handed to another watcher: an
    /// in-flight poll or re-watch that captured an older generation can never
    /// match the watcher that replaced the one it was reading for.
    next_generation: AtomicU64,
}

impl WatchRegistry {
    fn allocate_generation(&self) -> u64 {
        self.next_generation.fetch_add(1, Ordering::Relaxed)
    }
}

#[derive(Default)]
struct WatchedPr {
    /// Shared by every watcher: a rate limit applies to the account, not to
    /// one thread, so a new watch never clears it.
    paused_until: Option<Instant>,
    watchers: BTreeMap<String, ThreadWatch>,
}

#[derive(Default)]
struct ThreadWatch {
    /// This watcher's last successful read. `None` until its first read
    /// succeeds, so a watcher added while the poll is in flight starts from
    /// the current snapshot rather than replaying older news.
    baseline: Option<PrSnapshot>,
    /// This watcher was already told about the current head commit's failure.
    failure_seen: bool,
    comment_only_wakes: u32,
    failed_reads: u32,
    /// Consecutive wakes that never reached the thread.
    failed_wakes: u32,
    /// Bumped whenever the daemon applies or resets this watcher's snapshot.
    /// A `watch()` read that started before the bump must not write its older
    /// snapshot over the fresher one.
    generation: u64,
    /// The outside grant the watch was created under, when one was.
    grant: Option<WatchGrant>,
}

struct Wake {
    thread_id: ThreadId,
    message: String,
    ends_watch: bool,
    /// The watcher's generation when this wake was computed. Delivery spans a
    /// whole `send_message`, and a re-watch in that window creates a watcher
    /// this wake must not end or touch.
    generation: u64,
    /// The watcher's snapshot before this poll, kept so a failed wake can be
    /// retried by the next poll.
    previous: PrSnapshot,
    previous_failure_seen: bool,
    previous_comment_only_wakes: u32,
    /// The grant the watching thread holds for this work.
    grant: Option<WatchGrant>,
}

impl WatchedPr {
    /// Applies one successful snapshot and returns the wakes for every watcher.
    ///
    /// Each watcher compares the snapshot with its own baseline, so a watcher
    /// hears every change since it started watching exactly once, whatever
    /// other threads (or a re-watch) did to theirs.
    fn observe(
        &mut self,
        snapshot: PrSnapshot,
        viewer: Option<&str>,
        generations: &HashMap<String, u64>,
        next_generation: &AtomicU64,
    ) -> Vec<Wake> {
        let terminal = matches!(
            snapshot.summary.state,
            ForgePrState::Merged | ForgePrState::Closed
        );

        let mut wakes = Vec::new();
        let mut ended = Vec::new();
        for (thread, watch) in &mut self.watchers {
            // Only a watcher whose state is exactly what this read started
            // with may take its snapshot. A re-watch, a poll, or a failed-wake
            // retry that bumped the generation meanwhile has a newer state,
            // and applying this older body would move the baseline backwards.
            // A watcher added after the read started has no entry here: it
            // takes its own baseline from its own read.
            if generations.get(thread) != Some(&watch.generation) {
                tracing::debug!(
                    thread,
                    "discarding a PR poll snapshot a newer watch read overtook"
                );
                continue;
            }
            watch.generation = next_generation.fetch_add(1, Ordering::Relaxed);
            watch.failed_reads = 0;
            let failure_now = snapshot.checks.pending > 0 && snapshot.checks.failed > 0;
            let Some(previous) = watch.baseline.take() else {
                // This watcher started while the read was in flight. It
                // begins here: nothing before this moment is its news.
                watch.failure_seen = failure_now;
                watch.baseline = Some(snapshot.clone());
                if terminal {
                    ended.push(thread.clone());
                }
                continue;
            };
            let previous_failure_seen = watch.failure_seen;
            let previous_comment_only_wakes = watch.comment_only_wakes;
            let new_head = previous.summary.head_sha != snapshot.summary.head_sha;
            let prior_failure = if new_head {
                false
            } else {
                previous_failure_seen
            };
            let events = events_between(&previous, &snapshot, viewer, prior_failure);
            watch.failure_seen = failure_now;
            watch.baseline = Some(snapshot.clone());
            if events.is_empty() {
                if terminal {
                    // Already merged or closed when this watcher last looked:
                    // nothing more can happen, so the watch ends quietly.
                    ended.push(thread.clone());
                }
                continue;
            }
            let comment_only = events.iter().all(WatchEvent::is_comment_only);
            if comment_only {
                watch.comment_only_wakes += 1;
            } else {
                watch.comment_only_wakes = 0;
            }
            let last_comment =
                comment_only && watch.comment_only_wakes >= MAX_COMMENT_ONLY_WAKES && !terminal;
            // A comment-only cap is not applied here: the watcher stays until
            // the wake is delivered, so a failed final wake is retried like a
            // terminal one. `poll_once` removes it after a delivered wake.
            wakes.push(Wake {
                thread_id: ThreadId::new(thread.clone()),
                message: wake_message(&events, &snapshot),
                ends_watch: terminal || last_comment,
                generation: watch.generation,
                previous,
                previous_failure_seen,
                previous_comment_only_wakes,
                grant: watch.grant.clone(),
            });
        }
        for thread in ended {
            tracing::debug!(
                thread,
                "PR watch ended (PR finished or repeated comment-only wakes)"
            );
            self.watchers.remove(&thread);
        }

        wakes
    }
}

/// The state a watch compares against.
#[derive(Clone, Debug)]
struct PrSnapshot {
    summary: ForgePrSummary,
    mergeable: ForgeMergeable,
    checks: ForgeChecks,
    comments: ForgeCommentPage,
}

/// What changed between two snapshots that is worth waking the agent for.
#[derive(Clone, Debug, PartialEq)]
enum WatchEvent {
    CheckFailed {
        names: Vec<String>,
        url: String,
    },
    RequiredChecksPassed {
        url: String,
    },
    ChecksFinishedAfterFailure {
        passed: u32,
        failed: u32,
        url: String,
    },
    Comment {
        author: String,
        excerpt: String,
        url: String,
    },
    Review {
        author: String,
        state: ForgeReviewState,
        url: String,
    },
    Conflict {
        url: String,
    },
    Merged {
        url: String,
    },
    Closed {
        url: String,
    },
}

impl WatchEvent {
    fn is_comment_only(&self) -> bool {
        matches!(self, Self::Comment { .. } | Self::Review { .. })
    }
}

fn events_between(
    previous: &PrSnapshot,
    next: &PrSnapshot,
    viewer: Option<&str>,
    prior_failure: bool,
) -> Vec<WatchEvent> {
    let mut events = Vec::new();
    let url = if next.summary.web_url.is_empty() {
        next.summary.repo.pr_web_url(next.summary.number)
    } else {
        next.summary.web_url.clone()
    };

    match (previous.summary.state, next.summary.state) {
        (ForgePrState::Merged | ForgePrState::Closed, _) => {}
        (_, ForgePrState::Merged) => events.push(WatchEvent::Merged { url: url.clone() }),
        (_, ForgePrState::Closed) => events.push(WatchEvent::Closed { url: url.clone() }),
        _ => {}
    }

    if previous.mergeable != ForgeMergeable::Conflicting
        && next.mergeable == ForgeMergeable::Conflicting
        && !matches!(
            next.summary.state,
            ForgePrState::Merged | ForgePrState::Closed
        )
    {
        events.push(WatchEvent::Conflict { url: url.clone() });
    }

    let previous_failures = failure_keys(&previous.checks);
    // Only a failure this watcher has not seen is news. A shrinking set (a
    // re-run passed) is not a new failure, and a re-run that fails again
    // carries a new completion time, so it is.
    let new_failures: Vec<String> = failure_keys(&next.checks)
        .into_iter()
        .filter(|key| !previous_failures.contains(key))
        .collect();
    if !new_failures.is_empty() {
        let names = next
            .checks
            .checks
            .iter()
            .filter(|check| check.status.is_failure() && new_failures.contains(&failure_key(check)))
            .map(|check| check.name.clone())
            .collect();
        events.push(WatchEvent::CheckFailed {
            names,
            url: url.clone(),
        });
    }

    let previous_required = required_all_pass(&previous.checks);
    let next_required = required_all_pass(&next.checks);
    if previous_required == Some(false) && next_required == Some(true) {
        events.push(WatchEvent::RequiredChecksPassed { url: url.clone() });
    }

    if prior_failure && previous.checks.pending > 0 && next.checks.pending == 0 {
        events.push(WatchEvent::ChecksFinishedAfterFailure {
            passed: next.checks.passed,
            failed: next.checks.failed,
            url: url.clone(),
        });
    }

    // Comments and reviews are compared by id (plus author, so a provider
    // that reuses ids cannot hide one). Only someone other than the account
    // wakes the agent. An account with no known login cannot make that
    // distinction, so this watch skips both triggers rather than treating
    // every author as someone else and waking the agent for its own words.
    if let Some(viewer) = viewer {
        let known_comments = comment_keys(&previous.comments);
        for comment in &next.comments.comments {
            let key = format!("{}:{}", comment.comment_id, comment.author);
            if known_comments.contains(&key) || same_login(&comment.author, viewer) {
                continue;
            }
            events.push(WatchEvent::Comment {
                author: comment.author.clone(),
                excerpt: excerpt(&comment.body),
                url: comment.web_url.clone().unwrap_or_else(|| url.clone()),
            });
        }
        for thread in &next.comments.threads {
            for comment in &thread.comments {
                let key = format!("{}:{}", comment.comment_id, comment.author);
                if known_comments.contains(&key) || same_login(&comment.author, viewer) {
                    continue;
                }
                events.push(WatchEvent::Comment {
                    author: comment.author.clone(),
                    excerpt: excerpt(&comment.body),
                    url: comment.web_url.clone().unwrap_or_else(|| url.clone()),
                });
            }
        }

        let known_reviews = review_keys(&previous.comments);
        for review in &next.comments.reviews {
            let key = review_key(review);
            if known_reviews.contains(&key) || same_login(&review.author, viewer) {
                continue;
            }
            events.push(WatchEvent::Review {
                author: review.author.clone(),
                state: review.state,
                url: url.clone(),
            });
        }
    }

    events
}

fn comment_keys(page: &ForgeCommentPage) -> HashSet<String> {
    let mut keys = HashSet::new();
    for comment in &page.comments {
        keys.insert(format!("{}:{}", comment.comment_id, comment.author));
    }
    for thread in &page.threads {
        for comment in &thread.comments {
            keys.insert(format!("{}:{}", comment.comment_id, comment.author));
        }
    }
    keys
}

fn review_keys(page: &ForgeCommentPage) -> HashSet<String> {
    page.reviews.iter().map(review_key).collect()
}

fn review_key(review: &h4rn_protocol::ForgeReview) -> String {
    format!(
        "{}:{:?}:{}",
        review.author,
        review.state,
        review
            .submitted_at
            .map(|at| at.to_rfc3339())
            .unwrap_or_default()
    )
}

/// A stable fingerprint of one failing check, including completion time so a
/// re-run that fails again is new news.
fn failure_key(check: &ForgeCheck) -> String {
    format!(
        "{}:{:?}:{}",
        check.name,
        check.status,
        check
            .completed_at
            .map(|at| at.to_rfc3339())
            .unwrap_or_default()
    )
}

/// The fingerprint of every failing check, sorted.
fn failure_keys(checks: &ForgeChecks) -> Vec<String> {
    let mut keys: Vec<String> = checks
        .checks
        .iter()
        .filter(|check| check.status.is_failure())
        .map(failure_key)
        .collect();
    keys.sort();
    keys
}

/// `Some(true)` when every required check passed, `Some(false)` when a
/// required check is not passing, `None` when the forge cannot say which
/// checks are required (or names none).
fn required_all_pass(checks: &ForgeChecks) -> Option<bool> {
    let required: Vec<_> = checks
        .checks
        .iter()
        .filter(|check| check.required == Some(true))
        .collect();
    if required.is_empty() {
        return None;
    }
    Some(
        checks.state != ForgeChecksState::Unknown
            && required.iter().all(|check| check.status.is_success()),
    )
}

fn same_login(author: &str, viewer: &str) -> bool {
    !viewer.is_empty() && author.eq_ignore_ascii_case(viewer)
}

fn excerpt(body: &str) -> String {
    let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 160 {
        format!("{}…", flat.chars().take(160).collect::<String>())
    } else {
        flat
    }
}

/// A plain-language message that names what changed, with links.
fn wake_message(events: &[WatchEvent], snapshot: &PrSnapshot) -> String {
    let number = snapshot.summary.number;
    let title = &snapshot.summary.title;
    let mut lines = Vec::new();
    for event in events {
        lines.push(match event {
            WatchEvent::CheckFailed { names, url } => format!(
                "Checks failed on PR #{number} “{title}”: {}. {url}",
                names.join(", ")
            ),
            WatchEvent::RequiredChecksPassed { url } => format!(
                "The required checks passed on PR #{number} “{title}”. {url}"
            ),
            WatchEvent::ChecksFinishedAfterFailure {
                passed,
                failed,
                url,
            } => format!(
                "The other checks on PR #{number} “{title}” finished: {passed} passed, {failed} failed. {url}"
            ),
            WatchEvent::Comment {
                author,
                excerpt,
                url,
            } => format!("{author} commented on PR #{number} “{title}”: “{excerpt}”. {url}"),
            WatchEvent::Review {
                author,
                state,
                url,
            } => format!(
                "{author} left a review on PR #{number} “{title}” ({}). {url}",
                review_label(*state)
            ),
            WatchEvent::Conflict { url } => {
                format!("PR #{number} “{title}” now has merge conflicts. {url}")
            }
            WatchEvent::Merged { url } => {
                format!("PR #{number} “{title}” was merged. {url} The watch has ended.")
            }
            WatchEvent::Closed { url } => {
                format!(
                    "PR #{number} “{title}” was closed without merging. {url} The watch has ended."
                )
            }
        });
    }
    lines.join("\n")
}

fn review_label(state: ForgeReviewState) -> &'static str {
    match state {
        ForgeReviewState::Approved => "approved",
        ForgeReviewState::ChangesRequested => "changes requested",
        ForgeReviewState::Commented => "commented",
        ForgeReviewState::Pending => "review pending",
        ForgeReviewState::Dismissed => "review dismissed",
    }
}

/// Maps the daemon's forge service onto the watch's source shape.
pub struct ForgePrSource {
    forge: Arc<crate::forge::ForgeService>,
}

impl ForgePrSource {
    /// Wraps the daemon's forge service.
    pub fn new(forge: Arc<crate::forge::ForgeService>) -> Self {
        Self { forge }
    }
}

#[async_trait]
impl PrSource for ForgePrSource {
    async fn resolve_account(
        &self,
        account_id: Option<&str>,
        repo: &ForgeRepoRef,
    ) -> Result<String, PrSourceError> {
        self.forge
            .resolve_account_id(account_id, repo)
            .map_err(PrSourceError::from)
    }

    fn account_login(&self, account_id: &str) -> Option<String> {
        self.forge
            .list_accounts()
            .0
            .into_iter()
            .find(|account: &ForgeAccount| account.account_id == account_id)
            .and_then(|account| account.username)
    }

    async fn get_pr(
        &self,
        account_id: &str,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<ForgePullRequest, PrSourceError> {
        self.forge
            .get_pr_quiet(Some(account_id), repo, number)
            .await
            .map_err(PrSourceError::from)
    }

    async fn list_comments(
        &self,
        account_id: &str,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<ForgeCommentPage, PrSourceError> {
        self.forge
            .list_comments_quiet(Some(account_id), repo, number)
            .await
            .map_err(PrSourceError::from)
    }

    async fn list_checks(
        &self,
        account_id: &str,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<ForgeChecks, PrSourceError> {
        self.forge
            .list_checks_quiet(Some(account_id), repo, number)
            .await
            .map_err(PrSourceError::from)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet, VecDeque};

    use chrono::TimeZone;
    use h4rn_protocol::{
        ForgeCheck, ForgeCheckStatus, ForgeComment, ForgeRerunSupport, ForgeReview,
    };

    use super::*;
    use crate::journal::SqliteJournal;

    fn at(seconds: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, seconds)
            .single()
            .expect("valid time")
    }

    fn repo() -> ForgeRepoRef {
        ForgeRepoRef {
            forge: h4rn_protocol::ForgeKind::GitHub,
            base_url: "https://github.com".into(),
            owner: "h4rn".into(),
            repo: "api".into(),
        }
    }

    fn thread(name: &str) -> ThreadId {
        ThreadId::new(name)
    }

    fn summary(state: ForgePrState, head: &str) -> ForgePrSummary {
        ForgePrSummary {
            repo: repo(),
            number: 7,
            title: "Add PR watch".into(),
            state,
            author: "alice".into(),
            head_branch: "h4rn/pr-watch".into(),
            base_branch: "main".into(),
            head_sha: Some(head.into()),
            updated_at: at(1),
            web_url: "https://github.com/h4rn/api/pull/7".into(),
            checks: ForgeChecksState::Unknown,
            review: ForgeReviewState::Commented,
            draft: false,
        }
    }

    fn detail(state: ForgePrState, head: &str, mergeable: ForgeMergeable) -> ForgePullRequest {
        ForgePullRequest {
            summary: summary(state, head),
            body: String::new(),
            labels: Vec::new(),
            mergeable,
            base_sha: Some("base".into()),
            additions: 1,
            deletions: 0,
            changed_files: 1,
            changed_files_known: true,
            line_counts_known: true,
            commits: 1,
            created_at: at(0),
            reviewers: Vec::new(),
            merge_commit_sha: None,
        }
    }

    fn check(
        name: &str,
        status: ForgeCheckStatus,
        required: Option<bool>,
        completed: Option<u32>,
    ) -> ForgeCheck {
        ForgeCheck {
            name: name.into(),
            status,
            details_url: None,
            description: None,
            started_at: Some(at(0)),
            completed_at: completed.map(at),
            duration_ms: None,
            required,
        }
    }

    fn checks(head: &str, checks: Vec<ForgeCheck>) -> ForgeChecks {
        let total = checks.len() as u32;
        let passed = checks
            .iter()
            .filter(|check| check.status.is_success())
            .count() as u32;
        let failed = checks
            .iter()
            .filter(|check| check.status.is_failure())
            .count() as u32;
        let pending = checks
            .iter()
            .filter(|check| check.status.is_pending())
            .count() as u32;
        let state = if failed > 0 {
            ForgeChecksState::Failing
        } else if pending > 0 {
            ForgeChecksState::Pending
        } else if total > 0 {
            ForgeChecksState::Passing
        } else {
            ForgeChecksState::Unknown
        };
        ForgeChecks {
            repo: repo(),
            number: 7,
            head_sha: head.into(),
            state,
            total,
            passed,
            failed,
            pending,
            checks,
            rerun: ForgeRerunSupport {
                supported: true,
                reason: None,
            },
            updated_at: at(1),
        }
    }

    fn comments(comments: Vec<ForgeComment>, reviews: Vec<ForgeReview>) -> ForgeCommentPage {
        ForgeCommentPage {
            repo: repo(),
            number: 7,
            comments,
            threads: Vec::new(),
            reviews,
            has_more: false,
        }
    }

    fn comment(id: &str, author: &str, body: &str) -> ForgeComment {
        ForgeComment {
            comment_id: id.into(),
            author: author.into(),
            body: body.into(),
            created_at: at(2),
            updated_at: None,
            web_url: Some(format!(
                "https://github.com/h4rn/api/pull/7#issuecomment-{id}"
            )),
        }
    }

    struct MockState {
        pr: ForgePullRequest,
        checks: ForgeChecks,
        comments: ForgeCommentPage,
        error: Option<PrSourceError>,
        get_pr_calls: usize,
        check_calls: usize,
    }

    struct MockSource {
        state: Mutex<MockState>,
        logins: Mutex<HashMap<String, String>>,
        /// Checks reads held between a watch's or poll's start and this test's
        /// release, so another read can overtake them. Served in call order.
        gated_checks: Mutex<VecDeque<GatedChecks>>,
    }

    struct GatedChecks {
        value: ForgeChecks,
        started: tokio::sync::oneshot::Sender<()>,
        release: tokio::sync::oneshot::Receiver<()>,
    }

    impl MockSource {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                state: Mutex::new(MockState {
                    pr: detail(ForgePrState::Open, "sha-1", ForgeMergeable::Mergeable),
                    checks: checks(
                        "sha-1",
                        vec![check(
                            "build",
                            ForgeCheckStatus::InProgress,
                            Some(true),
                            None,
                        )],
                    ),
                    comments: comments(Vec::new(), Vec::new()),
                    error: None,
                    get_pr_calls: 0,
                    check_calls: 0,
                }),
                logins: Mutex::new(HashMap::from([("acct_1".into(), "me".into())])),
                gated_checks: Mutex::new(VecDeque::new()),
            })
        }

        /// Holds the next checks read with the current body until released.
        ///
        /// Each call queues one read; reads take their gates in call order.
        /// The caller gets a signal when that read is waiting and a sender
        /// that lets it finish, so another read can be run in between.
        fn gate_next_checks(
            &self,
        ) -> (
            tokio::sync::oneshot::Receiver<()>,
            tokio::sync::oneshot::Sender<()>,
        ) {
            let (started, started_rx) = tokio::sync::oneshot::channel();
            let (release, release_rx) = tokio::sync::oneshot::channel();
            let value = self.state().checks.clone();
            self.gated_checks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push_back(GatedChecks {
                    value,
                    started,
                    release: release_rx,
                });
            (started_rx, release)
        }

        fn state(&self) -> std::sync::MutexGuard<'_, MockState> {
            self.state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }

        fn set(&self, update: impl FnOnce(&mut MockState)) {
            update(&mut self.state());
        }

        fn get_pr_calls(&self) -> usize {
            self.state().get_pr_calls
        }

        fn check_calls(&self) -> usize {
            self.state().check_calls
        }

        fn reset_calls(&self) {
            let mut state = self.state();
            state.get_pr_calls = 0;
            state.check_calls = 0;
        }

        /// Models an account whose forge never reported a login.
        fn forget_logins(&self) {
            self.logins
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clear();
        }
    }

    #[async_trait]
    impl PrSource for MockSource {
        async fn resolve_account(
            &self,
            _account_id: Option<&str>,
            _repo: &ForgeRepoRef,
        ) -> Result<String, PrSourceError> {
            Ok("acct_1".into())
        }

        fn account_login(&self, account_id: &str) -> Option<String> {
            self.logins
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(account_id)
                .cloned()
        }

        async fn get_pr(
            &self,
            _account_id: &str,
            _repo: &ForgeRepoRef,
            _number: u32,
        ) -> Result<ForgePullRequest, PrSourceError> {
            let mut state = self.state();
            state.get_pr_calls += 1;
            if let Some(error) = &state.error {
                return Err(error.clone());
            }
            Ok(state.pr.clone())
        }

        async fn list_comments(
            &self,
            _account_id: &str,
            _repo: &ForgeRepoRef,
            _number: u32,
        ) -> Result<ForgeCommentPage, PrSourceError> {
            let state = self.state();
            if let Some(error) = &state.error {
                return Err(error.clone());
            }
            Ok(state.comments.clone())
        }

        async fn list_checks(
            &self,
            _account_id: &str,
            _repo: &ForgeRepoRef,
            _number: u32,
        ) -> Result<ForgeChecks, PrSourceError> {
            let gate = self
                .gated_checks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .pop_front();
            if let Some(GatedChecks {
                value,
                started,
                release,
            }) = gate
            {
                let _ = started.send(());
                let _ = release.await;
                return Ok(value);
            }
            let mut state = self.state();
            state.check_calls += 1;
            if let Some(error) = &state.error {
                return Err(error.clone());
            }
            Ok(state.checks.clone())
        }
    }

    #[derive(Default)]
    struct MockSink {
        active: Mutex<HashSet<String>>,
        titles: Mutex<HashMap<String, String>>,
        wakes: Mutex<Vec<(String, String)>>,
        fail_wakes: Mutex<usize>,
        /// One specific wake attempt (1-based) that fails.
        fail_wake_at: Mutex<Option<usize>>,
        wake_attempts: Mutex<usize>,
        /// The next wake held between its start and this test's release, so a
        /// re-watch can complete while delivery is in flight.
        gated_wake: Mutex<Option<GatedWake>>,
    }

    struct GatedWake {
        started: tokio::sync::oneshot::Sender<()>,
        release: tokio::sync::oneshot::Receiver<()>,
    }

    impl MockSink {
        fn add(&self, thread_id: &ThreadId) {
            self.active
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(thread_id.as_str().to_owned());
            self.titles
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(thread_id.as_str().to_owned(), "Watch the PR".into());
        }

        fn deactivate(&self, thread_id: &ThreadId) {
            self.active
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(thread_id.as_str());
        }

        fn wakes(&self) -> Vec<(String, String)> {
            self.wakes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }

        fn fail_next_wakes(&self, count: usize) {
            *self
                .fail_wakes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = count;
        }

        /// Fails exactly the given wake attempt (1-based).
        fn fail_wake_at(&self, attempt: usize) {
            *self
                .fail_wake_at
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(attempt);
        }

        /// Holds the next wake until the returned sender releases it, and
        /// signals the returned receiver when the delivery is waiting.
        fn gate_next_wake(
            &self,
        ) -> (
            tokio::sync::oneshot::Receiver<()>,
            tokio::sync::oneshot::Sender<()>,
        ) {
            let (started, started_rx) = tokio::sync::oneshot::channel();
            let (release, release_rx) = tokio::sync::oneshot::channel();
            *self
                .gated_wake
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(GatedWake {
                started,
                release: release_rx,
            });
            (started_rx, release)
        }
    }

    #[async_trait]
    impl PrWakeSink for MockSink {
        async fn thread_title(&self, thread_id: &ThreadId) -> Option<String> {
            self.titles
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(thread_id.as_str())
                .cloned()
        }

        async fn is_active(&self, thread_id: &ThreadId) -> bool {
            self.active
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(thread_id.as_str())
        }

        async fn wake(
            &self,
            thread_id: &ThreadId,
            message: String,
            _grant: Option<WatchGrant>,
        ) -> Result<(), WakeError> {
            let gate = self
                .gated_wake
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            if let Some(GatedWake { started, release }) = gate {
                let _ = started.send(());
                let _ = release.await;
            }
            let attempt = {
                let mut attempts = self
                    .wake_attempts
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                *attempts += 1;
                *attempts
            };
            let fail_here = *self
                .fail_wake_at
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                == Some(attempt);
            let mut remaining = self
                .fail_wakes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if *remaining > 0 {
                *remaining -= 1;
                return Err(WakeError::Retry("the thread could not be woken".into()));
            }
            if fail_here {
                return Err(WakeError::Retry("the thread could not be woken".into()));
            }
            self.wakes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push((thread_id.as_str().to_owned(), message));
            Ok(())
        }
    }

    async fn service() -> (Arc<PrLinkService>, Arc<MockSource>, Arc<MockSink>) {
        let journal: Arc<dyn Journal> =
            Arc::new(SqliteJournal::in_memory().await.expect("in-memory journal"));
        service_with_journal(journal)
    }

    fn service_with_journal(
        journal: Arc<dyn Journal>,
    ) -> (Arc<PrLinkService>, Arc<MockSource>, Arc<MockSink>) {
        let source = MockSource::new();
        let sink = Arc::new(MockSink::default());
        let service = PrLinkService::new(journal, source.clone(), sink.clone());
        (service, source, sink)
    }

    async fn watched(service: &Arc<PrLinkService>, sink: &Arc<MockSink>, thread_id: &ThreadId) {
        sink.add(thread_id);
        service
            .link_known(thread_id, "acct_1", &repo(), 7)
            .await
            .expect("link");
        service.watch(thread_id, &repo(), 7).await.expect("watch");
    }

    #[tokio::test]
    async fn links_survive_a_restart() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("journal.db");
        let thread_id = thread("th_link");
        {
            let journal: Arc<dyn Journal> =
                Arc::new(SqliteJournal::connect(&path).await.expect("journal"));
            let (service, _source, sink) = service_with_journal(journal);
            sink.add(&thread_id);
            let pull = service
                .link(&thread_id, None, &repo(), 7)
                .await
                .expect("link");
            assert_eq!(pull.number, 7);
            assert_eq!(pull.summary.expect("summary").title, "Add PR watch");
        }

        let journal: Arc<dyn Journal> =
            Arc::new(SqliteJournal::connect(&path).await.expect("reopen journal"));
        let (service, _source, sink) = service_with_journal(journal);
        sink.add(&thread_id);
        let pulls = service.list(&thread_id).await.expect("list after restart");
        assert_eq!(pulls.len(), 1);
        assert_eq!(pulls[0].repo, repo());
        assert_eq!(pulls[0].number, 7);
        assert!(pulls[0].summary.is_some());
        assert!(!pulls[0].watching);
    }

    #[tokio::test]
    async fn a_link_can_be_removed_and_is_reverse_listed() {
        let (service, _source, sink) = service().await;
        let thread_id = thread("th_link");
        sink.add(&thread_id);
        service
            .link(&thread_id, None, &repo(), 7)
            .await
            .expect("link");
        let threads = service.list_for_pr(&repo(), 7).await.expect("reverse");
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].thread_id, thread_id);
        assert_eq!(threads[0].title, "Watch the PR");

        service
            .unlink(&thread_id, &repo(), 7)
            .await
            .expect("unlink");
        assert!(service.list(&thread_id).await.expect("list").is_empty());
        assert!(
            service
                .list_for_pr(&repo(), 7)
                .await
                .expect("reverse")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn failed_checks_wake_once_with_a_link() {
        let (service, source, sink) = service().await;
        let thread_id = thread("th_checks");
        watched(&service, &sink, &thread_id).await;

        source.set(|state| {
            state.checks = checks(
                "sha-1",
                vec![check(
                    "build",
                    ForgeCheckStatus::Failure,
                    Some(true),
                    Some(3),
                )],
            );
        });
        service.poll_once().await;
        let wakes = sink.wakes();
        assert_eq!(wakes.len(), 1, "{wakes:?}");
        assert!(wakes[0].1.contains("Checks failed"));
        assert!(wakes[0].1.contains("build"));
        assert!(wakes[0].1.contains("https://github.com/h4rn/api/pull/7"));

        // The same state is not news twice.
        service.poll_once().await;
        assert_eq!(sink.wakes().len(), 1);
    }

    #[tokio::test]
    async fn passing_required_checks_wake_once() {
        let (service, source, sink) = service().await;
        let thread_id = thread("th_required");
        watched(&service, &sink, &thread_id).await;

        source.set(|state| {
            state.checks = checks(
                "sha-1",
                vec![check(
                    "build",
                    ForgeCheckStatus::Success,
                    Some(true),
                    Some(3),
                )],
            );
        });
        service.poll_once().await;
        let wakes = sink.wakes();
        assert_eq!(wakes.len(), 1, "{wakes:?}");
        assert!(wakes[0].1.contains("required checks passed"));

        service.poll_once().await;
        assert_eq!(sink.wakes().len(), 1);
    }

    #[tokio::test]
    async fn required_checks_are_skipped_when_the_forge_cannot_say() {
        let (service, source, sink) = service().await;
        let thread_id = thread("th_required_unknown");
        watched(&service, &sink, &thread_id).await;

        source.set(|state| {
            state.checks = checks(
                "sha-1",
                vec![check("build", ForgeCheckStatus::Success, None, Some(3))],
            );
        });
        service.poll_once().await;
        assert!(sink.wakes().is_empty());
    }

    #[tokio::test]
    async fn other_checks_finishing_after_a_failure_wake_once() {
        let (service, source, sink) = service().await;
        let thread_id = thread("th_finish");
        source.set(|state| {
            state.checks = checks(
                "sha-1",
                vec![
                    check("build", ForgeCheckStatus::InProgress, Some(true), None),
                    check("test", ForgeCheckStatus::InProgress, Some(false), None),
                ],
            );
        });
        watched(&service, &sink, &thread_id).await;

        source.set(|state| {
            state.checks = checks(
                "sha-1",
                vec![
                    check("build", ForgeCheckStatus::Failure, Some(true), Some(3)),
                    check("test", ForgeCheckStatus::InProgress, Some(false), None),
                ],
            );
        });
        service.poll_once().await;
        assert_eq!(sink.wakes().len(), 1);
        assert!(sink.wakes()[0].1.contains("Checks failed"));

        source.set(|state| {
            state.checks = checks(
                "sha-1",
                vec![
                    check("build", ForgeCheckStatus::Failure, Some(true), Some(3)),
                    check("test", ForgeCheckStatus::Success, Some(false), Some(4)),
                ],
            );
        });
        service.poll_once().await;
        let wakes = sink.wakes();
        assert_eq!(wakes.len(), 2, "{wakes:?}");
        assert!(wakes[1].1.contains("other checks"));
        assert!(wakes[1].1.contains("1 passed, 1 failed"));

        service.poll_once().await;
        assert_eq!(sink.wakes().len(), 2);
    }

    #[tokio::test]
    async fn comments_and_reviews_from_others_wake_once_but_own_do_not() {
        let (service, source, sink) = service().await;
        let thread_id = thread("th_comments");
        watched(&service, &sink, &thread_id).await;

        source.set(|state| {
            state.comments = comments(
                vec![comment("c1", "alice", "Looks good")],
                vec![ForgeReview {
                    author: "bob".into(),
                    state: ForgeReviewState::ChangesRequested,
                    submitted_at: Some(at(5)),
                    body: Some("Please add a test".into()),
                }],
            );
        });
        service.poll_once().await;
        let wakes = sink.wakes();
        assert_eq!(wakes.len(), 1, "{wakes:?}");
        assert!(wakes[0].1.contains("alice commented"));
        assert!(wakes[0].1.contains("bob left a review"));
        assert!(wakes[0].1.contains("changes requested"));

        // My own comment is never news.
        source.set(|state| {
            state.comments = comments(
                vec![
                    comment("c1", "alice", "Looks good"),
                    comment("c2", "me", "Thanks"),
                ],
                Vec::new(),
            );
        });
        service.poll_once().await;
        assert_eq!(sink.wakes().len(), 1);

        // The same comments are not reported again.
        service.poll_once().await;
        assert_eq!(sink.wakes().len(), 1);
    }

    #[tokio::test]
    async fn a_new_conflict_wakes_once() {
        let (service, source, sink) = service().await;
        let thread_id = thread("th_conflict");
        watched(&service, &sink, &thread_id).await;

        source.set(|state| {
            state.pr = detail(ForgePrState::Open, "sha-1", ForgeMergeable::Conflicting);
        });
        service.poll_once().await;
        assert_eq!(sink.wakes().len(), 1);
        assert!(sink.wakes()[0].1.contains("merge conflicts"));

        service.poll_once().await;
        assert_eq!(sink.wakes().len(), 1);
    }

    #[tokio::test]
    async fn merging_wakes_once_and_ends_the_watch() {
        let (service, source, sink) = service().await;
        let thread_id = thread("th_merge");
        watched(&service, &sink, &thread_id).await;

        source.set(|state| {
            state.pr = detail(ForgePrState::Merged, "sha-1", ForgeMergeable::Mergeable);
        });
        service.poll_once().await;
        let wakes = sink.wakes();
        assert_eq!(wakes.len(), 1, "{wakes:?}");
        assert!(wakes[0].1.contains("was merged"));
        assert!(wakes[0].1.contains("watch has ended"));
        assert!(service.watching_threads(&repo(), 7).is_empty());

        source.set(|state| {
            state.comments = comments(vec![comment("c9", "alice", "after")], Vec::new());
        });
        service.poll_once().await;
        assert_eq!(sink.wakes().len(), 1);
        // The link survives the watch ending.
        assert_eq!(service.list(&thread_id).await.expect("list").len(), 1);
    }

    #[tokio::test]
    async fn watching_a_pr_that_is_already_merged_ends_at_once() {
        let (service, source, sink) = service().await;
        let thread_id = thread("th_merged_first");
        source.set(|state| {
            state.pr = detail(ForgePrState::Merged, "sha-1", ForgeMergeable::Mergeable);
        });
        watched(&service, &sink, &thread_id).await;

        // Nothing can change on a merged PR, so the watch never starts.
        assert!(service.watching_threads(&repo(), 7).is_empty());
        service.poll_once().await;
        assert!(sink.wakes().is_empty());
        assert_eq!(service.list(&thread_id).await.expect("list").len(), 1);
    }

    #[tokio::test]
    async fn closing_wakes_and_ends_the_watch() {
        let (service, source, sink) = service().await;
        let thread_id = thread("th_close");
        watched(&service, &sink, &thread_id).await;

        source.set(|state| {
            state.pr = detail(ForgePrState::Closed, "sha-1", ForgeMergeable::Mergeable);
        });
        service.poll_once().await;
        let wakes = sink.wakes();
        assert_eq!(wakes.len(), 1);
        assert!(wakes[0].1.contains("closed without merging"));
        assert!(service.watching_threads(&repo(), 7).is_empty());
    }

    #[tokio::test]
    async fn ten_comment_only_wakes_end_the_watch() {
        let (service, source, sink) = service().await;
        let thread_id = thread("th_comments_end");
        watched(&service, &sink, &thread_id).await;

        for index in 0..MAX_COMMENT_ONLY_WAKES {
            source.set(|state| {
                state.comments = comments(
                    vec![comment(
                        &format!("c{index}"),
                        "alice",
                        &format!("comment {index}"),
                    )],
                    Vec::new(),
                );
            });
            service.poll_once().await;
        }
        assert_eq!(sink.wakes().len(), MAX_COMMENT_ONLY_WAKES as usize);
        assert!(service.watching_threads(&repo(), 7).is_empty());
    }

    #[tokio::test]
    async fn eight_failed_reads_end_the_watch() {
        let (service, source, sink) = service().await;
        let thread_id = thread("th_fail_reads");
        watched(&service, &sink, &thread_id).await;

        source.set(|state| {
            state.error = Some(PrSourceError::Other("network down".into()));
        });
        for _ in 0..MAX_FAILED_READS {
            service.poll_once().await;
        }
        assert!(service.watching_threads(&repo(), 7).is_empty());
        assert!(sink.wakes().is_empty());
    }

    #[tokio::test]
    async fn a_rate_limit_pauses_instead_of_counting_failed_reads() {
        let (service, source, sink) = service().await;
        let thread_id = thread("th_rate_limit");
        watched(&service, &sink, &thread_id).await;

        source.set(|state| {
            state.error = Some(PrSourceError::RateLimited {
                retry_after: Some(60),
            });
        });
        for _ in 0..(MAX_FAILED_READS + 4) {
            service.poll_once().await;
        }
        assert_eq!(service.watching_threads(&repo(), 7).len(), 1);
        assert!(sink.wakes().is_empty());
    }

    #[tokio::test]
    async fn two_threads_on_one_pr_share_one_poll() {
        let (service, source, sink) = service().await;
        let first = thread("th_shared_a");
        let second = thread("th_shared_b");
        watched(&service, &sink, &first).await;
        watched(&service, &sink, &second).await;

        source.reset_calls();
        source.set(|state| {
            state.checks = checks(
                "sha-1",
                vec![check(
                    "build",
                    ForgeCheckStatus::Failure,
                    Some(true),
                    Some(3),
                )],
            );
        });
        service.poll_once().await;

        assert_eq!(source.get_pr_calls(), 1);
        assert_eq!(source.check_calls(), 1);
        let wakes = sink.wakes();
        assert_eq!(wakes.len(), 2, "{wakes:?}");
        assert_eq!(
            wakes
                .iter()
                .map(|(thread, _)| thread.as_str())
                .collect::<HashSet<_>>(),
            HashSet::from(["th_shared_a", "th_shared_b"])
        );
    }

    #[tokio::test]
    async fn a_new_watcher_does_not_swallow_the_change_an_older_watcher_has_not_seen() {
        let (service, source, sink) = service().await;
        let first = thread("th_per_watch_a");
        let second = thread("th_per_watch_b");
        watched(&service, &sink, &first).await;

        // The check fails after the first watcher's baseline.
        source.set(|state| {
            state.checks = checks(
                "sha-1",
                vec![check(
                    "build",
                    ForgeCheckStatus::Failure,
                    Some(true),
                    Some(3),
                )],
            );
        });

        // A second thread starts watching before the next poll. It starts from
        // the current snapshot, but it must not take the first thread's
        // baseline with it.
        watched(&service, &sink, &second).await;
        service.poll_once().await;

        let wakes = sink.wakes();
        assert_eq!(wakes.len(), 1, "{wakes:?}");
        assert_eq!(wakes[0].0, "th_per_watch_a");
        assert!(wakes[0].1.contains("Checks failed"));

        // Each watcher hears it exactly once.
        service.poll_once().await;
        assert_eq!(sink.wakes().len(), 1);
    }

    #[tokio::test]
    async fn re_watching_from_one_thread_leaves_the_other_threads_baseline_alone() {
        let (service, source, sink) = service().await;
        let first = thread("th_rewatch_a");
        let second = thread("th_rewatch_b");
        watched(&service, &sink, &first).await;
        watched(&service, &sink, &second).await;

        source.set(|state| {
            state.checks = checks(
                "sha-1",
                vec![check(
                    "build",
                    ForgeCheckStatus::Failure,
                    Some(true),
                    Some(3),
                )],
            );
        });

        // The first thread watches again before the poll. Its fresh baseline
        // starts now, but the second thread still has not seen the failure.
        service.watch(&first, &repo(), 7).await.expect("re-watch");
        service.poll_once().await;

        let wakes = sink.wakes();
        assert_eq!(wakes.len(), 1, "{wakes:?}");
        assert_eq!(wakes[0].0, "th_rewatch_b");

        // The re-watch does not repeat news for the thread that restarted.
        service.poll_once().await;
        assert_eq!(sink.wakes().len(), 1);
    }

    #[tokio::test]
    async fn another_threads_watch_does_not_clear_a_rate_limit_pause() {
        let (service, source, sink) = service().await;
        let first = thread("th_pause_a");
        let second = thread("th_pause_b");
        watched(&service, &sink, &first).await;

        source.set(|state| {
            state.error = Some(PrSourceError::RateLimited {
                retry_after: Some(60),
            });
        });
        service.poll_once().await;

        watched(&service, &sink, &second).await;

        // The pause belongs to the account and the PR, not to one thread.
        source.set(|state| state.error = None);
        source.reset_calls();
        service.poll_once().await;
        assert_eq!(
            source.get_pr_calls(),
            0,
            "a new watch must not clear the poller's pause"
        );
    }

    #[tokio::test]
    async fn a_shrinking_failure_set_is_not_a_new_failure() {
        let (service, source, sink) = service().await;
        let thread_id = thread("th_shrink");
        source.set(|state| {
            state.checks = checks(
                "sha-1",
                vec![
                    check("build", ForgeCheckStatus::Failure, Some(false), Some(3)),
                    check("lint", ForgeCheckStatus::Failure, Some(false), Some(4)),
                ],
            );
        });
        watched(&service, &sink, &thread_id).await;

        // Lint was re-run and passed; build still fails. That is not a new
        // failure, so the agent is not woken again.
        source.set(|state| {
            state.checks = checks(
                "sha-1",
                vec![
                    check("build", ForgeCheckStatus::Failure, Some(false), Some(3)),
                    check("lint", ForgeCheckStatus::Success, Some(false), Some(5)),
                ],
            );
        });
        service.poll_once().await;
        assert!(sink.wakes().is_empty(), "{:?}", sink.wakes());
    }

    #[tokio::test]
    async fn a_failure_that_is_new_is_named_on_its_own() {
        let (service, source, sink) = service().await;
        let thread_id = thread("th_new_failure");
        source.set(|state| {
            state.checks = checks(
                "sha-1",
                vec![check(
                    "build",
                    ForgeCheckStatus::Failure,
                    Some(false),
                    Some(3),
                )],
            );
        });
        watched(&service, &sink, &thread_id).await;

        source.set(|state| {
            state.checks = checks(
                "sha-1",
                vec![
                    check("build", ForgeCheckStatus::Failure, Some(false), Some(3)),
                    check("lint", ForgeCheckStatus::Failure, Some(false), Some(5)),
                ],
            );
        });
        service.poll_once().await;
        let wakes = sink.wakes();
        assert_eq!(wakes.len(), 1, "{wakes:?}");
        assert!(wakes[0].1.contains("lint"), "{wakes:?}");
        assert!(!wakes[0].1.contains("build"), "{wakes:?}");
    }

    #[tokio::test]
    async fn an_unknown_login_skips_comment_and_review_triggers() {
        let (service, source, sink) = service().await;
        let thread_id = thread("th_no_login");
        source.forget_logins();
        watched(&service, &sink, &thread_id).await;

        source.set(|state| {
            state.comments = comments(
                vec![comment("c1", "alice", "Looks good")],
                vec![ForgeReview {
                    author: "bob".into(),
                    state: ForgeReviewState::ChangesRequested,
                    submitted_at: Some(at(5)),
                    body: Some("Please add a test".into()),
                }],
            );
        });
        service.poll_once().await;
        assert!(
            sink.wakes().is_empty(),
            "without a login every author would look like someone else: {:?}",
            sink.wakes()
        );

        // The linked-items panel can say what this watch cannot cover.
        let pulls = service.list(&thread_id).await.expect("list");
        let notice = pulls[0].watch_notice.as_deref().expect("watch notice");
        assert!(notice.contains("login"), "{notice}");

        // The other triggers are untouched: a merge still wakes the agent.
        source.set(|state| {
            state.pr = detail(ForgePrState::Merged, "sha-1", ForgeMergeable::Mergeable);
        });
        service.poll_once().await;
        assert_eq!(sink.wakes().len(), 1, "{:?}", sink.wakes());
        assert!(sink.wakes()[0].1.contains("was merged"));
    }

    #[tokio::test]
    async fn a_watch_event_wakes_a_real_thread_manager() {
        use h4rn_harness::FakeHarness;

        use crate::harnesses::StaticHarnessCatalog;
        use crate::threads::{NewThread, ThreadManagerOptions};

        let workspace = tempfile::tempdir().expect("temp dir");
        let (harness, sends) = FakeHarness::new().with_send_count();
        let host = h4rn_protocol::HostId::new("host_test");
        let journal: Arc<dyn Journal> =
            Arc::new(SqliteJournal::in_memory().await.expect("journal"));
        let manager = Arc::new(
            ThreadManager::open(
                journal.clone(),
                Arc::new(StaticHarnessCatalog::with(Arc::new(harness))),
                host.clone(),
                ThreadManagerOptions {
                    // The manager reacts only to this test's own calls.
                    tick_interval: Duration::from_secs(3600),
                    ..ThreadManagerOptions::default()
                },
                None,
            )
            .await
            .expect("manager"),
        );
        let created = manager
            .create_thread(NewThread {
                thread_id: None,
                parent_thread_id: None,
                subagent_id: None,
                host,
                harness: h4rn_protocol::HarnessKind::ClaudeCode,
                agent_id: None,
                workspace_path: workspace.path().display().to_string(),
                model: "fake-model".into(),
                prompt: "first".into(),
                attachments: Vec::new(),
                runtime_mode: h4rn_protocol::RuntimeMode::ApprovalRequired,
                reasoning_effort: None,
                fast: false,
                worktree: None,
            })
            .await
            .expect("create");

        let source = MockSource::new();
        let grants = Arc::new(
            crate::mcp_server::OAuthStore::open(&tempfile::tempdir().expect("tempdir").keep())
                .expect("grant store"),
        );
        let sink = Arc::new(ThreadWakeSink::new(manager, grants));
        let service = PrLinkService::new(journal, source.clone(), sink);
        service
            .link_known(&created.thread_id, "acct_1", &repo(), 7)
            .await
            .expect("link");
        service
            .watch(&created.thread_id, &repo(), 7)
            .await
            .expect("watch");

        source.set(|state| {
            state.checks = checks(
                "sha-1",
                vec![check(
                    "build",
                    ForgeCheckStatus::Failure,
                    Some(true),
                    Some(3),
                )],
            );
        });
        service.poll_once().await;

        // The wake appends a message and a turn runs: the first prompt and the
        // wake each reached the fake harness.
        tokio::time::timeout(Duration::from_secs(10), async {
            while sends.get() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the watch wake started a turn");
        assert!(sends.get() >= 2);
    }

    #[tokio::test]
    async fn an_archived_thread_stops_watching_but_keeps_its_link() {
        let (service, _source, sink) = service().await;
        let thread_id = thread("th_archive");
        watched(&service, &sink, &thread_id).await;

        sink.deactivate(&thread_id);
        service.poll_once().await;
        assert!(service.watching_threads(&repo(), 7).is_empty());
        assert_eq!(service.list(&thread_id).await.expect("list").len(), 1);
    }

    #[tokio::test]
    async fn watching_requires_a_link() {
        let (service, _source, sink) = service().await;
        let thread_id = thread("th_unlinked");
        sink.add(&thread_id);
        let error = service
            .watch(&thread_id, &repo(), 7)
            .await
            .expect_err("watching an unlinked PR must fail");
        assert!(matches!(error, PrLinkError::NotLinked(_)));
    }

    #[tokio::test]
    async fn relinking_after_unlinking_does_not_replay_old_news() {
        let (service, source, sink) = service().await;
        let thread_id = thread("th_relink");
        watched(&service, &sink, &thread_id).await;

        source.set(|state| {
            state.checks = checks(
                "sha-1",
                vec![check(
                    "build",
                    ForgeCheckStatus::Failure,
                    Some(true),
                    Some(3),
                )],
            );
        });
        service.poll_once().await;
        assert_eq!(sink.wakes().len(), 1);

        service.unwatch(&thread_id, &repo(), 7);
        service
            .unlink(&thread_id, &repo(), 7)
            .await
            .expect("unlink");
        watched(&service, &sink, &thread_id).await;
        service.poll_once().await;
        assert_eq!(sink.wakes().len(), 1, "re-watching must not repeat news");
    }

    #[tokio::test]
    async fn another_readers_cache_refresh_does_not_hide_a_merge() {
        // The account client's ETag cache is shared by every reader. Another
        // reader refreshes it after the PR merges; the watch's next poll gets
        // a 304 for the refreshed validator. Its snapshot must come from the
        // cached body (merged), not from the watch's own earlier read.
        use serde_json::json;
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        use h4rn_forge::Forge;

        let server = MockServer::start().await;
        let repo_ref = ForgeRepoRef {
            forge: h4rn_protocol::ForgeKind::GitHub,
            base_url: server.uri(),
            owner: "acme".into(),
            repo: "widget".into(),
        };
        let detail = |state: &str, merged: bool| {
            json!({
                "number": 7,
                "title": "Add PR watch",
                "state": state,
                "merged": merged,
                "merged_at": if merged { json!("2026-01-01T00:03:00Z") } else { json!(null) },
                "draft": false,
                "user": {"login": "alice"},
                "head": {"ref": "h4rn/pr-watch", "sha": "she-1", "repo": {"full_name": "acme/widget"}},
                "base": {"ref": "main", "sha": "base"},
                "updated_at": "2026-01-01T00:00:01Z",
                "html_url": "https://github.com/acme/widget/pull/7",
                "body": "",
                "mergeable": true,
                "additions": 1,
                "deletions": 0,
                "changed_files": 1,
                "commits": 1,
                "created_at": "2026-01-01T00:00:00Z"
            })
        };

        // Conditional mocks first: all of them match the path, and wiremock
        // keeps registration order for equal priorities.
        Mock::given(method("GET"))
            .and(path("/api/v3/repos/acme/widget/pulls/7"))
            .and(header("if-none-match", "\"v1\""))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(detail("closed", true))
                    .insert_header("etag", "\"v2\""),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v3/repos/acme/widget/pulls/7"))
            .and(header("if-none-match", "\"v2\""))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v3/repos/acme/widget/pulls/7"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(detail("open", false))
                    .insert_header("etag", "\"v1\""),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v3/repos/acme/widget/pulls/7/reviews"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;

        struct SharedClient {
            client: h4rn_forge::GitHubClient,
            checks: Mutex<ForgeChecks>,
        }

        #[async_trait]
        impl PrSource for SharedClient {
            async fn resolve_account(
                &self,
                _account_id: Option<&str>,
                _repo: &ForgeRepoRef,
            ) -> Result<String, PrSourceError> {
                Ok("acct_1".into())
            }

            fn account_login(&self, _account_id: &str) -> Option<String> {
                Some("me".into())
            }

            async fn get_pr(
                &self,
                _account_id: &str,
                repo: &ForgeRepoRef,
                number: u32,
            ) -> Result<ForgePullRequest, PrSourceError> {
                self.client
                    .get_pr(repo, number)
                    .await
                    .map_err(|error| PrSourceError::Other(error.to_string()))
            }

            async fn list_comments(
                &self,
                _account_id: &str,
                _repo: &ForgeRepoRef,
                _number: u32,
            ) -> Result<ForgeCommentPage, PrSourceError> {
                Ok(comments(Vec::new(), Vec::new()))
            }

            async fn list_checks(
                &self,
                _account_id: &str,
                _repo: &ForgeRepoRef,
                _number: u32,
            ) -> Result<ForgeChecks, PrSourceError> {
                Ok(self
                    .checks
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone())
            }
        }

        let source = Arc::new(SharedClient {
            // A token-shaped literal is avoided on purpose; GitHub accepts any
            // bearer token and secret scanners flag realistic fakes.
            client: h4rn_forge::GitHubClient::new(&server.uri(), "test-token", 120)
                .expect("client"),
            checks: Mutex::new(checks(
                "she-1",
                vec![check(
                    "build",
                    ForgeCheckStatus::InProgress,
                    Some(true),
                    None,
                )],
            )),
        });
        let sink = Arc::new(MockSink::default());
        let journal: Arc<dyn Journal> =
            Arc::new(SqliteJournal::in_memory().await.expect("journal"));
        let service = PrLinkService::new(journal, source.clone(), sink.clone());
        let thread_id = thread("th_shared_cache");
        sink.add(&thread_id);
        service
            .link_known(&thread_id, "acct_1", &repo_ref, 7)
            .await
            .expect("link");
        service
            .watch(&thread_id, &repo_ref, 7)
            .await
            .expect("watch");

        // Another reader (checks, the badge, the PR page) reads after the
        // merge and refreshes the account client's cache.
        let refreshed = source.client.get_pr(&repo_ref, 7).await.expect("read");
        assert_eq!(refreshed.summary.state, ForgePrState::Merged);

        service.poll_once().await;
        let wakes = sink.wakes();
        assert_eq!(wakes.len(), 1, "{wakes:?}");
        assert!(wakes[0].1.contains("was merged"), "{wakes:?}");
        assert!(service.watching_threads(&repo_ref, 7).is_empty());
    }

    #[tokio::test]
    async fn a_poll_that_overtakes_a_rewatch_does_not_move_the_baseline_back() {
        let (service, source, sink) = service().await;
        let thread_id = thread("th_rewatch_race");
        watched(&service, &sink, &thread_id).await;

        // The re-watch read holds the pre-failure checks while a poll runs.
        let (started, release) = source.gate_next_checks();
        let service_for_watch = Arc::clone(&service);
        let thread_for_watch = thread_id.clone();
        let rewatch = tokio::spawn(async move {
            service_for_watch
                .watch(&thread_for_watch, &repo(), 7)
                .await
                .expect("re-watch");
        });
        started.await.expect("the re-watch read is waiting");

        source.set(|state| {
            state.checks = checks(
                "sha-1",
                vec![check(
                    "build",
                    ForgeCheckStatus::Failure,
                    Some(true),
                    Some(3),
                )],
            );
        });
        service.poll_once().await;
        assert_eq!(sink.wakes().len(), 1, "{:?}", sink.wakes());

        // The held read now finishes with the older body. It must not replace
        // the post-failure baseline the poll just stored.
        release.send(()).expect("release the re-watch read");
        rewatch.await.expect("re-watch finished");

        service.poll_once().await;
        assert_eq!(
            sink.wakes().len(),
            1,
            "the failure is news once, not again after the re-watch: {:?}",
            sink.wakes()
        );
    }

    #[tokio::test]
    async fn a_rewatch_that_overtakes_a_poll_does_not_replay_a_delivered_failure() {
        let (service, source, sink) = service().await;
        let thread_id = thread("th_poll_race");
        watched(&service, &sink, &thread_id).await;

        // The failure is delivered once.
        source.set(|state| {
            state.checks = checks(
                "sha-1",
                vec![check(
                    "build",
                    ForgeCheckStatus::Failure,
                    Some(true),
                    Some(3),
                )],
            );
        });
        service.poll_once().await;
        assert_eq!(sink.wakes().len(), 1, "{:?}", sink.wakes());

        // A poll read starts with that failing body and is held in checks.
        let (started, release) = source.gate_next_checks();
        let service_for_poll = Arc::clone(&service);
        let poll = tokio::spawn(async move { service_for_poll.poll_once().await });
        started.await.expect("the poll read is waiting");

        // The checks pass and a re-watch reads that state to completion.
        source.set(|state| {
            state.checks = checks(
                "sha-1",
                vec![check(
                    "build",
                    ForgeCheckStatus::Success,
                    Some(true),
                    Some(4),
                )],
            );
        });
        service
            .watch(&thread_id, &repo(), 7)
            .await
            .expect("re-watch");

        // The held poll now finishes with the older, failing body. It must
        // not replace the fresh passing baseline and report the failure again.
        release.send(()).expect("release the poll read");
        poll.await.expect("poll finished");

        service.poll_once().await;
        assert_eq!(
            sink.wakes().len(),
            1,
            "a stale poll body must not re-report a delivered failure: {:?}",
            sink.wakes()
        );
    }

    #[tokio::test]
    async fn a_terminal_rewatch_does_not_delete_a_watcher_retrying_its_wake() {
        let (service, source, sink) = service().await;
        let thread_id = thread("th_terminal_rewatch");
        watched(&service, &sink, &thread_id).await;

        source.set(|state| {
            state.pr = detail(ForgePrState::Merged, "sha-1", ForgeMergeable::Mergeable);
        });

        // The re-watch read starts with the merged body and is held in checks.
        let (started, release) = source.gate_next_checks();
        let service_for_watch = Arc::clone(&service);
        let thread_for_watch = thread_id.clone();
        let rewatch = tokio::spawn(async move {
            service_for_watch
                .watch(&thread_for_watch, &repo(), 7)
                .await
                .expect("re-watch");
        });
        started.await.expect("the re-watch read is waiting");

        // A poll reports the merge, but its wake fails, so the watcher stays
        // alive to retry it.
        sink.fail_next_wakes(1);
        service.poll_once().await;
        assert!(sink.wakes().is_empty());
        assert_eq!(service.watching_threads(&repo(), 7).len(), 1);

        // The held re-watch now finishes. It must not delete the watcher that
        // is waiting to retry the undelivered merge.
        release.send(()).expect("release the re-watch read");
        rewatch.await.expect("re-watch finished");
        assert_eq!(
            service.watching_threads(&repo(), 7).len(),
            1,
            "a terminal re-watch must not delete a watcher retrying its wake"
        );

        // The retry delivers the merge exactly once and ends the watch.
        service.poll_once().await;
        let wakes = sink.wakes();
        assert_eq!(wakes.len(), 1, "{wakes:?}");
        assert!(wakes[0].1.contains("was merged"), "{wakes:?}");
        assert!(service.watching_threads(&repo(), 7).is_empty());

        service.poll_once().await;
        assert_eq!(sink.wakes().len(), 1);
    }

    #[tokio::test]
    async fn a_failed_tenth_comment_only_wake_is_retried() {
        let (service, source, sink) = service().await;
        let thread_id = thread("th_comment_retry");
        watched(&service, &sink, &thread_id).await;

        // Fail exactly the wake that reaches the comment-only cap.
        sink.fail_wake_at(MAX_COMMENT_ONLY_WAKES as usize);
        for index in 0..MAX_COMMENT_ONLY_WAKES {
            source.set(|state| {
                state.comments = comments(
                    vec![comment(
                        &format!("c{index}"),
                        "alice",
                        &format!("comment {index}"),
                    )],
                    Vec::new(),
                );
            });
            service.poll_once().await;
        }
        assert_eq!(
            sink.wakes().len(),
            (MAX_COMMENT_ONLY_WAKES - 1) as usize,
            "the failed final wake must not be delivered: {:?}",
            sink.wakes()
        );
        assert_eq!(
            service.watching_threads(&repo(), 7).len(),
            1,
            "a failed final comment wake must keep the watcher for its retry"
        );

        service.poll_once().await;
        let wakes = sink.wakes();
        assert_eq!(wakes.len(), MAX_COMMENT_ONLY_WAKES as usize, "{wakes:?}");
        assert!(
            wakes
                .last()
                .is_some_and(|(_, message)| message.contains("comment 9")),
            "{wakes:?}"
        );
        assert!(
            service.watching_threads(&repo(), 7).is_empty(),
            "the retried wake is the watch's last"
        );

        service.poll_once().await;
        assert_eq!(sink.wakes().len(), MAX_COMMENT_ONLY_WAKES as usize);
    }

    #[tokio::test]
    async fn a_watch_during_a_rate_limit_pause_spends_no_read() {
        let (service, source, sink) = service().await;
        let first = thread("th_pause_watch_a");
        watched(&service, &sink, &first).await;

        source.set(|state| {
            state.error = Some(PrSourceError::RateLimited {
                retry_after: Some(60),
            });
        });
        service.poll_once().await;
        source.set(|state| state.error = None);
        source.reset_calls();

        let second = thread("th_pause_watch_b");
        sink.add(&second);
        service
            .link_known(&second, "acct_1", &repo(), 7)
            .await
            .expect("link");
        service.watch(&first, &repo(), 7).await.expect("re-watch");
        service.watch(&second, &repo(), 7).await.expect("watch");

        assert_eq!(
            source.get_pr_calls(),
            0,
            "a watch must not read while the PR is paused"
        );
        assert_eq!(
            source.check_calls(),
            0,
            "a watch must not read checks while the PR is paused"
        );

        // The first poll after the pause takes the baseline, so a change that
        // happened before it is not replayed as news.
        service.clear_pause(&repo(), 7);
        source.set(|state| {
            state.checks = checks(
                "sha-1",
                vec![check(
                    "build",
                    ForgeCheckStatus::Failure,
                    Some(true),
                    Some(3),
                )],
            );
        });
        service.poll_once().await;
        assert!(sink.wakes().is_empty(), "{:?}", sink.wakes());

        // A failure that happens after that baseline is news for each watch.
        source.set(|state| {
            state.checks = checks(
                "sha-1",
                vec![check(
                    "build",
                    ForgeCheckStatus::Failure,
                    Some(true),
                    Some(4),
                )],
            );
        });
        service.poll_once().await;
        assert_eq!(sink.wakes().len(), 2, "{:?}", sink.wakes());
        service.poll_once().await;
        assert_eq!(sink.wakes().len(), 2);
    }

    #[tokio::test]
    async fn a_failed_wake_is_retried_and_delivered_once() {
        let (service, source, sink) = service().await;
        let thread_id = thread("th_wake_retry");
        watched(&service, &sink, &thread_id).await;

        source.set(|state| {
            state.pr = detail(ForgePrState::Merged, "sha-1", ForgeMergeable::Mergeable);
        });
        sink.fail_next_wakes(1);
        service.poll_once().await;
        assert!(sink.wakes().is_empty());
        assert_eq!(
            service.watching_threads(&repo(), 7).len(),
            1,
            "a terminal watch stays alive for its retry"
        );

        service.poll_once().await;
        let wakes = sink.wakes();
        assert_eq!(wakes.len(), 1, "{wakes:?}");
        assert!(wakes[0].1.contains("was merged"), "{wakes:?}");
        assert!(service.watching_threads(&repo(), 7).is_empty());

        service.poll_once().await;
        assert_eq!(sink.wakes().len(), 1);
    }

    #[tokio::test]
    async fn repeated_failed_wakes_end_the_watch() {
        let (service, source, sink) = service().await;
        let thread_id = thread("th_wake_retry_bound");
        watched(&service, &sink, &thread_id).await;

        source.set(|state| {
            state.pr = detail(ForgePrState::Merged, "sha-1", ForgeMergeable::Mergeable);
        });
        sink.fail_next_wakes(MAX_FAILED_WAKES as usize);
        for _ in 0..MAX_FAILED_WAKES {
            service.poll_once().await;
        }
        assert!(service.watching_threads(&repo(), 7).is_empty());
        assert!(sink.wakes().is_empty());
    }

    #[tokio::test]
    async fn a_poll_that_overtakes_an_unwatch_and_rewatch_cannot_seed_the_new_watcher() {
        let (service, source, sink) = service().await;
        let thread_id = thread("th_generation_reuse");
        sink.add(&thread_id);
        service
            .link_known(&thread_id, "acct_1", &repo(), 7)
            .await
            .expect("link");

        // The first watch is slow in checks. Its watcher exists with a fresh
        // generation and no baseline; its read holds the passing body.
        source.set(|state| {
            state.checks = checks(
                "sha-1",
                vec![check(
                    "build",
                    ForgeCheckStatus::Success,
                    Some(true),
                    Some(3),
                )],
            );
        });
        let (first_started, release_first) = source.gate_next_checks();
        let service_for_first = Arc::clone(&service);
        let thread_for_first = thread_id.clone();
        let first_watch = tokio::spawn(async move {
            service_for_first
                .watch(&thread_for_first, &repo(), 7)
                .await
                .expect("watch");
        });
        first_started
            .await
            .expect("the first watch read is waiting");

        // A poll starts, captures that watcher's generation and is held in
        // checks with an older, still-running body.
        source.set(|state| {
            state.checks = checks(
                "sha-1",
                vec![check(
                    "build",
                    ForgeCheckStatus::InProgress,
                    Some(true),
                    None,
                )],
            );
        });
        let (poll_started, release_poll) = source.gate_next_checks();
        let service_for_poll = Arc::clone(&service);
        let poll = tokio::spawn(async move { service_for_poll.poll_once().await });
        poll_started.await.expect("the poll read is waiting");

        // The thread unwatches and watches again while that poll is in flight.
        // The replacement's read is held too, so it is still at its fresh
        // generation when the older poll is released.
        service.unwatch(&thread_id, &repo(), 7);
        source.set(|state| {
            state.checks = checks(
                "sha-1",
                vec![check(
                    "build",
                    ForgeCheckStatus::Success,
                    Some(true),
                    Some(3),
                )],
            );
        });
        let (second_started, release_second) = source.gate_next_checks();
        let service_for_second = Arc::clone(&service);
        let thread_for_second = thread_id.clone();
        let second_watch = tokio::spawn(async move {
            service_for_second
                .watch(&thread_for_second, &repo(), 7)
                .await
                .expect("re-watch");
        });
        second_started.await.expect("the re-watch read is waiting");

        // The in-flight poll finishes first, with the older body.
        release_poll.send(()).expect("release the poll read");
        poll.await.expect("poll finished");

        // Then the two watch reads finish.
        release_first
            .send(())
            .expect("release the first watch read");
        release_second.send(()).expect("release the re-watch read");
        first_watch.await.expect("first watch finished");
        second_watch.await.expect("re-watch finished");

        // The replacement's baseline is its own fresh read, never the older
        // body a poll captured under the generation of the watcher it
        // replaced.
        let baseline = service
            .lock_watches()
            .prs
            .values()
            .flat_map(|watched| watched.watchers.values())
            .find_map(|watch| {
                watch
                    .baseline
                    .as_ref()
                    .map(|baseline| baseline.checks.state)
            });
        assert_eq!(
            baseline,
            Some(ForgeChecksState::Passing),
            "the new watcher's baseline must come from its own read"
        );

        // And the passing body is not reported as news.
        service.poll_once().await;
        assert!(sink.wakes().is_empty(), "{:?}", sink.wakes());
    }

    #[tokio::test]
    async fn a_delivered_terminal_wake_does_not_delete_a_rewatched_watcher() {
        let (service, source, sink) = service().await;
        let thread_id = thread("th_wake_rewatch");
        watched(&service, &sink, &thread_id).await;

        source.set(|state| {
            state.pr = detail(ForgePrState::Merged, "sha-1", ForgeMergeable::Mergeable);
        });

        // The merge wake is held inside delivery, where a re-watch can happen.
        let (started, release) = sink.gate_next_wake();
        let service_for_poll = Arc::clone(&service);
        let poll = tokio::spawn(async move { service_for_poll.poll_once().await });
        started.await.expect("the merge wake is waiting");

        // The thread re-watches while that wake is in flight. Its read fails
        // (the forge is briefly unavailable), so the replacement waits for its
        // first poll instead of ending on a merged body it could not read.
        source.set(|state| {
            state.error = Some(PrSourceError::Other("network down".into()));
        });
        service.unwatch(&thread_id, &repo(), 7);
        service
            .watch(&thread_id, &repo(), 7)
            .await
            .expect("re-watch");
        source.set(|state| state.error = None);

        // The delivery finishes. It may end only the watcher it was for.
        release.send(()).expect("release the merge wake");
        poll.await.expect("poll finished");

        assert_eq!(
            service.watching_threads(&repo(), 7).len(),
            1,
            "a delivered merge wake must not delete a watcher a re-watch created"
        );
        let wakes = sink.wakes();
        assert_eq!(wakes.len(), 1, "{wakes:?}");
        assert!(wakes[0].1.contains("was merged"), "{wakes:?}");

        // The replacement now reads the merged PR and ends as a fresh watch
        // would, without repeating the merge as news.
        service.poll_once().await;
        assert!(service.watching_threads(&repo(), 7).is_empty());
        assert_eq!(sink.wakes().len(), 1);
    }

    #[tokio::test]
    async fn a_failed_wake_a_rewatch_overtook_does_not_restore_the_old_baseline() {
        let (service, source, sink) = service().await;
        let thread_id = thread("th_failed_wake_rewatch");
        watched(&service, &sink, &thread_id).await;

        // A check fails. The poll reports it, but its wake is held inside
        // delivery and is going to fail.
        source.set(|state| {
            state.checks = checks(
                "sha-1",
                vec![check(
                    "build",
                    ForgeCheckStatus::Failure,
                    Some(true),
                    Some(3),
                )],
            );
        });
        let (started, release) = sink.gate_next_wake();
        sink.fail_next_wakes(1);
        let service_for_poll = Arc::clone(&service);
        let poll = tokio::spawn(async move { service_for_poll.poll_once().await });
        started.await.expect("the failure wake is waiting");

        // The thread unwatches and watches again while that wake is in
        // flight. The re-watch reads a body that already contains the
        // failure, so the failure is not news for the replacement.
        service.unwatch(&thread_id, &repo(), 7);
        service
            .watch(&thread_id, &repo(), 7)
            .await
            .expect("re-watch");

        // Releasing the failed wake must not put the pre-failure snapshot
        // back on the replacement: that would make the next poll report the
        // same failure for a watcher that already has it.
        release.send(()).expect("release the failure wake");
        poll.await.expect("poll finished");
        assert!(sink.wakes().is_empty(), "{:?}", sink.wakes());

        service.poll_once().await;
        assert!(
            sink.wakes().is_empty(),
            "a failed wake a re-watch overtook must not re-report its change: {:?}",
            sink.wakes()
        );
        assert_eq!(service.watching_threads(&repo(), 7).len(), 1);
    }

    fn wiremock_repo(server: &wiremock::MockServer) -> ForgeRepoRef {
        ForgeRepoRef {
            forge: h4rn_protocol::ForgeKind::GitHub,
            base_url: server.uri(),
            owner: "acme".into(),
            repo: "widget".into(),
        }
    }

    fn pr_json(state: &str, merged: bool) -> serde_json::Value {
        serde_json::json!({
            "number": 7,
            "title": "Add PR watch",
            "state": state,
            "merged": merged,
            "merged_at": if merged { serde_json::json!("2026-01-01T00:03:00Z") } else { serde_json::json!(null) },
            "draft": false,
            "user": {"login": "alice"},
            "head": {"ref": "h4rn/pr-watch", "sha": "sha-1", "repo": {"full_name": "acme/widget"}},
            "base": {"ref": "main", "sha": "base"},
            "updated_at": "2026-01-01T00:00:01Z",
            "html_url": "https://github.com/acme/widget/pull/7",
            "body": "",
            "mergeable": true,
            "additions": 1,
            "deletions": 0,
            "changed_files": 1,
            "commits": 1,
            "created_at": "2026-01-01T00:00:00Z"
        })
    }

    /// A real forge service whose one GitHub account points at the mock server.
    async fn forge_over(
        server: &wiremock::MockServer,
    ) -> (Arc<crate::forge::ForgeService>, tempfile::TempDir) {
        use h4rn_protocol::{ForgeAccount, ForgeCredentialSource};

        let dir = tempfile::tempdir().expect("forge data dir");
        let account = ForgeAccount {
            account_id: "acct_1".into(),
            forge: h4rn_protocol::ForgeKind::GitHub,
            base_url: server.uri(),
            username: Some("alice".into()),
            display_name: None,
            scopes: Vec::new(),
            source: ForgeCredentialSource::Pat,
            created_at: Utc::now(),
        };
        std::fs::write(
            dir.path().join(crate::forge::ACCOUNTS_FILE),
            serde_json::to_vec(&vec![account]).expect("accounts"),
        )
        .expect("write accounts");
        std::fs::write(
            dir.path().join(crate::forge::HOSTS_FILE),
            serde_json::to_vec(&serde_json::json!({
                "hosts": [{"host": "github.com", "forge": "github", "base_url": server.uri()}]
            }))
            .expect("hosts"),
        )
        .expect("write hosts");
        h4rn_transport::keys::write_secret_file(
            &dir.path().join("forge-token-acct_1"),
            b"test-token",
        )
        .expect("store the token");
        let forge = crate::forge::ForgeService::open(dir.path()).expect("forge service");
        forge.shutdown();
        (forge, dir)
    }

    #[tokio::test]
    async fn watch_reads_do_not_enrol_the_pr_in_the_checks_poller() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v3/repos/acme/widget/pulls/7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(pr_json("open", false)))
            .mount(&server)
            .await;
        // The watch's full read: comments, reviews and checks. All of them
        // must succeed, or the watch never takes a baseline and a leak would
        // go unnoticed.
        for quiet in [
            "/api/v3/repos/acme/widget/issues/7/comments",
            "/api/v3/repos/acme/widget/pulls/7/comments",
            "/api/v3/repos/acme/widget/pulls/7/reviews",
        ] {
            Mock::given(method("GET"))
                .and(path(quiet))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
                .mount(&server)
                .await;
        }
        Mock::given(method("GET"))
            .and(path("/api/v3/repos/acme/widget/commits/sha-1/check-runs"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"check_runs": []})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v3/repos/acme/widget/commits/sha-1/status"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"state": "success", "statuses": []})),
            )
            .mount(&server)
            .await;

        let (forge, _forge_dir) = forge_over(&server).await;
        let repo_ref = wiremock_repo(&server);
        let journal: Arc<dyn Journal> =
            Arc::new(SqliteJournal::in_memory().await.expect("journal"));
        let sink = Arc::new(MockSink::default());
        let service = PrLinkService::new(
            journal,
            Arc::new(ForgePrSource::new(Arc::clone(&forge))),
            sink.clone(),
        );
        let thread_id = thread("th_no_leak");
        sink.add(&thread_id);

        service
            .link(&thread_id, None, &repo_ref, 7)
            .await
            .expect("link");
        service
            .watch(&thread_id, &repo_ref, 7)
            .await
            .expect("watch");
        // The watch's own read completed: it has a baseline to compare with.
        assert!(
            service
                .lock_watches()
                .prs
                .values()
                .flat_map(|watched| watched.watchers.values())
                .all(|watch| watch.baseline.is_some()),
            "the watch read must complete and take a baseline"
        );
        assert!(
            forge.watched_prs().is_empty(),
            "a watch read must not add the PR to the checks poller: {:?}",
            forge.watched_prs()
        );

        // A full poll reads all three quiet paths again and still enrols
        // nothing.
        service.poll_once().await;
        assert_eq!(service.watching_threads(&repo_ref, 7).len(), 1);
        assert!(sink.wakes().is_empty(), "{:?}", sink.wakes());
        assert!(
            forge.watched_prs().is_empty(),
            "a watch poll must not add the PR to the checks poller: {:?}",
            forge.watched_prs()
        );

        service.unwatch(&thread_id, &repo_ref, 7);
        service
            .unlink(&thread_id, &repo_ref, 7)
            .await
            .expect("unlink");
        assert!(
            forge.watched_prs().is_empty(),
            "unlinking must not leave a poll behind: {:?}",
            forge.watched_prs()
        );

        // The PR page's own read still registers the PR for its checks poll.
        forge
            .get_pr(Some("acct_1"), &repo_ref, 7)
            .await
            .expect("page read");
        assert_eq!(forge.watched_prs().len(), 1);
    }
}
