//! Linear connections, team selection and issue reads.
//!
//! An API key is validated once, stored in the OS keychain or the encrypted
//! secret file, and never returned: [`LinearAccount`] carries only the
//! workspace name and email. A project's team choice is public metadata and is
//! kept in `linear-projects.json`; the key is not part of it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use chrono::Utc;
use h4rn_forge::ForgeError;
use h4rn_forge::issues::{LINEAR_ENDPOINT, LinearClient};
use h4rn_protocol::issues::{
    ForgeIssueFilter, LinearAccount, LinearIssue, LinearIssueComment, LinearIssuePage,
    LinearProjectTeam, LinearTeam,
};

use crate::config::create_private_dir;
use crate::forge::{ForgeServiceError, SecretStore};

/// File holding public Linear connection metadata (never a key).
pub const ACCOUNTS_FILE: &str = "linear-accounts.json";
/// File holding each project's chosen team.
pub const PROJECTS_FILE: &str = "linear-projects.json";
/// OS keychain service name for Linear keys.
const KEYCHAIN_SERVICE: &str = "h4rn-linear";

/// The daemon's Linear connections.
pub struct LinearService {
    accounts_path: PathBuf,
    projects_path: PathBuf,
    accounts: RwLock<Vec<LinearAccount>>,
    projects: RwLock<Vec<LinearProjectTeam>>,
    tokens: SecretStore,
    endpoint: String,
}

