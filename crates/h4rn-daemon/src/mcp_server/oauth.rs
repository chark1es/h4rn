//! OAuth grants for outside agents.
//!
//! An outside agent is an MCP client h4rn did not start: Claude Code in the
//! person's own terminal, Codex, ChatGPT, a bot. It connects with OAuth 2.1
//! authorization code + PKCE, after the person approves a level inside h4rn.
//! Every token is stored as a BLAKE3 digest, so a copy of the store is not a
//! copy of the credentials.
//!
//! Tokens are scoped (read-only, supervised, full access), expiring (one hour
//! for access, thirty days for refresh) and revocable per client. The daemon
//! checks the scope again on every tool call; nothing the client says about
//! its own authority is trusted.
//!
//! Two invariants shape the store:
//!
//! - **One lock covers the state and its write.** A revocation, a spent code
//!   or a rotated token is written before the lock is released, so a slower
//!   writer can never put a stale snapshot back on disk. The file is replaced
//!   atomically.
//! - **A replay is an attack, not a retry.** Presenting a code or a rotated
//!   refresh token a second time revokes everything that came from it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ::url::Url;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, TimeDelta, Utc};
use h4rn_protocol::{OutsideAgentLevel, OutsideAgentSignIn, OutsideAgentView};
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
/// How long an unused registration is kept.
pub const UNUSED_CLIENT_TTL: TimeDelta = TimeDelta::hours(24);

const FILE: &str = "outside_agents.json";
const MAX_REDIRECT_URIS: usize = 8;
const MAX_NAME_CHARS: usize = 80;
/// Registered clients kept at once. Dynamic registration is unauthenticated, so
/// the list must be bounded; an unused registration expires first.
pub const MAX_CLIENTS: usize = 32;
/// Sign-in requests waiting for the person at once, across every client.
///
/// The authorize endpoint is unauthenticated, so a process that reaches the
/// loopback listener must not be able to bury the person's real request in
/// thousands of fakes.
pub const MAX_PENDING: usize = 16;
/// Sign-in requests one client may have waiting at once.
pub const MAX_PENDING_PER_CLIENT: usize = 3;
/// How often the last-used timestamp is written.
const LAST_USE_INTERVAL: Duration = Duration::from_secs(30);

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
    /// RFC 8707 resource this token was issued for, when the client named one.
    #[serde(default)]
    resource: Option<String>,
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
    /// Family of the pair this code issued, so a replay revokes it.
    #[serde(default)]
    family: Option<String>,
    #[serde(default)]
    resource: Option<String>,
    expires_at: DateTime<Utc>,
    #[serde(default)]
    used_at: Option<DateTime<Utc>>,
}

/// What the browser is eventually sent to.
///
/// The location carries a live authorization code, so it lives in memory only:
/// a copy of the store is never a copy of a code, and a restart forgets the
/// answer (the browser is told to start the sign-in again).
#[derive(Clone, Debug)]
struct PendingResolution {
    location: String,
    at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// One authorization request waiting on the person in h4rn.
pub struct PendingRecord {
    /// Approval id handed to the page.
    pub id: String,
    /// Client that asked.
    pub client_id: String,
    /// Exact callback the browser will be sent to.
    pub redirect_uri: String,
    redirect_origin: String,
    #[serde(default)]
    state: Option<String>,
    challenge: String,
    #[serde(default)]
    resource: Option<String>,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    /// Set once the person answered. The answer itself (which carries the
    /// code) is kept in memory, never written.
    #[serde(default)]
    resolved_at: Option<DateTime<Utc>>,
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
    /// The `resource` parameter names something this daemon does not serve.
    #[error("{0}")]
    InvalidTarget(String),
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
            Self::InvalidTarget(_) => "invalid_target",
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
    /// Resource the pair is bound to, when the client named one.
    pub resource: Option<String>,
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
    /// RFC 8707 resource the token is bound to, if any.
    pub resource: Option<String>,
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
    /// The whole state and the write of it share one lock. A snapshot is never
    /// taken, released and written later: that is what used to let a revocation
    /// lose a race with a last-used update.
    state: Mutex<StoreState>,
    /// Where an answered approval sends the browser, by request id.
    ///
    /// Memory only: the location holds the one-time code. Lock order is always
    /// `state` then `resolutions`, never the reverse.
    resolutions: Mutex<HashMap<String, PendingResolution>>,
}

struct StoreState {
    file: StoreFile,
    last_use_write: Instant,
}

impl OAuthStore {
    /// Opens the store, treating a missing file as empty.
    ///
    /// A file the daemon cannot read is quarantined instead of stopping the
    /// daemon: every grant in it is lost, which is the safe direction, and the
    /// person is told where the old file went.
    pub fn open(data_dir: &Path) -> std::io::Result<Self> {
        let path = data_dir.join(FILE);
        let file = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<StoreFile>(&bytes) {
                Ok(file) => file,
                Err(error) => {
                    let quarantine = quarantine_path(&path);
                    // Never log the serde detail: a data error quotes the
                    // unexpected value, and that value can be a credential.
                    let kind = match error.classify() {
                        serde_json::error::Category::Io => "a read failure",
                        serde_json::error::Category::Syntax => "invalid JSON",
                        serde_json::error::Category::Data => "an unexpected shape",
                        serde_json::error::Category::Eof => "a truncated file",
                    };
                    tracing::warn!(
                        kind,
                        line = error.line(),
                        column = error.column(),
                        path = %path.display(),
                        quarantined = %quarantine.display(),
                        "the outside-agent grant file is unreadable; starting with no grants"
                    );
                    // A best-effort move: if it fails, the empty store still
                    // starts and the unreadable file stays for inspection.
                    let _ = std::fs::rename(&path, &quarantine);
                    StoreFile::default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => StoreFile::default(),
            Err(error) => return Err(error),
        };
        Ok(Self {
            path,
            state: Mutex::new(StoreState {
                file,
                last_use_write: Instant::now() - LAST_USE_INTERVAL,
            }),
            resolutions: Mutex::new(HashMap::new()),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, StoreState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Writes the live state atomically. Called with the state lock held.
    fn persist_locked(&self, file: &StoreFile) -> Result<(), OAuthError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| OAuthError::Storage(error.to_string()))?;
        }
        let bytes = serde_json::to_vec_pretty(file)
            .map_err(|error| OAuthError::Storage(error.to_string()))?;
        let temp = self.path.with_extension("tmp");
        {
            use std::io::Write;
            let mut handle = std::fs::File::create(&temp)
                .map_err(|error| OAuthError::Storage(error.to_string()))?;
            handle
                .write_all(&bytes)
                .map_err(|error| OAuthError::Storage(error.to_string()))?;
            // A revocation that is only in the page cache is not a revocation.
            handle
                .sync_all()
                .map_err(|error| OAuthError::Storage(error.to_string()))?;
        }
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
        let now = Utc::now();
        let mut state = self.lock();
        prune_clients(&mut state.file, now);
        if state.file.clients.len() >= MAX_CLIENTS {
            return Err(OAuthError::InvalidRequest(format!(
                "at most {MAX_CLIENTS} outside clients can be registered at once; revoke one in h4rn"
            )));
        }
        let client = ClientRecord {
            client_id: format!("h4rn_client_{}", random_hex()),
            name,
            redirect_uris: admitted,
            created_at: now,
        };
        state.file.clients.push(client.clone());
        self.persist_locked(&state.file)?;
        Ok(RegisteredClient {
            client_id: client.client_id,
            name: client.name,
            redirect_uris: client.redirect_uris,
        })
    }

    fn client_locked<'a>(file: &'a StoreFile, client_id: &str) -> Option<&'a ClientRecord> {
        file.clients
            .iter()
            .find(|client| client.client_id == client_id)
    }

