//! OAuth grants for outside agents.
//!
//! An outside agent is an MCP client h4rn did not start: Claude Code in the
//! person's own terminal, Codex, ChatGPT, a bot. It connects with OAuth 2.1
//! authorization code + PKCE, after the person approves a level on the
//! daemon's approval page. Every token is stored as a BLAKE3 digest, so a copy
//! of the store is not a copy of the credentials.
//!
//! Tokens are scoped (read-only, supervised, full access), expiring (one hour
//! for access, thirty days for refresh) and revocable per client. The daemon
//! checks the scope again on every tool call; nothing the client says about
//! its own authority is trusted.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ::url::Url;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, TimeDelta, Utc};
use h4rn_protocol::{OutsideAgentLevel, OutsideAgentView};
use rand::RngCore;
use serde::{Deserialize, Serialize};

use super::url;
use super::url::redirect_allowed;

/// How long an access token works.
pub const ACCESS_TTL: TimeDelta = TimeDelta::hours(1);
/// How long a refresh token works.
pub const REFRESH_TTL: TimeDelta = TimeDelta::days(30);
/// How long an authorization code works.
pub const CODE_TTL: TimeDelta = TimeDelta::minutes(10);
/// How long an approval page stays open.
pub const PENDING_TTL: TimeDelta = TimeDelta::minutes(10);

const FILE: &str = "outside_agents.json";
const MAX_REDIRECT_URIS: usize = 8;
const MAX_NAME_CHARS: usize = 80;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TokenKind {
    Access,
    Refresh,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ClientRecord {
    client_id: String,
    name: String,
    redirect_uris: Vec<String>,
    created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TokenRecord {
    hash: String,
    kind: TokenKind,
    client_id: String,
    level: OutsideAgentLevel,
    family: String,
    issued_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    #[serde(default)]
    revoked_at: Option<DateTime<Utc>>,
    #[serde(default)]
    last_used_at: Option<DateTime<Utc>>,
}

impl TokenRecord {
    fn live(&self, now: DateTime<Utc>) -> bool {
        self.revoked_at.is_none() && self.expires_at > now
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CodeRecord {
    hash: String,
    client_id: String,
    redirect_uri: String,
    challenge: String,
    level: OutsideAgentLevel,
    expires_at: DateTime<Utc>,
    #[serde(default)]
    used_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// One authorization request waiting on the approval page.
pub struct PendingRecord {
    /// Approval id handed to the page.
    pub id: String,
    client_id: String,
    redirect_uri: String,
    #[serde(default)]
    state: Option<String>,
    challenge: String,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default)]
    clients: Vec<ClientRecord>,
    #[serde(default)]
    tokens: Vec<TokenRecord>,
    #[serde(default)]
    codes: Vec<CodeRecord>,
    #[serde(default)]
    pending: Vec<PendingRecord>,
}

/// A failure that becomes an OAuth error response.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OAuthError {
    /// The client id is unknown.
    #[error("the client is not registered")]
    InvalidClient,
    /// The redirect URI does not belong to the client or fails the allow-list.
    #[error("{0}")]
    InvalidRedirect(String),
    /// A code, refresh token or verifier did not check out.
    #[error("{0}")]
    InvalidGrant(String),
    /// The request itself is malformed.
    #[error("{0}")]
    InvalidRequest(String),
    /// The approval page had no such request, or it expired.
    #[error("{0}")]
    NotFound(String),
    /// The store could not be written.
    #[error("the daemon could not save the grant: {0}")]
    Storage(String),
}

impl OAuthError {
    /// The OAuth `error` code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidClient => "invalid_client",
            Self::InvalidRedirect(_) | Self::InvalidRequest(_) => "invalid_request",
            Self::InvalidGrant(_) => "invalid_grant",
            Self::NotFound(_) => "invalid_request",
            Self::Storage(_) => "server_error",
        }
    }

    /// The human-readable description.
    pub fn description(&self) -> String {
        self.to_string()
    }
}