impl LinearService {
    /// Opens the service, reading public metadata from `data_dir`.
    pub fn open(data_dir: &Path) -> Result<Arc<Self>, ForgeServiceError> {
        let endpoint = std::env::var("H4RN_LINEAR_ENDPOINT")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| LINEAR_ENDPOINT.to_owned());
        Self::open_with_endpoint(data_dir, &endpoint)
    }

    /// Opens the service with an explicit GraphQL endpoint, for tests.
    pub fn open_with_endpoint(
        data_dir: &Path,
        endpoint: &str,
    ) -> Result<Arc<Self>, ForgeServiceError> {
        create_private_dir(data_dir)
            .map_err(|error| ForgeServiceError::Local(format!("prepare data dir: {error}")))?;
        let accounts =
            read_json::<Vec<LinearAccount>>(&data_dir.join(ACCOUNTS_FILE))?.unwrap_or_default();
        let projects =
            read_json::<Vec<LinearProjectTeam>>(&data_dir.join(PROJECTS_FILE))?.unwrap_or_default();
        Ok(Arc::new(Self {
            accounts_path: data_dir.join(ACCOUNTS_FILE),
            projects_path: data_dir.join(PROJECTS_FILE),
            accounts: RwLock::new(accounts),
            projects: RwLock::new(projects),
            tokens: SecretStore::new(data_dir, KEYCHAIN_SERVICE, "linear-token-"),
            endpoint: endpoint.to_owned(),
        }))
    }

    /// Connected workspaces, oldest first.
    pub fn list_accounts(&self) -> Vec<LinearAccount> {
        self.accounts
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Validates an API key and stores the connection.
    pub async fn connect(&self, api_key: &str) -> Result<LinearAccount, ForgeServiceError> {
        let client = LinearClient::with_endpoint(api_key, &self.endpoint)?;
        let viewer = client.viewer().await?;
        let account = LinearAccount {
            account_id: format!("lin_{}", uuid::Uuid::now_v7().simple()),
            name: viewer.name,
            email: viewer.email,
            created_at: Utc::now(),
        };
        self.tokens.save(&account.account_id, api_key.trim())?;
        {
            let mut accounts = self
                .accounts
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            accounts.push(account.clone());
            if let Err(error) = write_json(&self.accounts_path, &*accounts) {
                accounts.retain(|candidate| candidate.account_id != account.account_id);
                self.tokens.remove(&account.account_id);
                return Err(error);
            }
        }
        Ok(account)
    }

    /// Forgets a connection, its key and every project choice that used it.
    pub fn remove(&self, account_id: &str) -> Result<(), ForgeServiceError> {
        {
            let mut accounts = self
                .accounts
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let before = accounts.len();
            accounts.retain(|account| account.account_id != account_id);
            if accounts.len() == before {
                return Err(
                    ForgeError::NotFound(format!("no Linear connection {account_id}")).into(),
                );
            }
            write_json(&self.accounts_path, &*accounts)?;
        }
        {
            let mut projects = self
                .projects
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            projects.retain(|team| team.account_id != account_id);
            write_json(&self.projects_path, &*projects)?;
        }
        self.tokens.remove(account_id);
        Ok(())
    }

    /// Teams one connection exposes.
    pub async fn teams(&self, account_id: &str) -> Result<Vec<LinearTeam>, ForgeServiceError> {
        let client = self.client(account_id)?;
        Ok(client.teams().await?)
    }

    /// Team choices, one per project at most.
    pub fn project_teams(&self) -> Vec<LinearProjectTeam> {
        self.projects
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Attaches a team to a project, or detaches it with `None`.
    ///
    /// The team's key and name are read from Linear, not trusted from the
    /// caller, so a stale client cannot label a mapping with a team that no
    /// longer exists.
    pub async fn set_project_team(
        &self,
        project_id: &str,
        team: Option<LinearProjectTeam>,
    ) -> Result<Option<LinearProjectTeam>, ForgeServiceError> {
        let Some(team) = team else {
            let mut projects = self
                .projects
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            projects.retain(|candidate| candidate.project_id != project_id);
            write_json(&self.projects_path, &*projects)?;
            return Ok(None);
        };
        let teams = self.teams(&team.account_id).await?;
        let verified = teams
            .into_iter()
            .find(|candidate| candidate.id == team.team_id)
            .ok_or_else(|| {
                ForgeError::NotFound(format!(
                    "Linear connection {} has no team {}",
                    team.account_id, team.team_id
                ))
            })?;
        let mapping = LinearProjectTeam {
            project_id: project_id.to_owned(),
            account_id: team.account_id,
            team_id: verified.id,
            team_key: verified.key,
            team_name: verified.name,
        };
        {
            let mut projects = self
                .projects
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            projects.retain(|candidate| candidate.project_id != project_id);
            projects.push(mapping.clone());
            write_json(&self.projects_path, &*projects)?;
        }
        Ok(Some(mapping))
    }

    /// Lists the issues of the team attached to a project.
    pub async fn list_issues(
        &self,
        project_id: &str,
        filter: ForgeIssueFilter,
        cursor: Option<&str>,
        query: Option<&str>,
    ) -> Result<LinearIssuePage, ForgeServiceError> {
        let mapping = self.mapping(project_id)?;
        let client = self.client(&mapping.account_id)?;
        Ok(client
            .list_issues(&mapping.team_id, filter, cursor, query)
            .await?)
    }

    /// Reads one issue and a page of its comments.
    pub async fn get_issue(
        &self,
        account_id: &str,
        issue_id: &str,
        comments_after: Option<&str>,
    ) -> Result<LinearIssue, ForgeServiceError> {
        let client = self.client(account_id)?;
        Ok(client.get_issue(issue_id, comments_after).await?)
    }

    /// Reads one issue for a linked thread, resolving the account itself.
    pub async fn get_linked_issue(
        &self,
        account_id: &str,
        issue_id: &str,
    ) -> Result<LinearIssue, ForgeServiceError> {
        self.get_issue(account_id, issue_id, None).await
    }

    /// Posts a comment.
    pub async fn comment(
        &self,
        account_id: &str,
        issue_id: &str,
        body: &str,
    ) -> Result<LinearIssueComment, ForgeServiceError> {
        let client = self.client(account_id)?;
        Ok(client.create_comment(issue_id, body).await?)
    }

    fn mapping(&self, project_id: &str) -> Result<LinearProjectTeam, ForgeServiceError> {
        self.projects
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .find(|candidate| candidate.project_id == project_id)
            .cloned()
            .ok_or_else(|| {
                ForgeError::NotConfigured(
                    "choose a Linear team for this project in Settings → Source control & issues"
                        .into(),
                )
                .into()
            })
    }

    fn client(&self, account_id: &str) -> Result<LinearClient, ForgeServiceError> {
        let known = self
            .accounts
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .any(|account| account.account_id == account_id);
        if !known {
            return Err(
                ForgeError::NotConfigured(format!("no Linear connection {account_id}")).into(),
            );
        }
        let key = self.tokens.load(account_id)?.ok_or_else(|| {
            ForgeError::InvalidRequest(
                "the stored Linear key is missing; connect Linear again".into(),
            )
        })?;
        Ok(LinearClient::with_endpoint(&key, &self.endpoint)?)
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, ForgeServiceError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(ForgeServiceError::Local(format!(
            "read {}: {error}",
            path.display()
        ))),
    }
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), ForgeServiceError> {
    let bytes = serde_json::to_vec_pretty(value)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn home() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    #[tokio::test]
    async fn a_key_is_validated_stored_and_never_returned() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": {"viewer": {"name": "Alice", "email": "alice@example.test", "organization": {"name": "Acme"}}}
            })))
            .mount(&server)
            .await;
        let dir = home();
        let service =
            LinearService::open_with_endpoint(dir.path(), &format!("{}/graphql", server.uri()))
                .unwrap();
        let key = ["fixture", "lin", "ear"].join("-");
        let account = service.connect(&key).await.unwrap();
        assert_eq!(account.name, "Acme");
        let encoded = serde_json::to_string(&account).unwrap();
        assert!(!encoded.contains(&key));
        assert_eq!(service.list_accounts().len(), 1);
    }

    #[tokio::test]
    async fn removing_a_connection_drops_its_project_choices() {
        let dir = home();
        let service = LinearService::open(dir.path()).unwrap();
        {
            let mut accounts = service.accounts.write().unwrap();
            accounts.push(LinearAccount {
                account_id: "lin_1".into(),
                name: "Acme".into(),
                email: None,
                created_at: Utc::now(),
            });
        }
        {
            let mut projects = service.projects.write().unwrap();
            projects.push(LinearProjectTeam {
                project_id: "proj_1".into(),
                account_id: "lin_1".into(),
                team_id: "team_1".into(),
                team_key: "ENG".into(),
                team_name: "Engineering".into(),
            });
        }
        service.remove("lin_1").unwrap();
        assert!(service.project_teams().is_empty());
    }
}