    fn client(&self, client_id: &str) -> Option<ClientRecord> {
        Self::client_locked(&self.lock().file, client_id).cloned()
    }

    /// Whether `client_id` names a registered client.
    ///
    /// Rate-limit buckets are keyed on this: a bucket exists only for an id
    /// that survived registration, never for an attacker's random string.
    pub fn client_exists(&self, client_id: &str) -> bool {
        Self::client_locked(&self.lock().file, client_id).is_some()
    }

    /// The display name of a registered client, for the approval page.
    pub fn client_name(&self, client_id: &str) -> Option<String> {
        self.client(client_id).map(|client| client.name)
    }

    /// Starts an authorization request and returns the approval page id.
    ///
    /// `resource` is the RFC 8707 resource the caller named, already validated
    /// against this daemon by the caller.
    pub fn begin_authorization(
        &self,
        client_id: &str,
        redirect_uri: &str,
        state: Option<&str>,
        challenge: &str,
        resource: Option<&str>,
    ) -> Result<PendingRecord, OAuthError> {
        let now = Utc::now();
        let mut guard = self.lock();
        let client = Self::client_locked(&guard.file, client_id)
            .ok_or(OAuthError::InvalidClient)?
            .clone();
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
        let redirect_origin = url::origin_of(redirect_uri).ok_or_else(|| {
            OAuthError::InvalidRedirect(format!("redirect_uri `{redirect_uri}` is not a URL"))
        })?;
        guard.file.pending.retain(|entry| entry.expires_at > now);
        // The authorize endpoint is unauthenticated. Cap what one client, and
        // all clients together, may leave waiting for the person: a flood must
        // not bury the request that is really theirs.
        let waiting = guard
            .file
            .pending
            .iter()
            .filter(|entry| entry.resolved_at.is_none())
            .count();
        if waiting >= MAX_PENDING {
            return Err(OAuthError::InvalidRequest(format!(
                "at most {MAX_PENDING} sign-in requests can wait for approval at once; answer or deny one in h4rn"
            )));
        }
        let waiting_for_client = guard
            .file
            .pending
            .iter()
            .filter(|entry| entry.resolved_at.is_none() && entry.client_id == client.client_id)
            .count();
        if waiting_for_client >= MAX_PENDING_PER_CLIENT {
            return Err(OAuthError::InvalidRequest(format!(
                "this client already has {MAX_PENDING_PER_CLIENT} sign-in requests waiting; answer or deny one in h4rn"
            )));
        }
        let pending = PendingRecord {
            id: format!("h4rn_auth_{}", random_hex()),
            client_id: client.client_id,
            redirect_uri: redirect_uri.to_owned(),
            redirect_origin,
            state: state.map(str::to_owned),
            challenge: challenge.to_owned(),
            resource: resource.map(str::to_owned),
            created_at: now,
            expires_at: now + PENDING_TTL,
            resolved_at: None,
        };
        guard.file.pending.push(pending.clone());
        self.persist_locked(&guard.file)?;
        Ok(pending)
    }

    /// One pending approval, when it is still open.
    pub fn pending(&self, id: &str) -> Option<PendingRecord> {
        let now = Utc::now();
        self.lock()
            .file
            .pending
            .iter()
            .find(|entry| entry.id == id && entry.expires_at > now && entry.resolved_at.is_none())
            .cloned()
    }

