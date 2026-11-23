//! Thread ↔ issue links.
//!
//! A link is durable metadata owned by the thread journal: the tracker scope
//! (a forge repository or a Linear team) and the provider issue id. State is
//! always read live; a failed read is reported as such instead of being kept
//! from the last success. Unlike pull requests, an issue link starts no watch.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use h4rn_forge::ForgeError;
use h4rn_protocol::issues::{IssueScope, LinearIssueSummary, LinkedIssueState, ThreadIssue};
use h4rn_protocol::{ForgeIssueSummary, ForgeRepoRef, ProtoError, ProtoErrorCode, ThreadId};
use serde::{Deserialize, Serialize};

use crate::forge::ForgeServiceError;
use crate::journal::{Journal, JournalError};
use crate::threads::ThreadManager;

/// One durable thread → issue link.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueLinkRecord {
    /// Thread that holds the link.
    pub thread_id: ThreadId,
    /// Account the daemon reads the issue with.
    pub account_id: String,
    /// Tracker coordinates.
    pub scope: IssueScope,
    /// Provider issue id: a decimal number for forge issues.
    pub issue_id: String,
    /// When the link was created.
    pub linked_at: DateTime<Utc>,
}

/// A failure while linking or reading an issue.
#[derive(Debug, thiserror::Error)]
pub enum IssueLinkError {
    /// The thread does not exist on this daemon.
    #[error("thread {0} was not found")]
    ThreadNotFound(String),
    /// The issue id is not valid for its tracker.
    #[error("{0}")]
    InvalidIssue(String),
    /// The tracker failed.
    #[error("{0}")]
    Tracker(String),
    /// The journal failed.
    #[error("{0}")]
    Journal(String),
}

impl IssueLinkError {
    /// Maps the failure onto the protocol error class and a user hint.
    pub fn into_proto(self) -> ProtoError {
        match self {
            Self::ThreadNotFound(message) => ProtoError::new(
                ProtoErrorCode::NotFound,
                message,
                "Refresh the thread list and try again",
            ),
            Self::InvalidIssue(message) => ProtoError::new(
                ProtoErrorCode::InvalidRequest,
                message,
                "Pick the issue from the Issues page",
            ),
            Self::Tracker(message) => ProtoError::new(
                ProtoErrorCode::Internal,
                message,
                "Check the tracker connection and try again",
            ),
            Self::Journal(message) => ProtoError::new(
                ProtoErrorCode::Internal,
                message,
                "Check the daemon log for journal details",
            ),
        }
    }
}

impl From<JournalError> for IssueLinkError {
    fn from(error: JournalError) -> Self {
        Self::Journal(error.to_string())
    }
}

impl From<ForgeServiceError> for IssueLinkError {
    fn from(error: ForgeServiceError) -> Self {
        Self::Tracker(error.to_string())
    }
}

impl From<ForgeError> for IssueLinkError {
    fn from(error: ForgeError) -> Self {
        Self::Tracker(error.to_string())
    }
}

/// The tracker reads the link service performs.
#[async_trait]
pub trait IssueSource: Send + Sync {
    /// Picks the account for a scope and returns its id.
    async fn resolve_account(
        &self,
        account_id: Option<&str>,
        scope: &IssueScope,
    ) -> Result<String, IssueLinkError>;