/// A newly issued access/refresh pair.
#[derive(Clone, Debug)]
pub struct TokenPair {
    /// Bearer value for MCP requests.
    pub access_token: String,
    /// Value used to renew the access token.
    pub refresh_token: String,
    /// Access lifetime in seconds.
    pub expires_in: u64,
    /// Level the pair carries.
    pub level: OutsideAgentLevel,
}

/// The authority behind one valid bearer token.
#[derive(Clone, Debug)]
pub struct Grant {
    /// Public client id.
    pub client_id: String,
    /// Registered client name.
    pub client_name: String,
    /// Chosen level.
    pub level: OutsideAgentLevel,
}

/// One client registration.
#[derive(Clone, Debug)]
pub struct RegisteredClient {
    /// Public client id.
    pub client_id: String,
    /// Display name.
    pub name: String,
    /// Admitted redirect URIs.
    pub redirect_uris: Vec<String>,
}

/// Where an approval or denial sends the browser.
#[derive(Clone, Debug)]
pub struct ApprovalRedirect {
    /// Absolute redirect URI with `code` or `error` and the state.
    pub location: String,
}

/// Durable OAuth state.
pub struct OAuthStore {
    path: PathBuf,
    file: Mutex<StoreFile>,
    last_use_write: Mutex<Instant>,
}

impl OAuthStore {
    /// Opens the store, treating a missing file as empty.
    pub fn open(data_dir: &Path) -> std::io::Result<Self> {
        let path = data_dir.join(FILE);
        let file: StoreFile = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(std::io::Error::other)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => StoreFile::default(),
            Err(error) => return Err(error),
        };
        Ok(Self {
            path,
            file: Mutex::new(file),
            last_use_write: Mutex::new(Instant::now() - Duration::from_secs(120)),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, StoreFile> {
        self.file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn persist(&self, file: &StoreFile) -> Result<(), OAuthError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| OAuthError::Storage(error.to_string()))?;
        }
        let bytes = serde_json::to_vec_pretty(file)
            .map_err(|error| OAuthError::Storage(error.to_string()))?;
        let temp = self.path.with_extension("tmp");
        std::fs::write(&temp, bytes).map_err(|error| OAuthError::Storage(error.to_string()))?;
        std::fs::rename(&temp, &self.path).map_err(|error| OAuthError::Storage(error.to_string()))
    }