    /// Sign-ins waiting for the person, oldest first.
    pub fn pending_sign_ins(&self) -> Vec<OutsideAgentSignIn> {
        let now = Utc::now();
        let guard = self.lock();
        let mut sign_ins: Vec<OutsideAgentSignIn> = guard
            .file
            .pending
            .iter()
            .filter(|entry| entry.resolved_at.is_none() && entry.expires_at > now)
            .map(|entry| OutsideAgentSignIn {
                request_id: entry.id.clone(),
                client_name: Self::client_locked(&guard.file, &entry.client_id)
                    .map(|client| client.name.clone())
                    .unwrap_or_else(|| "An MCP client".into()),
                redirect_uri: entry.redirect_uri.clone(),
                redirect_origin: entry.redirect_origin.clone(),
                requested_at: entry.created_at,
                expires_at: entry.expires_at,
            })
            .collect();
        sign_ins.sort_by_key(|entry| entry.requested_at);
        sign_ins
    }

    /// The browser's next stop once the person answered, if they have.
    pub fn approval_outcome(&self, id: &str) -> Option<ApprovalOutcome> {
        let now = Utc::now();
        {
            let state = self.lock();
            let entry = state
                .file
                .pending
                .iter()
                .find(|entry| entry.id == id && entry.expires_at > now)?;
            entry.resolved_at?;
        }
        let resolutions = self
            .resolutions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        resolutions.get(id).map(|resolution| ApprovalOutcome {
            location: resolution.location.clone(),
            decided_at: resolution.at,
        })
    }

    /// Consumes an approval and issues a one-time code.
    pub fn approve(
        &self,
        id: &str,
        level: OutsideAgentLevel,
    ) -> Result<ApprovalRedirect, OAuthError> {
        let now = Utc::now();
        let mut guard = self.lock();
        let Some(index) = guard
            .file
            .pending
            .iter()
            .position(|entry| entry.id == id && entry.expires_at > now)
        else {
            return Err(OAuthError::NotFound(
                "this approval request is no longer open; start the sign-in again".into(),
            ));
        };
        if guard.file.pending[index].resolved_at.is_some() {
            return Err(OAuthError::NotFound(
                "this approval was already answered; start the sign-in again".into(),
            ));
        }
        let pending = guard.file.pending[index].clone();
        let code = format!("h4rn_code_{}", random_hex());
        guard.file.codes.push(CodeRecord {
            hash: digest(&code),
            client_id: pending.client_id.clone(),
            redirect_uri: pending.redirect_uri.clone(),
            challenge: pending.challenge.clone(),
            level,
            family: None,
            resource: pending.resource.clone(),
            expires_at: now + CODE_TTL,
            used_at: None,
        });
        // Drop anything that expired while the page was open.
        guard.file.codes.retain(|entry| entry.expires_at > now);
        let location = redirect_with(
            &pending.redirect_uri,
            &[("code", code.as_str())],
            pending.state.as_deref(),
        )?;
        guard.file.pending[index].resolved_at = Some(now);
        self.persist_locked(&guard.file)?;
        // The answer is remembered in memory only: the redirect carries the
        // one-time code, and the store on disk must never hold it.
        self.remember_resolution(&guard.file, &pending.id, location.clone(), now);
        Ok(ApprovalRedirect { location })
    }

    /// Consumes an approval and returns the client's error redirect.
    pub fn deny(&self, id: &str) -> Result<ApprovalRedirect, OAuthError> {
        let now = Utc::now();
        let mut guard = self.lock();
        let Some(index) = guard
            .file
            .pending
            .iter()
            .position(|entry| entry.id == id && entry.expires_at > now)
        else {
            return Err(OAuthError::NotFound(
                "this approval request is no longer open; start the sign-in again".into(),
            ));
        };
        if guard.file.pending[index].resolved_at.is_some() {
            return Err(OAuthError::NotFound(
                "this approval was already answered; start the sign-in again".into(),
            ));
        }
        let pending = guard.file.pending[index].clone();
        let location = redirect_with(
            &pending.redirect_uri,
            &[("error", "access_denied")],
            pending.state.as_deref(),
        )?;
        guard.file.pending[index].resolved_at = Some(now);
        self.persist_locked(&guard.file)?;
        self.remember_resolution(&guard.file, &pending.id, location.clone(), now);
        Ok(ApprovalRedirect { location })
    }

    /// Remembers an answered request for the waiting page, in memory only.
    ///
    /// The same pass forgets answers whose request has expired, so a daemon
    /// that has been up for days does not accumulate them.
    fn remember_resolution(
        &self,
        file: &StoreFile,
        id: &str,
        location: String,
        now: DateTime<Utc>,
    ) {
        let mut resolutions = self
            .resolutions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        resolutions.insert(id.to_owned(), PendingResolution { location, at: now });
        resolutions.retain(|key, _| {
            file.pending
                .iter()
                .any(|entry| &entry.id == key && entry.expires_at > now)
        });
    }