    /// Reads one forge issue's summary.
    async fn forge_issue(
        &self,
        account_id: &str,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<ForgeIssueSummary, IssueLinkError>;

    /// Reads one Linear issue's summary.
    async fn linear_issue(
        &self,
        account_id: &str,
        team_id: &str,
        issue_id: &str,
    ) -> Result<LinearIssueSummary, IssueLinkError>;
}

/// What links need from the thread manager.
#[async_trait]
pub trait IssueThreadSink: Send + Sync {
    /// The thread's title, or `None` when it does not exist.
    async fn thread_title(&self, thread_id: &ThreadId) -> Option<String>;
}

/// The daemon's thread manager as a link sink.
pub struct ThreadTitleSink {
    manager: Arc<ThreadManager>,
}

impl ThreadTitleSink {
    /// Builds the sink for one manager.
    pub fn new(manager: Arc<ThreadManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl IssueThreadSink for ThreadTitleSink {
    async fn thread_title(&self, thread_id: &ThreadId) -> Option<String> {
        self.manager
            .thread_summary(thread_id)
            .await
            .map(|summary| summary.title)
    }
}

/// The service behind issue link requests.
pub struct IssueLinkService {
    journal: Arc<dyn Journal>,
    source: Arc<dyn IssueSource>,
    sink: Arc<dyn IssueThreadSink>,
}

impl IssueLinkService {
    /// Builds the service.
    pub fn new(
        journal: Arc<dyn Journal>,
        source: Arc<dyn IssueSource>,
        sink: Arc<dyn IssueThreadSink>,
    ) -> Arc<Self> {
        Arc::new(Self {
            journal,
            source,
            sink,
        })
    }

    /// Links an issue after reading it once, so the caller gets current state.
    pub async fn link(
        &self,
        thread_id: &ThreadId,
        account_id: Option<&str>,
        scope: &IssueScope,
        issue_id: &str,
    ) -> Result<ThreadIssue, IssueLinkError> {
        if self.sink.thread_title(thread_id).await.is_none() {
            return Err(IssueLinkError::ThreadNotFound(
                thread_id.as_str().to_owned(),
            ));
        }
        let issue_id = issue_id.trim();
        if issue_id.is_empty() {
            return Err(IssueLinkError::InvalidIssue("the issue id is empty".into()));
        }
        let account_id = self.source.resolve_account(account_id, scope).await?;
        let state = self.read(&account_id, scope, issue_id).await?;
        let record = IssueLinkRecord {
            thread_id: thread_id.clone(),
            account_id,
            scope: scope.clone(),
            issue_id: issue_id.to_owned(),
            linked_at: Utc::now(),
        };
        self.journal.upsert_issue_link(&record).await?;
        Ok(ThreadIssue {
            scope: record.scope,
            issue_id: record.issue_id,
            account_id: record.account_id,
            state: Some(state),
            error: None,
            linked_at: record.linked_at,
        })
    }

    /// Removes one link.
    pub async fn unlink(
        &self,
        thread_id: &ThreadId,
        scope: &IssueScope,
        issue_id: &str,
    ) -> Result<(), IssueLinkError> {
        self.journal
            .remove_issue_link(thread_id, scope, issue_id)
            .await?;
        Ok(())
    }

    /// Lists a thread's links with the latest tracker state.
    pub async fn list(&self, thread_id: &ThreadId) -> Result<Vec<ThreadIssue>, IssueLinkError> {
        if self.sink.thread_title(thread_id).await.is_none() {
            return Err(IssueLinkError::ThreadNotFound(
                thread_id.as_str().to_owned(),
            ));
        }
        let records = self.journal.issue_links_for_thread(thread_id).await?;
        let mut issues = Vec::with_capacity(records.len());
        for record in records {
            match self
                .read(&record.account_id, &record.scope, &record.issue_id)
                .await
            {
                Ok(state) => issues.push(ThreadIssue {
                    scope: record.scope,
                    issue_id: record.issue_id,
                    account_id: record.account_id,
                    state: Some(state),
                    error: None,
                    linked_at: record.linked_at,
                }),
                Err(error) => issues.push(ThreadIssue {
                    scope: record.scope,
                    issue_id: record.issue_id,
                    account_id: record.account_id,
                    state: None,
                    error: Some(error.to_string()),
                    linked_at: record.linked_at,
                }),
            }
        }
        Ok(issues)
    }

    async fn read(
        &self,
        account_id: &str,
        scope: &IssueScope,
        issue_id: &str,
    ) -> Result<LinkedIssueState, IssueLinkError> {
        match scope {
            IssueScope::Forge { repo } => {
                let number = issue_id.parse::<u32>().map_err(|_| {
                    IssueLinkError::InvalidIssue(format!("{issue_id} is not a forge issue number"))
                })?;
                let issue = self.source.forge_issue(account_id, repo, number).await?;
                Ok(LinkedIssueState::Forge { issue })
            }
            IssueScope::Linear { team_id } => {
                let issue = self
                    .source
                    .linear_issue(account_id, team_id, issue_id)
                    .await?;
                Ok(LinkedIssueState::Linear { issue })
            }
        }
    }
}

/// The real tracker source: forge accounts and Linear connections.
pub struct TrackerIssueSource {
    forge: Arc<crate::forge::ForgeService>,
    linear: Arc<crate::linear::LinearService>,
}

impl TrackerIssueSource {
    /// Builds the source over both services.
    pub fn new(
        forge: Arc<crate::forge::ForgeService>,
        linear: Arc<crate::linear::LinearService>,
    ) -> Self {
        Self { forge, linear }
    }
}

#[async_trait]
impl IssueSource for TrackerIssueSource {
    async fn resolve_account(
        &self,
        account_id: Option<&str>,
        scope: &IssueScope,
    ) -> Result<String, IssueLinkError> {
        match scope {
            IssueScope::Forge { repo } => self
                .forge
                .resolve_account_id(account_id, repo)
                .map_err(IssueLinkError::from),
            IssueScope::Linear { .. } => {
                if let Some(account_id) = account_id {
                    return Ok(account_id.to_owned());
                }
                let accounts = self.linear.list_accounts();
                match accounts.as_slice() {
                    [only] => Ok(only.account_id.clone()),
                    [] => Err(IssueLinkError::Tracker(
                        "connect Linear in Settings → Source control & issues first".into(),
                    )),
                    _ => Err(IssueLinkError::InvalidIssue(
                        "several Linear connections exist; name account_id".into(),
                    )),
                }
            }
        }
    }

    async fn forge_issue(
        &self,
        account_id: &str,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<ForgeIssueSummary, IssueLinkError> {
        let issue = self.forge.get_issue(Some(account_id), repo, number).await?;
        Ok(issue.summary)
    }

    async fn linear_issue(
        &self,
        account_id: &str,
        _team_id: &str,
        issue_id: &str,
    ) -> Result<LinearIssueSummary, IssueLinkError> {
        let issue = self.linear.get_issue(account_id, issue_id, None).await?;
        Ok(issue.summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::SqliteJournal;
    use h4rn_protocol::ForgeKind;
    use h4rn_protocol::issues::LinearStateType;
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct MockSource {
        repo: ForgeRepoRef,
        calls: Mutex<Vec<String>>,
        fail: Mutex<bool>,
    }

    impl MockSource {
        fn new() -> Self {
            Self {
                repo: ForgeRepoRef {
                    forge: ForgeKind::GitHub,
                    base_url: "https://github.com".into(),
                    owner: "acme".into(),
                    repo: "widget".into(),
                },
                calls: Mutex::new(Vec::new()),
                fail: Mutex::new(false),
            }
        }
    }

    #[async_trait]
    impl IssueSource for MockSource {
        async fn resolve_account(
            &self,
            account_id: Option<&str>,
            _scope: &IssueScope,
        ) -> Result<String, IssueLinkError> {
            Ok(account_id.unwrap_or("acct_1").to_owned())
        }

        async fn forge_issue(
            &self,
            _account_id: &str,
            repo: &ForgeRepoRef,
            number: u32,
        ) -> Result<ForgeIssueSummary, IssueLinkError> {
            self.calls.lock().unwrap().push(format!("forge:{number}"));
            if *self.fail.lock().unwrap() {
                return Err(IssueLinkError::Tracker("forge is down".into()));
            }
            Ok(ForgeIssueSummary {
                repo: repo.clone(),
                number,
                title: "Dark mode flickers".into(),
                state: h4rn_protocol::ForgeIssueState::Open,
                author: "alice".into(),
                labels: Vec::new(),
                assignees: Vec::new(),
                comment_count: 1,
                updated_at: Some(Utc::now()),
                web_url: "https://github.com/acme/widget/issues/12".into(),
            })
        }

        async fn linear_issue(
            &self,
            _account_id: &str,
            _team_id: &str,
            issue_id: &str,
        ) -> Result<LinearIssueSummary, IssueLinkError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("linear:{issue_id}"));
            Ok(LinearIssueSummary {
                id: issue_id.into(),
                identifier: "ENG-1".into(),
                title: "Rate limit".into(),
                state_name: "In Progress".into(),
                state_type: LinearStateType::Started,
                team_id: "team_1".into(),
                team_key: "ENG".into(),
                assignees: Vec::new(),
                labels: Vec::new(),
                updated_at: Some(Utc::now()),
                url: "https://linear.app/acme/issue/ENG-1".into(),
            })
        }
    }

    struct MockSink {
        threads: HashMap<String, String>,
    }

    #[async_trait]
    impl IssueThreadSink for MockSink {
        async fn thread_title(&self, thread_id: &ThreadId) -> Option<String> {
            self.threads.get(thread_id.as_str()).cloned()
        }
    }

    async fn service() -> (Arc<IssueLinkService>, Arc<MockSource>, Arc<SqliteJournal>) {
        let journal = Arc::new(SqliteJournal::in_memory().await.unwrap());
        let source = Arc::new(MockSource::new());
        let sink = Arc::new(MockSink {
            threads: HashMap::from([("th_1".to_owned(), "Fix flicker".to_owned())]),
        });
        (
            IssueLinkService::new(journal.clone(), source.clone(), sink),
            source,
            journal,
        )
    }

    #[tokio::test]
    async fn a_link_is_stored_and_its_state_is_read_again() {
        let (service, source, _journal) = service().await;
        let thread = ThreadId::new("th_1");
        let scope = IssueScope::Forge {
            repo: source.repo.clone(),
        };
        let linked = service.link(&thread, None, &scope, "12").await.unwrap();
        assert!(matches!(linked.state, Some(LinkedIssueState::Forge { .. })));
        let listed = service.list(&thread).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            source.calls.lock().unwrap().as_slice(),
            ["forge:12".to_owned(), "forge:12".to_owned()]
        );
    }

    #[tokio::test]
    async fn a_link_survives_a_restart_and_a_failed_read_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.db");
        let thread = ThreadId::new("th_1");
        let source = Arc::new(MockSource::new());
        let scope = IssueScope::Forge {
            repo: source.repo.clone(),
        };
        {
            let journal = Arc::new(SqliteJournal::connect(&path).await.unwrap());
            let sink = Arc::new(MockSink {
                threads: HashMap::from([("th_1".to_owned(), "Fix flicker".to_owned())]),
            });
            let service = IssueLinkService::new(journal, source.clone(), sink);
            service.link(&thread, None, &scope, "12").await.unwrap();
        }
        let journal = Arc::new(SqliteJournal::connect(&path).await.unwrap());
        let sink = Arc::new(MockSink {
            threads: HashMap::from([("th_1".to_owned(), "Fix flicker".to_owned())]),
        });
        let service = IssueLinkService::new(journal, source.clone(), sink);
        *source.fail.lock().unwrap() = true;
        let listed = service.list(&thread).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].state.is_none());
        assert!(
            listed[0]
                .error
                .as_deref()
                .unwrap()
                .contains("forge is down")
        );
    }

    #[tokio::test]
    async fn an_unknown_thread_cannot_hold_a_link() {
        let (service, source, _journal) = service().await;
        let error = service
            .link(
                &ThreadId::new("th_missing"),
                None,
                &IssueScope::Forge {
                    repo: source.repo.clone(),
                },
                "12",
            )
            .await
            .unwrap_err();
        assert!(matches!(error, IssueLinkError::ThreadNotFound(_)));
    }

    #[tokio::test]
    async fn unlink_removes_only_the_named_issue() {
        let (service, source, _journal) = service().await;
        let thread = ThreadId::new("th_1");
        let scope = IssueScope::Forge {
            repo: source.repo.clone(),
        };
        service.link(&thread, None, &scope, "12").await.unwrap();
        service.unlink(&thread, &scope, "12").await.unwrap();
        assert!(service.list(&thread).await.unwrap().is_empty());
    }
}