    /// Registers a client (dynamic client registration) and returns its id.
    pub fn register_client(
        &self,
        name: Option<&str>,
        redirect_uris: &[String],
    ) -> Result<RegisteredClient, OAuthError> {
        if redirect_uris.is_empty() {
            return Err(OAuthError::InvalidRedirect(
                "at least one redirect_uri is required".into(),
            ));
        }
        if redirect_uris.len() > MAX_REDIRECT_URIS {
            return Err(OAuthError::InvalidRedirect(format!(
                "at most {MAX_REDIRECT_URIS} redirect URIs are allowed"
            )));
        }
        let mut admitted = Vec::new();
        for uri in redirect_uris {
            if !redirect_allowed(uri) {
                return Err(OAuthError::InvalidRedirect(format!(
                    "redirect_uri `{uri}` is not allowed: use https or a localhost address"
                )));
            }
            if !admitted.contains(uri) {
                admitted.push(uri.clone());
            }
        }
        let name = name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.chars().take(MAX_NAME_CHARS).collect::<String>())
            .unwrap_or_else(|| "An MCP client".to_owned());
        let client = ClientRecord {
            client_id: format!("h4rn_client_{}", random_hex()),
            name,
            redirect_uris: admitted,
            created_at: Utc::now(),
        };
        let mut file = self.lock();
        file.clients.push(client.clone());
        self.persist(&file)?;
        Ok(RegisteredClient {
            client_id: client.client_id,
            name: client.name,
            redirect_uris: client.redirect_uris,
        })
    }

    fn client(&self, client_id: &str) -> Option<ClientRecord> {
        self.lock()
            .clients
            .iter()
            .find(|client| client.client_id == client_id)
            .cloned()
    }

    /// The display name of a registered client, for the approval page.
    pub fn client_name(&self, client_id: &str) -> Option<String> {
        self.client(client_id).map(|client| client.name)
    }

    /// Starts an authorization request and returns the approval page id.
    pub fn begin_authorization(
        &self,
        client_id: &str,
        redirect_uri: &str,
        state: Option<&str>,
        challenge: &str,
    ) -> Result<PendingRecord, OAuthError> {
        let client = self.client(client_id).ok_or(OAuthError::InvalidClient)?;
        if !client.redirect_uris.iter().any(|uri| uri == redirect_uri) {
            return Err(OAuthError::InvalidRedirect(
                "redirect_uri is not registered for this client".into(),
            ));
        }
        if !url::valid_s256_challenge(challenge) {
            return Err(OAuthError::InvalidRequest(
                "code_challenge must be a base64url S256 digest (43 characters)".into(),
            ));
        }
        let pending = PendingRecord {
            id: format!("h4rn_auth_{}", random_hex()),
            client_id: client.client_id,
            redirect_uri: redirect_uri.to_owned(),
            state: state.map(str::to_owned),
            challenge: challenge.to_owned(),
            created_at: Utc::now(),
            expires_at: Utc::now() + PENDING_TTL,
        };
        let mut file = self.lock();
        file.pending.retain(|entry| entry.expires_at > Utc::now());
        file.pending.push(pending.clone());
        self.persist(&file)?;
        Ok(pending)
    }

    /// One pending approval, when it is still open.
    pub fn pending(&self, id: &str) -> Option<PendingRecord> {
        self.lock()
            .pending
            .iter()
            .find(|entry| entry.id == id && entry.expires_at > Utc::now())
            .cloned()
    }

    /// Consumes an approval and issues a one-time code.
    pub fn approve(
        &self,
        id: &str,
        level: OutsideAgentLevel,
    ) -> Result<ApprovalRedirect, OAuthError> {
        let now = Utc::now();
        let mut file = self.lock();
        let Some(index) = file
            .pending
            .iter()
            .position(|entry| entry.id == id && entry.expires_at > now)
        else {
            return Err(OAuthError::NotFound(
                "this approval request is no longer open; start the sign-in again".into(),
            ));
        };
        let pending = file.pending.remove(index);
        let code = format!("h4rn_code_{}", random_hex());
        file.codes.push(CodeRecord {
            hash: digest(&code),
            client_id: pending.client_id.clone(),
            redirect_uri: pending.redirect_uri.clone(),
            challenge: pending.challenge.clone(),
            level,
            expires_at: now + CODE_TTL,
            used_at: None,
        });
        // Drop anything that expired while the page was open.
        file.codes.retain(|entry| entry.expires_at > now);
        self.persist(&file)?;
        let location = redirect_with(
            &pending.redirect_uri,
            &[("code", code.as_str())],
            pending.state.as_deref(),
        )?;
        Ok(ApprovalRedirect { location })
    }

    /// Consumes an approval and returns the client's error redirect.
    pub fn deny(&self, id: &str) -> Result<ApprovalRedirect, OAuthError> {
        let now = Utc::now();
        let mut file = self.lock();
        let Some(index) = file
            .pending
            .iter()
            .position(|entry| entry.id == id && entry.expires_at > now)
        else {
            return Err(OAuthError::NotFound(
                "this approval request is no longer open; start the sign-in again".into(),
            ));
        };
        let pending = file.pending.remove(index);
        self.persist(&file)?;
        let location = redirect_with(
            &pending.redirect_uri,
            &[("error", "access_denied")],
            pending.state.as_deref(),
        )?;
        Ok(ApprovalRedirect { location })
    }

    /// Exchanges an authorization code for a token pair.
    pub fn exchange_code(
        &self,
        code: &str,
        verifier: &str,
        client_id: &str,
        redirect_uri: &str,
    ) -> Result<TokenPair, OAuthError> {
        if !url::valid_verifier(verifier) {
            return Err(OAuthError::InvalidGrant(
                "code_verifier must be 43 to 128 characters of A-Z, a-z, 0-9, '-', '.', '_' or '~'"
                    .into(),
            ));
        }
        let now = Utc::now();
        let hash = digest(code);
        let mut file = self.lock();
        let Some(index) = file.codes.iter().position(|entry| entry.hash == hash) else {
            return Err(OAuthError::InvalidGrant(
                "the authorization code is unknown or was already used".into(),
            ));
        };
        let record = file.codes[index].clone();
        if record.used_at.is_some() {
            return Err(OAuthError::InvalidGrant(
                "the authorization code was already used".into(),
            ));
        }
        if record.expires_at <= now {
            return Err(OAuthError::InvalidGrant(
                "the authorization code expired; sign in again".into(),
            ));
        }
        if record.client_id != client_id {
            return Err(OAuthError::InvalidGrant(
                "the authorization code belongs to another client".into(),
            ));
        }
        if record.redirect_uri != redirect_uri {
            return Err(OAuthError::InvalidGrant(
                "redirect_uri does not match the authorization request".into(),
            ));
        }
        if !url::challenge_matches(verifier, &record.challenge) {
            return Err(OAuthError::InvalidGrant(
                "code_verifier does not match the PKCE challenge".into(),
            ));
        }
        file.codes[index].used_at = Some(now);
        let pair = issue_pair(&mut file, &record.client_id, record.level, now, None);
        self.persist(&file)?;
        Ok(pair)
    }

    /// Renews an access token with a refresh token, rotating the refresh token.
    pub fn refresh(&self, refresh_token: &str, client_id: &str) -> Result<TokenPair, OAuthError> {
        let now = Utc::now();
        let hash = digest(refresh_token);
        let mut file = self.lock();
        let Some(index) = file.tokens.iter().position(|entry| entry.hash == hash) else {
            return Err(OAuthError::InvalidGrant(
                "the refresh token is unknown".into(),
            ));
        };
        let record = file.tokens[index].clone();
        if record.kind != TokenKind::Refresh || record.client_id != client_id || !record.live(now) {
            return Err(OAuthError::InvalidGrant(
                "the refresh token is expired, revoked or belongs to another client".into(),
            ));
        }
        file.tokens[index].revoked_at = Some(now);
        let pair = issue_pair(
            &mut file,
            &record.client_id,
            record.level,
            now,
            Some(record.family),
        );
        self.persist(&file)?;
        Ok(pair)
    }

    /// Verifies a bearer token and records its use.
    pub fn verify_access(&self, token: &str) -> Option<Grant> {
        let now = Utc::now();
        let hash = digest(token);
        let (grant, client_name) = {
            let mut file = self.lock();
            let index = file
                .tokens
                .iter()
                .position(|entry| entry.hash == hash && entry.kind == TokenKind::Access)?;
            if !file.tokens[index].live(now) {
                return None;
            }
            file.tokens[index].last_used_at = Some(now);
            let record = file.tokens[index].clone();
            let name = file
                .clients
                .iter()
                .find(|client| client.client_id == record.client_id)
                .map(|client| client.name.clone())
                .unwrap_or_else(|| "An MCP client".into());
            (
                Grant {
                    client_id: record.client_id,
                    client_name: name.clone(),
                    level: record.level,
                },
                name,
            )
        };
        let _ = client_name;
        self.persist_use_if_due();
        Some(grant)
    }

    /// Persists a last-used timestamp at most once every thirty seconds.
    fn persist_use_if_due(&self) {
        let mut last = self
            .last_use_write
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if last.elapsed() < Duration::from_secs(30) {
            return;
        }
        *last = Instant::now();
        let snapshot = self.lock().clone();
        if let Err(error) = self.persist(&snapshot) {
            tracing::warn!(%error, "could not record an outside agent's last use");
        }
    }

    /// Revokes a token and every token in its family.
    pub fn revoke_token(&self, token: &str) -> Result<(), OAuthError> {
        let now = Utc::now();
        let hash = digest(token);
        let mut file = self.lock();
        let family = file
            .tokens
            .iter()
            .find(|entry| entry.hash == hash)
            .map(|entry| entry.family.clone());
        let Some(family) = family else {
            // RFC 7009: an unknown token is not an error.
            return Ok(());
        };
        for entry in file
            .tokens
            .iter_mut()
            .filter(|entry| entry.family == family)
        {
            entry.revoked_at = Some(now);
        }
        self.persist(&file)
    }

    /// Revokes every token for a client, and its codes and approvals.
    pub fn revoke_client(&self, client_id: &str) -> Result<(), OAuthError> {
        let now = Utc::now();
        let mut file = self.lock();
        let mut found = false;
        for entry in file
            .tokens
            .iter_mut()
            .filter(|entry| entry.client_id == client_id)
        {
            entry.revoked_at = Some(now);
            found = true;
        }
        file.codes.retain(|entry| entry.client_id != client_id);
        file.pending.retain(|entry| entry.client_id != client_id);
        if !found
            && !file
                .clients
                .iter()
                .any(|client| client.client_id == client_id)
        {
            return Err(OAuthError::NotFound(
                "no outside agent has that client id".into(),
            ));
        }
        self.persist(&file)
    }

    /// Every client with a live token, newest first.
    pub fn connected_agents(&self) -> Vec<OutsideAgentView> {
        let now = Utc::now();
        let file = self.lock();
        let mut agents: HashMap<String, OutsideAgentView> = HashMap::new();
        for token in file.tokens.iter().filter(|token| token.live(now)) {
            let Some(client) = file
                .clients
                .iter()
                .find(|client| client.client_id == token.client_id)
            else {
                continue;
            };
            agents
                .entry(token.client_id.clone())
                .and_modify(|view| {
                    if level_rank(token.level) > level_rank(view.level) {
                        view.level = token.level;
                    }
                    if token.issued_at < view.connected_at {
                        view.connected_at = token.issued_at;
                    }
                    if token.expires_at > view.expires_at {
                        view.expires_at = token.expires_at;
                    }
                    if let Some(used) = token.last_used_at
                        && view.last_used_at.is_none_or(|current| used > current)
                    {
                        view.last_used_at = Some(used);
                    }
                })
                .or_insert(OutsideAgentView {
                    client_id: client.client_id.clone(),
                    name: client.name.clone(),
                    level: token.level,
                    connected_at: token.issued_at,
                    last_used_at: token.last_used_at,
                    expires_at: token.expires_at,
                });
        }
        let mut agents: Vec<_> = agents.into_values().collect();
        agents.sort_by_key(|agent| {
            std::cmp::Reverse(agent.last_used_at.unwrap_or(agent.connected_at))
        });
        agents
    }

    /// Number of live grants, for diagnostics and tests.
    pub fn live_token_count(&self) -> usize {
        let now = Utc::now();
        self.lock()
            .tokens
            .iter()
            .filter(|token| token.live(now))
            .count()
    }
}