    /// Exchanges an authorization code for a token pair.
    ///
    /// OAuth 2.1 no longer requires `redirect_uri` at the token endpoint: PKCE
    /// binds the request. When the client does send it, it must match the
    /// authorization request exactly.
    ///
    /// RFC 8707 is checked here too: a `resource` named at the token endpoint
    /// must be the one the code was approved for, and a grant that was bound to
    /// no resource cannot be widened by naming one now.
    pub fn exchange_code(
        &self,
        code: &str,
        verifier: &str,
        client_id: &str,
        redirect_uri: Option<&str>,
        resource: Option<&str>,
    ) -> Result<TokenPair, OAuthError> {
        if !url::valid_verifier(verifier) {
            return Err(OAuthError::InvalidGrant(
                "code_verifier must be 43 to 128 characters of A-Z, a-z, 0-9, '-', '.', '_' or '~'"
                    .into(),
            ));
        }
        let now = Utc::now();
        let hash = digest(code);
        let mut guard = self.lock();
        let Some(index) = guard.file.codes.iter().position(|entry| entry.hash == hash) else {
            return Err(OAuthError::InvalidGrant(
                "the authorization code is unknown or was already used".into(),
            ));
        };
        let record = guard.file.codes[index].clone();
        if record.used_at.is_some() {
            // A code the client already spent came back. Whatever pair it
            // issued is in an attacker's hands too: revoke it.
            if let Some(family) = record.family.as_deref() {
                revoke_family(&mut guard.file, family, now);
            }
            let _ = self.persist_locked(&guard.file);
            return Err(OAuthError::InvalidGrant(
                "the authorization code was already used; its tokens were revoked".into(),
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
        if let Some(supplied) = redirect_uri
            && record.redirect_uri != supplied
        {
            return Err(OAuthError::InvalidGrant(
                "redirect_uri does not match the authorization request".into(),
            ));
        }
        if let Some(requested) = resource
            && record.resource.as_deref() != Some(requested)
        {
            // The code is not spent: the client can retry the exchange with the
            // resource it was actually approved for (or none at all).
            return Err(OAuthError::InvalidTarget(
                "`resource` does not match the one this code was approved for".into(),
            ));
        }
        if !url::challenge_matches(verifier, &record.challenge) {
            return Err(OAuthError::InvalidGrant(
                "code_verifier does not match the PKCE challenge".into(),
            ));
        }
        guard.file.codes[index].used_at = Some(now);
        let family = random_hex();
        guard.file.codes[index].family = Some(family.clone());
        let pair = issue_pair(
            &mut guard.file,
            &record.client_id,
            record.level,
            now,
            Some(family),
            record.resource.clone(),
        );
        self.persist_locked(&guard.file)?;
        Ok(pair)
    }

    /// Renews an access token with a refresh token, rotating the refresh token.
    pub fn refresh(
        &self,
        refresh_token: &str,
        client_id: &str,
        resource: Option<&str>,
    ) -> Result<TokenPair, OAuthError> {
        let now = Utc::now();
        let hash = digest(refresh_token);
        let mut guard = self.lock();
        let Some(index) = guard
            .file
            .tokens
            .iter()
            .position(|entry| entry.hash == hash)
        else {
            return Err(OAuthError::InvalidGrant(
                "the refresh token is unknown".into(),
            ));
        };
        let record = guard.file.tokens[index].clone();
        if record.kind != TokenKind::Refresh || record.client_id != client_id || !record.live(now) {
            // A rotated refresh token presented again means two parties hold
            // it: kill the family rather than let either continue.
            if record.kind == TokenKind::Refresh && record.client_id == client_id {
                revoke_family(&mut guard.file, &record.family, now);
                let _ = self.persist_locked(&guard.file);
                return Err(OAuthError::InvalidGrant(
                    "the refresh token was already used; its tokens were revoked".into(),
                ));
            }
            return Err(OAuthError::InvalidGrant(
                "the refresh token is expired, revoked or belongs to another client".into(),
            ));
        }
        if let Some(requested) = resource
            && record
                .resource
                .as_deref()
                .is_some_and(|bound| bound != requested)
        {
            return Err(OAuthError::InvalidTarget(
                "this grant is bound to a different resource".into(),
            ));
        }
        guard.file.tokens[index].revoked_at = Some(now);
        let pair = issue_pair(
            &mut guard.file,
            &record.client_id,
            record.level,
            now,
            Some(record.family),
            record.resource.clone(),
        );
        self.persist_locked(&guard.file)?;
        Ok(pair)
    }

    /// Verifies a bearer token and records its use.
    ///
    /// `expected_resource` is the resource the endpoint serves. A token bound to
    /// another resource is refused (RFC 8707).
    pub fn verify_access(&self, token: &str, expected_resource: Option<&str>) -> Option<Grant> {
        let now = Utc::now();
        let hash = digest(token);
        let mut guard = self.lock();
        let index = guard
            .file
            .tokens
            .iter()
            .position(|entry| entry.hash == hash && entry.kind == TokenKind::Access)?;
        {
            let record = &guard.file.tokens[index];
            if !record.live(now) {
                return None;
            }
            if let (Some(stored), Some(expected)) = (record.resource.as_deref(), expected_resource)
                && stored != expected
            {
                return None;
            }
        }
        let record = guard.file.tokens[index].clone();
        let name = Self::client_locked(&guard.file, &record.client_id)
            .map(|client| client.name.clone())
            .unwrap_or_else(|| "An MCP client".into());
        guard.file.tokens[index].last_used_at = Some(now);
        // Last-used is a convenience, so it is written lazily -- but always
        // from the state under this lock, never from a snapshot taken earlier.
        if guard.last_use_write.elapsed() >= LAST_USE_INTERVAL {
            guard.last_use_write = Instant::now();
            if let Err(error) = self.persist_locked(&guard.file) {
                tracing::warn!(%error, "could not record an outside agent's last use");
            }
        }
        Some(Grant {
            client_id: record.client_id,
            client_name: name,
            level: record.level,
            resource: record.resource,
        })
    }

    /// Revokes a token and every token in its family.
    ///
    /// Returns only after the revocation is on disk.
    pub fn revoke_token(&self, token: &str) -> Result<(), OAuthError> {
        let now = Utc::now();
        let hash = digest(token);
        let mut guard = self.lock();
        let Some(family) = guard
            .file
            .tokens
            .iter()
            .find(|entry| entry.hash == hash)
            .map(|entry| entry.family.clone())
        else {
            // RFC 7009: an unknown token is not an error.
            return Ok(());
        };
        revoke_family(&mut guard.file, &family, now);
        self.persist_locked(&guard.file)
    }

    /// Revokes every token for a client, and its codes and approvals.
    ///
    /// Returns only after the revocation is on disk, so "Revoke" in Settings
    /// never reports success for a grant a restart would resurrect.
    pub fn revoke_client(&self, client_id: &str) -> Result<(), OAuthError> {
        let now = Utc::now();
        let mut guard = self.lock();
        let Some(client) = Self::client_locked(&guard.file, client_id).cloned() else {
            return Err(OAuthError::NotFound(
                "no outside agent has that client id".into(),
            ));
        };
        for entry in guard
            .file
            .tokens
            .iter_mut()
            .filter(|entry| entry.client_id == client.client_id)
        {
            entry.revoked_at = Some(now);
        }
        guard
            .file
            .codes
            .retain(|entry| entry.client_id != client.client_id);
        guard
            .file
            .pending
            .retain(|entry| entry.client_id != client.client_id);
        // Revoking frees the registration slot too. The cap counts registered
        // clients, and "revoke one in h4rn" has to make room for the next
        // agent; keeping a dead record would make that advice false.
        guard
            .file
            .clients
            .retain(|entry| entry.client_id != client.client_id);
        self.persist_locked(&guard.file)
    }

    /// Every client with a live token, newest first.
    pub fn connected_agents(&self) -> Vec<OutsideAgentView> {
        let now = Utc::now();
        let guard = self.lock();
        let file = &guard.file;
        let mut agents: HashMap<String, OutsideAgentView> = HashMap::new();
        for token in file.tokens.iter().filter(|token| token.live(now)) {
            let Some(client) = Self::client_locked(file, &token.client_id) else {
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
            .file
            .tokens
            .iter()
            .filter(|token| token.live(now))
            .count()
    }

    /// Number of registered clients, for diagnostics and tests.
    pub fn client_count(&self) -> usize {
        self.lock().file.clients.len()
    }
}

/// The stored answer to one pending approval.
#[derive(Clone, Debug)]
pub struct ApprovalOutcome {
    /// Absolute redirect URI with the code or the error.
    pub location: String,
    /// When the person answered.
    pub decided_at: DateTime<Utc>,
}

fn level_rank(level: OutsideAgentLevel) -> u8 {
    match level {
        OutsideAgentLevel::ReadOnly => 0,
        OutsideAgentLevel::Supervised => 1,
        OutsideAgentLevel::FullAccess => 2,
    }
}

/// Revokes every token issued from one code or refresh chain.
fn revoke_family(file: &mut StoreFile, family: &str, now: DateTime<Utc>) {
    for entry in file
        .tokens
        .iter_mut()
        .filter(|entry| entry.family == family)
    {
        if entry.revoked_at.is_none() {
            entry.revoked_at = Some(now);
        }
    }
}

/// Drops registrations that never connected and are past their window.
fn prune_clients(file: &mut StoreFile, now: DateTime<Utc>) {
    let cutoff = now - UNUSED_CLIENT_TTL;
    let clients = std::mem::take(&mut file.clients);
    for client in clients {
        if client.created_at > cutoff || client_is_used(file, &client.client_id, now) {
            file.clients.push(client);
        }
    }
}

/// Whether a registration has anything live that still names it.
fn client_is_used(file: &StoreFile, client_id: &str, now: DateTime<Utc>) -> bool {
    file.tokens
        .iter()
        .any(|token| token.client_id == client_id && token.live(now))
        || file
            .codes
            .iter()
            .any(|code| code.client_id == client_id && code.expires_at > now)
        || file
            .pending
            .iter()
            .any(|pending| pending.client_id == client_id && pending.expires_at > now)
}

fn issue_pair(
    file: &mut StoreFile,
    client_id: &str,
    level: OutsideAgentLevel,
    now: DateTime<Utc>,
    family: Option<String>,
    resource: Option<String>,
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
        resource: resource.clone(),
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
        resource: resource.clone(),
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
        resource,
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

/// Where an unreadable store is moved so the daemon can start empty.
fn quarantine_path(path: &Path) -> PathBuf {
    let stamp = Utc::now().format("%Y%m%dT%H%M%S");
    path.with_extension(format!("corrupt-{stamp}.json"))
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

    fn approved_code(store: &OAuthStore, client_id: &str, level: OutsideAgentLevel) -> String {
        let (_, challenge) = challenge();
        let pending = store
            .begin_authorization(
                client_id,
                "http://localhost:1455/cb",
                None,
                &challenge,
                None,
            )
            .expect("begin");
        let redirect = store.approve(&pending.id, level).expect("approve");
        Url::parse(&redirect.location)
            .expect("url")
            .query_pairs()
            .find(|(key, _)| key == "code")
            .map(|(_, value)| value.to_string())
            .expect("code")
    }

    fn pair(store: &OAuthStore, client_id: &str, level: OutsideAgentLevel) -> TokenPair {
        let (verifier, _) = challenge();
        let code = approved_code(store, client_id, level);
        store
            .exchange_code(&code, &verifier, client_id, None, None)
            .expect("exchange")
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
                None,
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
            .exchange_code(&code, &verifier, &client.client_id, None, None)
            .expect("exchange");
        assert_eq!(pair.level, OutsideAgentLevel::Supervised);
        let grant = store
            .verify_access(&pair.access_token, None)
            .expect("grant");
        assert_eq!(grant.level, OutsideAgentLevel::Supervised);
        assert_eq!(grant.client_name, "Codex");
    }

    #[test]
    fn a_code_cannot_be_used_twice_and_a_replay_revokes_its_tokens() {
        let (_dir, store) = store();
        let client = store
            .register_client(None, &["http://localhost:1455/cb".into()])
            .expect("register");
        let (verifier, _) = challenge();
        let code = approved_code(&store, &client.client_id, OutsideAgentLevel::ReadOnly);
        let pair = store
            .exchange_code(&code, &verifier, &client.client_id, None, None)
            .expect("first use");
        assert!(store.verify_access(&pair.access_token, None).is_some());
        assert!(
            store
                .exchange_code(&code, &verifier, &client.client_id, None, None)
                .is_err()
        );
        // The replay is treated as a stolen code: the pair it issued is dead.
        assert!(store.verify_access(&pair.access_token, None).is_none());
        assert!(
            store
                .refresh(&pair.refresh_token, &client.client_id, None)
                .is_err()
        );
    }

    #[test]
    fn a_replayed_refresh_token_revokes_the_whole_family() {
        let (_dir, store) = store();
        let client = store
            .register_client(None, &["http://localhost:1455/cb".into()])
            .expect("register");
        let first = pair(&store, &client.client_id, OutsideAgentLevel::ReadOnly);
        let rotated = store
            .refresh(&first.refresh_token, &client.client_id, None)
            .expect("rotate");
        assert!(store.verify_access(&rotated.access_token, None).is_some());
        // The old refresh token comes back: someone else has it too.
        assert!(
            store
                .refresh(&first.refresh_token, &client.client_id, None)
                .is_err()
        );
        assert!(store.verify_access(&rotated.access_token, None).is_none());
        assert!(
            store
                .refresh(&rotated.refresh_token, &client.client_id, None)
                .is_err()
        );
    }

    #[test]
    fn a_token_bound_to_another_resource_is_refused() {
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
                Some("http://127.0.0.1:47800/mcp"),
            )
            .expect("begin");
        assert_eq!(
            pending.resource.as_deref(),
            Some("http://127.0.0.1:47800/mcp")
        );
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
            .exchange_code(&code, &verifier, &client.client_id, None, None)
            .expect("exchange");
        assert!(
            store
                .verify_access(&pair.access_token, Some("http://127.0.0.1:47800/mcp"))
                .is_some()
        );
        assert!(
            store
                .verify_access(&pair.access_token, Some("https://elsewhere.example/mcp"))
                .is_none()
        );
    }

    #[test]
    fn a_wrong_verifier_fails_and_leaves_the_code_unused() {
        let (_dir, store) = store();
        let client = store
            .register_client(None, &["http://localhost:1455/cb".into()])
            .expect("register");
        let (verifier, _) = challenge();
        let code = approved_code(&store, &client.client_id, OutsideAgentLevel::ReadOnly);
        let wrong = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        assert!(
            store
                .exchange_code(&code, wrong, &client.client_id, None, None)
                .is_err()
        );
        store
            .exchange_code(&code, &verifier, &client.client_id, None, None)
            .expect("right verifier still works");
    }

    #[test]
    fn a_token_exchange_needs_no_redirect_uri_but_refuses_a_wrong_one() {
        let (_dir, store) = store();
        let client = store
            .register_client(None, &["http://localhost:1455/cb".into()])
            .expect("register");
        let (verifier, _) = challenge();
        let code = approved_code(&store, &client.client_id, OutsideAgentLevel::ReadOnly);
        assert!(
            store
                .exchange_code(
                    &code,
                    &verifier,
                    &client.client_id,
                    Some("http://localhost:1/x"),
                    None,
                )
                .is_err()
        );
        store
            .exchange_code(
                &code,
                &verifier,
                &client.client_id,
                Some("http://localhost:1455/cb"),
                None,
            )
            .expect("the exact redirect still works");
    }

    #[test]
    fn revoking_a_client_kills_its_access_and_refresh_tokens() {
        let (_dir, store) = store();
        let client = store
            .register_client(None, &["http://localhost:1455/cb".into()])
            .expect("register");
        let pair = pair(&store, &client.client_id, OutsideAgentLevel::FullAccess);
        assert_eq!(store.connected_agents().len(), 1);
        store.revoke_client(&client.client_id).expect("revoke");
        assert!(store.verify_access(&pair.access_token, None).is_none());
        assert!(
            store
                .refresh(&pair.refresh_token, &client.client_id, None)
                .is_err()
        );
        assert!(store.connected_agents().is_empty());
    }

    #[test]
    fn a_revocation_survives_a_restart_even_when_use_raced_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = OAuthStore::open(dir.path()).expect("store");
        let client = store
            .register_client(None, &["http://localhost:1455/cb".into()])
            .expect("register");
        let pair = pair(&store, &client.client_id, OutsideAgentLevel::ReadOnly);
        // Use the token so the lazy last-used write is armed, then revoke.
        store
            .verify_access(&pair.access_token, None)
            .expect("grant");
        store.revoke_client(&client.client_id).expect("revoke");
        // A burst of use attempts on the now-dead token must not resurrect it.
        for _ in 0..5 {
            assert!(store.verify_access(&pair.access_token, None).is_none());
        }
        drop(store);
        let reopened = OAuthStore::open(dir.path()).expect("reopen");
        assert!(reopened.verify_access(&pair.access_token, None).is_none());
        assert!(reopened.connected_agents().is_empty());
    }

    #[test]
    fn an_expired_access_token_is_refused() {
        let (_dir, store) = store();
        let client = store
            .register_client(None, &["http://localhost:1455/cb".into()])
            .expect("register");
        let pair = pair(&store, &client.client_id, OutsideAgentLevel::ReadOnly);
        {
            let mut state = store.lock();
            for token in &mut state.file.tokens {
                if token.kind == TokenKind::Access {
                    token.expires_at = Utc::now() - TimeDelta::seconds(1);
                }
            }
        }
        assert!(store.verify_access(&pair.access_token, None).is_none());
        store
            .refresh(&pair.refresh_token, &client.client_id, None)
            .expect("refresh still works");
    }

    #[test]
    fn a_pending_sign_in_shows_the_callback_origin_and_resolves_once() {
        let (_dir, store) = store();
        let client = store
            .register_client(Some("Claude Code"), &["http://localhost:1455/cb".into()])
            .expect("register");
        let (_, challenge) = challenge();
        let pending = store
            .begin_authorization(
                &client.client_id,
                "http://localhost:1455/cb",
                Some("s"),
                &challenge,
                None,
            )
            .expect("begin");
        let sign_ins = store.pending_sign_ins();
        assert_eq!(sign_ins.len(), 1);
        assert_eq!(sign_ins[0].client_name, "Claude Code");
        assert_eq!(sign_ins[0].redirect_origin, "http://localhost:1455");
        assert!(store.approval_outcome(&pending.id).is_none());
        store
            .approve(&pending.id, OutsideAgentLevel::ReadOnly)
            .expect("approve");
        let outcome = store.approval_outcome(&pending.id).expect("outcome");
        assert!(outcome.location.contains("code="));
        assert!(store.pending_sign_ins().is_empty());
        assert!(store.deny(&pending.id).is_err());
    }

    #[test]
    fn registrations_are_capped() {
        let (_dir, store) = store();
        for index in 0..MAX_CLIENTS {
            store
                .register_client(
                    Some(&format!("client {index}")),
                    &["http://localhost:1455/cb".into()],
                )
                .expect("register");
        }
        assert_eq!(store.client_count(), MAX_CLIENTS);
        assert!(
            store
                .register_client(None, &["http://localhost:1455/cb".into()])
                .is_err()
        );
        assert_eq!(store.client_count(), MAX_CLIENTS);
    }

    /// A writer the log test can read back.
    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_corrupt_store_logs_no_credential_from_inside_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let planted = format!("{}{}", "h4rn_at_", "QUARANTINEDTESTVALUE");
        let content = format!("{{ \"tokens\": [ \"{planted}\"");
        std::fs::write(dir.path().join(FILE), content).expect("write");
        let capture = Captured::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let _ = OAuthStore::open(dir.path()).expect("open");
        });
        let logs = String::from_utf8(capture.0.lock().expect("logs").clone()).expect("utf8");
        assert!(!logs.is_empty(), "the quarantine must be reported");
        assert!(
            !logs.contains(&planted),
            "the warning repeated a credential: {logs}"
        );
    }

    #[test]
    fn an_unreadable_store_is_quarantined_instead_of_stopping_the_daemon() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(FILE), b"{ not json").expect("write");
        let store = OAuthStore::open(dir.path()).expect("the daemon starts");
        assert!(store.connected_agents().is_empty());
        assert_eq!(store.client_count(), 0);
        let quarantined: Vec<_> = std::fs::read_dir(dir.path())
            .expect("dir")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.contains("corrupt"))
            })
            .collect();
        assert_eq!(quarantined.len(), 1, "the bad file must be kept aside");
    }

    /// The code out of an approval redirect.
    fn code_of(location: &str) -> String {
        Url::parse(location)
            .expect("url")
            .query_pairs()
            .find(|(key, _)| key == "code")
            .map(|(_, value)| value.to_string())
            .expect("code")
    }

    /// A token request's `resource` must be the one its code was approved for.
    ///
    /// RFC 8707 binding is worthless if the token endpoint lets the caller name
    /// a different resource, or widen a grant that had none.
    #[test]
    fn a_token_request_must_name_the_code_s_resource() {
        let (_dir, store) = store();
        let client = store
            .register_client(None, &["http://localhost:1455/cb".into()])
            .expect("register");
        let (verifier, challenge) = challenge();
        let mcp = "http://127.0.0.1:47800/mcp";

        // A code approved for the daemon's MCP endpoint.
        let pending = store
            .begin_authorization(
                &client.client_id,
                "http://localhost:1455/cb",
                None,
                &challenge,
                Some(mcp),
            )
            .expect("begin");
        let redirect = store
            .approve(&pending.id, OutsideAgentLevel::ReadOnly)
            .expect("approve");
        let code = code_of(&redirect.location);

        let error = store
            .exchange_code(
                &code,
                &verifier,
                &client.client_id,
                None,
                Some("https://elsewhere.example/mcp"),
            )
            .expect_err("a different resource must be refused");
        assert_eq!(error.code(), "invalid_target");

        // The refused request did not spend the code.
        let pair = store
            .exchange_code(&code, &verifier, &client.client_id, None, Some(mcp))
            .expect("the matching resource still works");
        assert_eq!(pair.resource.as_deref(), Some(mcp));

        // A code approved without a resource cannot be widened by naming one.
        let unbound = approved_code(&store, &client.client_id, OutsideAgentLevel::ReadOnly);
        let error = store
            .exchange_code(&unbound, &verifier, &client.client_id, None, Some(mcp))
            .expect_err("an unbound grant must not adopt a resource");
        assert_eq!(error.code(), "invalid_target");
        assert!(
            store
                .exchange_code(&unbound, &verifier, &client.client_id, None, None)
                .is_ok(),
            "the refused request spent the unbound code"
        );
    }

    /// Revoking an agent frees the registration slot it occupied.
    #[test]
    fn revoking_a_client_frees_its_registration_slot() {
        let (_dir, store) = store();
        let mut ids = Vec::new();
        for index in 0..MAX_CLIENTS {
            let client = store
                .register_client(
                    Some(&format!("client {index}")),
                    &["http://localhost:1455/cb".into()],
                )
                .expect("register");
            ids.push(client.client_id);
        }
        assert!(
            store
                .register_client(None, &["http://localhost:1455/cb".into()])
                .is_err(),
            "the cap must be reached first"
        );
        store.revoke_client(&ids[0]).expect("revoke one");
        assert_eq!(store.client_count(), MAX_CLIENTS - 1);
        store
            .register_client(None, &["http://localhost:1455/cb".into()])
            .expect("the freed slot accepts a new registration");
        assert_eq!(store.client_count(), MAX_CLIENTS);
    }

    /// The approval's redirect (and so its live code) never reaches the disk.
    #[test]
    fn an_approval_never_writes_the_code_to_disk() {
        let (dir, store) = store();
        let client = store
            .register_client(None, &["http://localhost:1455/cb".into()])
            .expect("register");
        let (_, challenge) = challenge();
        let pending = store
            .begin_authorization(
                &client.client_id,
                "http://localhost:1455/cb",
                None,
                &challenge,
                None,
            )
            .expect("begin");
        let redirect = store
            .approve(&pending.id, OutsideAgentLevel::ReadOnly)
            .expect("approve");
        assert!(redirect.location.contains("h4rn_code_"));
        // The waiting page can still follow the answer while the daemon runs.
        let outcome = store.approval_outcome(&pending.id).expect("outcome");
        assert_eq!(outcome.location, redirect.location);

        let raw = std::fs::read_to_string(dir.path().join(FILE)).expect("file");
        assert!(
            !raw.contains("h4rn_code_"),
            "the store wrote a live authorization code"
        );

        // A restart forgets the answer: the browser is asked to sign in again
        // rather than being handed a code the restarted daemon cannot vouch for.
        drop(store);
        let reopened = OAuthStore::open(dir.path()).expect("reopen");
        assert!(reopened.approval_outcome(&pending.id).is_none());
        assert!(
            reopened.pending(&pending.id).is_none(),
            "an answered request must not wait for the person again"
        );
    }

    /// Waiting sign-ins are bounded per client and in total.
    #[test]
    fn waiting_sign_ins_are_capped_per_client_and_in_total() {
        let (_dir, store) = store();
        let redirect = "http://localhost:1455/cb";
        // Enough clients to fill the total at the per-client cap each.
        let clients: Vec<_> = (0..6)
            .map(|index| {
                store
                    .register_client(Some(&format!("client {index}")), &[redirect.into()])
                    .expect("register")
            })
            .collect();
        let (_, challenge) = challenge();
        let begin = |client_id: &str| {
            store.begin_authorization(client_id, redirect, None, &challenge, None)
        };

        for _ in 0..MAX_PENDING_PER_CLIENT {
            begin(&clients[0].client_id).expect("begin");
        }
        let error =
            begin(&clients[0].client_id).expect_err("the client's own requests must be capped");
        assert!(matches!(error, OAuthError::InvalidRequest(_)), "{error:?}");

        // Other clients are not blocked by this one's requests, until the
        // total is reached.
        let mut waiting = MAX_PENDING_PER_CLIENT;
        for client in clients.iter().skip(1) {
            let take = (MAX_PENDING - waiting).min(MAX_PENDING_PER_CLIENT);
            for _ in 0..take {
                begin(&client.client_id).expect("begin");
                waiting += 1;
            }
            if waiting == MAX_PENDING {
                break;
            }
        }
        assert_eq!(store.pending_sign_ins().len(), MAX_PENDING);
        let last = clients.last().expect("a client");
        let error = begin(&last.client_id).expect_err("the total must be capped");
        assert!(matches!(error, OAuthError::InvalidRequest(_)), "{error:?}");
        assert_eq!(store.pending_sign_ins().len(), MAX_PENDING);
    }

    /// A revocation wins a race with a last-used write.
    ///
    /// The store used to take a snapshot of its state and write it after
    /// releasing the lock; a use landing in that window could put a live token
    /// back on disk after the person revoked it. Use and revocation now share
    /// one lock, so the token stays revoked and no older last-used value
    /// returns with it.
    #[test]
    fn a_revocation_wins_a_race_with_a_last_used_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = std::sync::Arc::new(OAuthStore::open(dir.path()).expect("store"));
        let client = store
            .register_client(None, &["http://localhost:1455/cb".into()])
            .expect("register");
        let pair = pair(&store, &client.client_id, OutsideAgentLevel::ReadOnly);
        store
            .verify_access(&pair.access_token, None)
            .expect("the first use arms the lazy write");

        let token = pair.access_token.clone();
        let reader = std::sync::Arc::clone(&store);
        let reader_token = token.clone();
        let uses = std::thread::spawn(move || {
            for _ in 0..120 {
                // Keep the lazy last-used write due on every pass, so a writer
                // that snapshots and persists outside the lock has the widest
                // window to put its stale copy back.
                {
                    let mut state = reader.lock();
                    state.last_use_write =
                        Instant::now() - LAST_USE_INTERVAL - Duration::from_secs(1);
                }
                let _ = reader.verify_access(&reader_token, None);
            }
        });
        std::thread::sleep(Duration::from_millis(2));
        store.revoke_client(&client.client_id).expect("revoke");
        uses.join().expect("the use loop");

        // In memory the token is dead and its last-used value is final.
        assert!(store.verify_access(&token, None).is_none());
        let hash = digest(&token);
        let recorded = {
            let state = store.lock();
            let row = state
                .file
                .tokens
                .iter()
                .find(|entry| entry.hash == hash)
                .expect("the token row");
            assert!(
                row.revoked_at.is_some(),
                "the revocation was lost in memory"
            );
            row.last_used_at
        };
        drop(store);

        let reopened = OAuthStore::open(dir.path()).expect("reopen");
        assert!(
            reopened.verify_access(&token, None).is_none(),
            "a use that raced the revocation resurrected the token"
        );
        assert_eq!(reopened.client_count(), 0, "the revoked client came back");
        let state = reopened.lock();
        let row = state
            .file
            .tokens
            .iter()
            .find(|entry| entry.hash == hash)
            .expect("the token row survived");
        assert!(
            row.revoked_at.is_some(),
            "the reopened store saved a live copy of a revoked token"
        );
        assert!(
            row.last_used_at >= recorded,
            "a stale last-used value came back: {:?} < {recorded:?}",
            row.last_used_at
        );
    }
}