fn level_rank(level: OutsideAgentLevel) -> u8 {
    match level {
        OutsideAgentLevel::ReadOnly => 0,
        OutsideAgentLevel::Supervised => 1,
        OutsideAgentLevel::FullAccess => 2,
    }
}

fn issue_pair(
    file: &mut StoreFile,
    client_id: &str,
    level: OutsideAgentLevel,
    now: DateTime<Utc>,
    family: Option<String>,
) -> TokenPair {
    let family = family.unwrap_or_else(random_hex);
    let access_token = format!("h4rn_at_{}", random_hex());
    let refresh_token = format!("h4rn_rt_{}", random_hex());
    file.tokens.push(TokenRecord {
        hash: digest(&access_token),
        kind: TokenKind::Access,
        client_id: client_id.to_owned(),
        level,
        family: family.clone(),
        issued_at: now,
        expires_at: now + ACCESS_TTL,
        revoked_at: None,
        last_used_at: None,
    });
    file.tokens.push(TokenRecord {
        hash: digest(&refresh_token),
        kind: TokenKind::Refresh,
        client_id: client_id.to_owned(),
        level,
        family,
        issued_at: now,
        expires_at: now + REFRESH_TTL,
        revoked_at: None,
        last_used_at: None,
    });
    // Keep the file bounded: an expired token older than sixty days says
    // nothing a person still needs. Revoked tokens stay long enough for an
    // in-flight call to see the revocation rather than a "unknown token".
    file.tokens
        .retain(|token| token.expires_at > now - TimeDelta::days(60));
    file.codes.retain(|code| code.expires_at > now);
    file.pending.retain(|pending| pending.expires_at > now);
    TokenPair {
        access_token,
        refresh_token,
        expires_in: ACCESS_TTL.num_seconds() as u64,
        level,
    }
}

fn redirect_with(
    base: &str,
    parameters: &[(&str, &str)],
    state: Option<&str>,
) -> Result<String, OAuthError> {
    let mut url = Url::parse(base).map_err(|error| {
        OAuthError::InvalidRedirect(format!("redirect_uri is not a URL: {error}"))
    })?;
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in parameters {
            query.append_pair(key, value);
        }
        if let Some(state) = state {
            query.append_pair("state", state);
        }
    }
    Ok(url.to_string())
}

fn random_hex() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn digest(value: &str) -> String {
    blake3::hash(value.as_bytes()).to_hex().to_string()
}

/// The digest a token is stored under. Integration tests use this to prove
/// the store keeps digests and never the bearer value.
#[doc(hidden)]
pub fn digest_for_test(value: &str) -> String {
    digest(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, OAuthStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = OAuthStore::open(dir.path()).expect("store");
        (dir, store)
    }

    fn challenge() -> (String, String) {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".to_owned();
        let challenge = url::s256(&verifier);
        (verifier, challenge)
    }

    #[test]
    fn registration_stores_only_the_public_client_id() {
        let (dir, store) = store();
        let client = store
            .register_client(Some("Claude Code"), &["http://localhost:1455/cb".into()])
            .expect("register");
        let raw = std::fs::read_to_string(dir.path().join(FILE)).expect("file");
        assert!(raw.contains(&client.client_id));
        assert!(!raw.to_lowercase().contains("secret"));
    }

    #[test]
    fn pkce_success_issues_a_live_access_token() {
        let (_dir, store) = store();
        let client = store
            .register_client(Some("Codex"), &["http://localhost:1455/cb".into()])
            .expect("register");
        let (verifier, challenge) = challenge();
        let pending = store
            .begin_authorization(
                &client.client_id,
                "http://localhost:1455/cb",
                Some("state-1"),
                &challenge,
            )
            .expect("begin");
        let redirect = store
            .approve(&pending.id, OutsideAgentLevel::Supervised)
            .expect("approve");
        let code = Url::parse(&redirect.location)
            .expect("url")
            .query_pairs()
            .find(|(key, _)| key == "code")
            .map(|(_, value)| value.to_string())
            .expect("code");
        let pair = store
            .exchange_code(
                &code,
                &verifier,
                &client.client_id,
                "http://localhost:1455/cb",
            )
            .expect("exchange");
        assert_eq!(pair.level, OutsideAgentLevel::Supervised);
        let grant = store.verify_access(&pair.access_token).expect("grant");
        assert_eq!(grant.level, OutsideAgentLevel::Supervised);
        assert_eq!(grant.client_name, "Codex");
    }

    #[test]
    fn a_code_cannot_be_used_twice() {
        let (_dir, store) = store();
        let client = store
            .register_client(None, &["http://localhost:1455/cb".into()])
            .expect("register");
        let (verifier, challenge) = challenge();
        let pending = store
            .begin_authorization(
                &client.client_id,
                "http://localhost:1455/cb",
                None,
                &challenge,
            )
            .expect("begin");
        let redirect = store
            .approve(&pending.id, OutsideAgentLevel::ReadOnly)
            .expect("approve");
        let code = Url::parse(&redirect.location)
            .unwrap()
            .query_pairs()
            .find(|(key, _)| key == "code")
            .unwrap()
            .1
            .to_string();
        store
            .exchange_code(
                &code,
                &verifier,
                &client.client_id,
                "http://localhost:1455/cb",
            )
            .expect("first use");
        assert!(
            store
                .exchange_code(
                    &code,
                    &verifier,
                    &client.client_id,
                    "http://localhost:1455/cb"
                )
                .is_err()
        );
    }

    #[test]
    fn a_wrong_verifier_fails_and_leaves_the_code_unused() {
        let (_dir, store) = store();
        let client = store
            .register_client(None, &["http://localhost:1455/cb".into()])
            .expect("register");
        let (verifier, challenge) = challenge();
        let pending = store
            .begin_authorization(
                &client.client_id,
                "http://localhost:1455/cb",
                None,
                &challenge,
            )
            .expect("begin");
        let redirect = store
            .approve(&pending.id, OutsideAgentLevel::ReadOnly)
            .expect("approve");
        let code = Url::parse(&redirect.location)
            .unwrap()
            .query_pairs()
            .find(|(key, _)| key == "code")
            .unwrap()
            .1
            .to_string();
        let wrong = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        assert!(
            store
                .exchange_code(&code, wrong, &client.client_id, "http://localhost:1455/cb")
                .is_err()
        );
        store
            .exchange_code(
                &code,
                &verifier,
                &client.client_id,
                "http://localhost:1455/cb",
            )
            .expect("right verifier still works");
    }

    #[test]
    fn revoking_a_client_kills_its_access_and_refresh_tokens() {
        let (_dir, store) = store();
        let client = store
            .register_client(None, &["http://localhost:1455/cb".into()])
            .expect("register");
        let (verifier, challenge) = challenge();
        let pending = store
            .begin_authorization(
                &client.client_id,
                "http://localhost:1455/cb",
                None,
                &challenge,
            )
            .expect("begin");
        let redirect = store
            .approve(&pending.id, OutsideAgentLevel::FullAccess)
            .expect("approve");
        let code = Url::parse(&redirect.location)
            .unwrap()
            .query_pairs()
            .find(|(key, _)| key == "code")
            .unwrap()
            .1
            .to_string();
        let pair = store
            .exchange_code(
                &code,
                &verifier,
                &client.client_id,
                "http://localhost:1455/cb",
            )
            .expect("exchange");
        assert_eq!(store.connected_agents().len(), 1);
        store.revoke_client(&client.client_id).expect("revoke");
        assert!(store.verify_access(&pair.access_token).is_none());
        assert!(
            store
                .refresh(&pair.refresh_token, &client.client_id)
                .is_err()
        );
        assert!(store.connected_agents().is_empty());
    }

    #[test]
    fn an_expired_access_token_is_refused() {
        let (_dir, store) = store();
        let client = store
            .register_client(None, &["http://localhost:1455/cb".into()])
            .expect("register");
        let (verifier, challenge) = challenge();
        let pending = store
            .begin_authorization(
                &client.client_id,
                "http://localhost:1455/cb",
                None,
                &challenge,
            )
            .expect("begin");
        let redirect = store
            .approve(&pending.id, OutsideAgentLevel::ReadOnly)
            .expect("approve");
        let code = Url::parse(&redirect.location)
            .unwrap()
            .query_pairs()
            .find(|(key, _)| key == "code")
            .unwrap()
            .1
            .to_string();
        let pair = store
            .exchange_code(
                &code,
                &verifier,
                &client.client_id,
                "http://localhost:1455/cb",
            )
            .expect("exchange");
        {
            let mut file = store.lock();
            for token in &mut file.tokens {
                if token.kind == TokenKind::Access {
                    token.expires_at = Utc::now() - TimeDelta::seconds(1);
                }
            }
        }
        assert!(store.verify_access(&pair.access_token).is_none());
        store
            .refresh(&pair.refresh_token, &client.client_id)
            .expect("refresh still works");
    }
}
