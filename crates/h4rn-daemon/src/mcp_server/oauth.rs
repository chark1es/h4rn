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
#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
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
/// The longest a Full-access grant may live, from approval to the last token.
///
/// Full access is the one level that can run unattended, so it needs a ceiling
/// of its own: the person re-approves it daily instead of a forgotten grant
/// renewing for the refresh token's thirty days. Supervised and read-only
/// grants keep the ordinary refresh lifetime.
pub const FULL_ACCESS_TTL: TimeDelta = TimeDelta::hours(24);

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

/// One sign-in's grant, shared by every token in its family.
///
/// The level lives here, not on each token: admission reads it again on every
/// request, so a level change applies to tokens already issued instead of
/// waiting for the next refresh. `created_at` is the ceiling's anchor — a
/// Full-access family may not outlive [`FULL_ACCESS_TTL`] from it, however
/// often its refresh token rotates.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct FamilyRecord {
    family: String,
    client_id: String,
    level: OutsideAgentLevel,
    created_at: DateTime<Utc>,
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
    /// Whether Settings → Labs → Outside agents (OAuth) is on.
    ///
    /// `None` means the file predates the Labs switch. Such a file is treated
    /// as off and its grants are revoked the first time it is opened, so an
    /// upgrade can never resurrect a grant when the switch is later turned on.
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    clients: Vec<ClientRecord>,
    /// One row per sign-in's grant. A file written before this existed has
    /// none; its tokens keep the level they were issued with.
    #[serde(default)]
    families: Vec<FamilyRecord>,
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
    /// The token family this access token belongs to.
    ///
    /// The family, not the client, is the grant's identity: a client can hold
    /// several sign-ins, and work asked for under one must not survive that
    /// one's revocation just because another is still live.
    pub family: String,
    /// RFC 8707 resource the token is bound to, if any.
    pub resource: Option<String>,
}

/// One prompt write's registration under a token family.
///
/// A revoke marks the family revoked and then waits until every ticket for it
/// is dropped. Holding it across the adapter write is what makes "Revoke
/// returned" mean nothing under that grant can still reach the agent.
pub struct OAuthGrantTicket {
    store: Arc<OAuthStore>,
    family: String,
}

impl Drop for OAuthGrantTicket {
    fn drop(&mut self) {
        self.store.end_delivery(&self.family);
    }
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

/// A test-only stop between the state lock and the durable write.
///
/// The revocation-race test parks a last-used write here. With the real shape
/// (write under the one state lock) the revoker cannot finish until the parked
/// write is released; with a snapshot taken and written outside the lock the
/// revoker finishes first and the stale write lands after it, which is exactly
/// the bug.
#[cfg(test)]
#[derive(Default)]
struct PersistGate {
    state: Mutex<(bool, bool)>,
    changed: std::sync::Condvar,
}

#[cfg(test)]
impl PersistGate {
    fn enter(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.0 = true;
        self.changed.notify_all();
        while !state.1 {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    fn wait_until_entered(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while !state.0 {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    fn release(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.1 = true;
        self.changed.notify_all();
    }
}

/// Test-only controls: injected write failures and the persist gate above.
#[cfg(test)]
#[derive(Default)]
struct TestHooks {
    fail_persist: AtomicUsize,
    last_use_gate: Mutex<Option<std::sync::Arc<PersistGate>>>,
    in_last_use: AtomicBool,
    /// Directory syncs a durable write performed, so a test can prove the
    /// rename is followed by one.
    dir_syncs: AtomicUsize,
}

/// Durable OAuth state.
pub struct OAuthStore {
    path: PathBuf,
    /// The whole state and the write of it share one lock. A snapshot is never
    /// taken, released and written later: that is what used to let a revocation
    /// lose a race with a last-used update.
    state: Mutex<StoreState>,
    #[cfg(test)]
    hooks: TestHooks,
    /// Where an answered approval sends the browser, by request id.
    ///
    /// Memory only: the location holds the one-time code. Lock order is always
    /// `state` then `resolutions`, never the reverse.
    resolutions: Mutex<HashMap<String, PendingResolution>>,
    /// Prompt writes in flight under a token family.
    ///
    /// A revoke marks the family revoked, then waits here for the writes that
    /// already passed their check. After it returns no prompt under that grant
    /// can still reach an agent.
    in_flight: Mutex<HashMap<String, usize>>,
    /// Signalled whenever a delivery under a family ends.
    drained: Condvar,
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
        let mut existed = true;
        let file = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<StoreFile>(&bytes) {
                Ok(file) => file,
                Err(error) => {
                    existed = false;
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
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                existed = false;
                StoreFile::default()
            }
            Err(error) => return Err(error),
        };
        let store = Self {
            path,
            state: Mutex::new(StoreState {
                file,
                last_use_write: Instant::now() - LAST_USE_INTERVAL,
            }),
            #[cfg(test)]
            hooks: TestHooks::default(),
            resolutions: Mutex::new(HashMap::new()),
            in_flight: Mutex::new(HashMap::new()),
            drained: Condvar::new(),
        };
        // A file written before the Labs switch existed has no answer for it.
        // Treat that as off and end every grant in memory, then try to commit
        // the answer. A failed write still leaves the grants revoked for this
        // run, so turning the switch on cannot revive them either.
        if existed && store.lock().file.enabled.is_none() {
            store.revoke_everything_for_legacy_file();
        }
        Ok(store)
    }

    /// Ends every grant in a store written before the Labs switch existed.
    fn revoke_everything_for_legacy_file(&self) {
        let now = Utc::now();
        let mut guard = self.lock();
        revoke_all(&mut guard.file, now);
        guard.file.enabled = Some(false);
        if let Err(error) = self.persist_locked(&guard.file) {
            tracing::warn!(
                %error,
                "could not save the outside-agent switch; grants stay revoked for this run"
            );
        }
    }

    /// Whether Settings → Labs → Outside agents (OAuth) is on.
    ///
    /// A file written before the switch existed counts as off.
    pub fn enabled(&self) -> bool {
        self.lock().file.enabled.unwrap_or(false)
    }

    /// Fails the next `count` durable writes, for tests that prove a failed
    /// write is reported and leaves the state retryable.
    #[cfg(test)]
    pub(crate) fn fail_next_persists_for_tests(&self, count: usize) {
        self.hooks.fail_persist.store(count, Ordering::SeqCst);
    }

    /// Arms the gate a last-used write parks at, for the revocation race test.
    #[cfg(test)]
    fn arm_last_use_gate_for_tests(&self) -> std::sync::Arc<PersistGate> {
        let gate = std::sync::Arc::new(PersistGate::default());
        *self
            .hooks
            .last_use_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(std::sync::Arc::clone(&gate));
        gate
    }

    /// Disarms the gate so later passes do not park.
    #[cfg(test)]
    fn disarm_last_use_gate_for_tests(&self) {
        *self
            .hooks
            .last_use_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, StoreState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Writes the live state atomically. Called with the state lock held.
    fn persist_locked(&self, file: &StoreFile) -> Result<(), OAuthError> {
        #[cfg(test)]
        {
            let remaining = self.hooks.fail_persist.load(Ordering::SeqCst);
            if remaining > 0 {
                self.hooks
                    .fail_persist
                    .store(remaining - 1, Ordering::SeqCst);
                return Err(OAuthError::Storage("injected write failure".into()));
            }
            if self.hooks.in_last_use.load(Ordering::SeqCst) {
                let gate = self
                    .hooks
                    .last_use_gate
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                if let Some(gate) = gate {
                    gate.enter();
                }
            }
        }
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
        std::fs::rename(&temp, &self.path)
            .map_err(|error| OAuthError::Storage(error.to_string()))?;
        // The rename itself is only in the directory's cache until the
        // directory is synced. Without this a crash can leave the old file in
        // place even though the revocation reported success.
        if let Some(parent) = self.path.parent() {
            self.sync_directory(parent)?;
        }
        Ok(())
    }

    /// Flushes a directory entry so a completed rename survives a crash.
    ///
    /// The counter lives in this function, not beside its call, so a test that
    /// removes the sync cannot still see a sync it never performed.
    fn sync_directory(&self, path: &Path) -> Result<(), OAuthError> {
        #[cfg(unix)]
        {
            std::fs::File::open(path)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| OAuthError::Storage(error.to_string()))?;
        }
        #[cfg(not(unix))]
        {
            let _ = path;
        }
        #[cfg(test)]
        self.hooks.dir_syncs.fetch_add(1, Ordering::SeqCst);
        Ok(())
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
        if !guard.file.enabled.unwrap_or(false) {
            return Err(OAuthError::NotFound(
                "outside agents are off in Settings → Labs".into(),
            ));
        }
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
        // Drop anything that expired while the page was open, under the same
        // rule issuance uses: a spent code whose family is still live is the
        // only record a replay can be recognised by, so it stays.
        prune_spent_codes(&mut guard.file, now);
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
            // issued is in an attacker's hands too: revoke it. The revocation
            // is only real once it is on disk, so a failed write is an error
            // rather than a silent "revoked" the restart would undo.
            if let Some(family) = record.family.as_deref() {
                let mut next = guard.file.clone();
                revoke_family(&mut next, family, now);
                self.persist_locked(&next)?;
                guard.file = next;
            }
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
        // RFC 8707: the exchange must name exactly the resource the code was
        // approved for. A caller that omits it reaches the HTTP layer already
        // bound to this daemon's own audience, so `None` here only matches a
        // code issued while no listener was bound.
        if record.resource.as_deref() != resource {
            // The code is not spent: the client can retry the exchange with the
            // resource it was actually approved for.
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
            // it: kill the family rather than let either continue. As with a
            // replayed code, the revocation is only real once it is on disk.
            if record.kind == TokenKind::Refresh && record.client_id == client_id {
                let mut next = guard.file.clone();
                revoke_family(&mut next, &record.family, now);
                self.persist_locked(&next)?;
                guard.file = next;
                return Err(OAuthError::InvalidGrant(
                    "the refresh token was already used; its tokens were revoked".into(),
                ));
            }
            return Err(OAuthError::InvalidGrant(
                "the refresh token is expired, revoked or belongs to another client".into(),
            ));
        }
        // The same RFC 8707 binding the code was approved for: a refresh names
        // the audience it will use, and an omitted audience is already bound to
        // this daemon's own by the HTTP layer. A grant without a binding (made
        // before one was recorded) cannot be widened or renewed into one.
        if record.resource.as_deref() != resource {
            return Err(OAuthError::InvalidTarget(
                "this grant is bound to a different resource".into(),
            ));
        }
        // A Full-access grant has a lifetime ceiling: once it is reached the
        // family is revoked and the person approves a new sign-in. The check is
        // here as well as on expiry because a clock can sit between the two.
        if let Some(family) = guard
            .file
            .families
            .iter()
            .find(|family| family.family == record.family)
        {
            if family.level == OutsideAgentLevel::FullAccess
                && now >= family.created_at + FULL_ACCESS_TTL
            {
                let mut next = guard.file.clone();
                revoke_family(&mut next, &record.family, now);
                self.persist_locked(&next)?;
                guard.file = next;
                return Err(OAuthError::InvalidGrant(
                    "this full-access grant reached its 24-hour limit; sign in again".into(),
                ));
            }
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
        // The Labs switch is a hard gate: a token in the file is not a grant
        // while outside-agent access is off.
        if !guard.file.enabled.unwrap_or(false) {
            return None;
        }
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
            // A token with no audience is not a token for this endpoint. Every
            // grant the daemon issues is bound to its MCP audience, so `None`
            // here can only be a token that cannot prove what it is for.
            if record.resource.as_deref() != expected_resource {
                return None;
            }
        }
        let record = guard.file.tokens[index].clone();
        let name = Self::client_locked(&guard.file, &record.client_id)
            .map(|client| client.name.clone())
            .unwrap_or_else(|| "An MCP client".into());
        // The grant's level is read from the family on every admission, not
        // copied from the token that was issued under it. A level the person
        // changes then applies to the very next request.
        let level = guard
            .file
            .families
            .iter()
            .find(|family| family.family == record.family)
            .map(|family| family.level)
            .unwrap_or(record.level);
        guard.file.tokens[index].last_used_at = Some(now);
        // Last-used is a convenience, so it is written lazily -- but always
        // from the state under this lock, never from a snapshot taken earlier.
        if guard.last_use_write.elapsed() >= LAST_USE_INTERVAL {
            guard.last_use_write = Instant::now();
            // The write happens under this same lock, so a revocation cannot
            // slip between the update and the file: the revoker waits, and the
            // file never gets a snapshot older than the revocation.
            #[cfg(test)]
            self.hooks.in_last_use.store(true, Ordering::SeqCst);
            let written = self.persist_locked(&guard.file);
            #[cfg(test)]
            self.hooks.in_last_use.store(false, Ordering::SeqCst);
            if let Err(error) = written {
                tracing::warn!(%error, "could not record an outside agent's last use");
            }
        }
        Some(Grant {
            client_id: record.client_id,
            client_name: name,
            level,
            family: record.family,
            resource: record.resource,
        })
    }

    /// Revokes a token and every token in its family.
    ///
    /// Returns only after the revocation is on disk.
    pub fn revoke_token(&self, token: &str) -> Result<(), OAuthError> {
        self.revoke_token_families(token).map(|_| ())
    }

    /// [`Self::revoke_token`], returning the token family it ended.
    ///
    /// The caller takes the admission gate and waits for that family's in-flight
    /// prompt writes after this returns, so a client-facing revoke means the
    /// same thing as one from Settings. An unknown token ends no family and is
    /// still not an error (RFC 7009).
    pub fn revoke_token_families(&self, token: &str) -> Result<Vec<String>, OAuthError> {
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
            return Ok(Vec::new());
        };
        let mut next = guard.file.clone();
        revoke_family(&mut next, &family, now);
        self.persist_locked(&next)?;
        guard.file = next;
        Ok(vec![family])
    }

    /// Turns outside-agent access on or off.
    ///
    /// Turning the Labs switch off *ends* every grant: every token family is
    /// revoked, and waiting codes and sign-in requests are dropped. The switch
    /// and the revocations are one durable write under the state lock, so a
    /// crash can never leave the switch off with a grant the other half never
    /// saw -- and turning it back on does not revive anything.
    pub fn set_enabled(&self, enabled: bool) -> Result<bool, OAuthError> {
        self.set_enabled_families(enabled).map(|_| enabled)
    }

    /// [`Self::set_enabled`], returning the families the switch ended.
    ///
    /// The caller waits for those families' in-flight prompt writes, so a
    /// switch turned off cannot report success while a write is still landing.
    pub fn set_enabled_families(&self, enabled: bool) -> Result<Vec<String>, OAuthError> {
        let now = Utc::now();
        let mut guard = self.lock();
        let mut next = guard.file.clone();
        let mut families = Vec::new();
        if !enabled {
            families = next
                .tokens
                .iter()
                .filter(|token| token.revoked_at.is_none() && token.expires_at > now)
                .map(|token| token.family.clone())
                .collect();
            families.sort();
            families.dedup();
            revoke_all(&mut next, now);
        }
        next.enabled = Some(enabled);
        self.persist_locked(&next)?;
        guard.file = next;
        Ok(families)
    }

    /// Revokes every token for a client, and its codes and approvals.
    ///
    /// Returns only after the revocation is on disk, so "Revoke" in Settings
    /// never reports success for a grant a restart would resurrect.
    pub fn revoke_client(&self, client_id: &str) -> Result<(), OAuthError> {
        self.revoke_client_families(client_id).map(|_| ())
    }

    /// [`Self::revoke_client`], returning the token families it ended.
    ///
    /// The caller waits for those families' in-flight prompt writes before
    /// answering, so "Revoke" returning means none of that grant's work can
    /// still reach an agent.
    pub fn revoke_client_families(&self, client_id: &str) -> Result<Vec<String>, OAuthError> {
        let now = Utc::now();
        let mut guard = self.lock();
        let Some(client) = Self::client_locked(&guard.file, client_id).cloned() else {
            return Err(OAuthError::NotFound(
                "no outside agent has that client id".into(),
            ));
        };
        // Apply the revocation to a candidate and commit it to memory only
        // after the file holds it. A failed write must leave the grant exactly
        // as it was, so the retry still finds it and revokes it for real; the
        // old order removed it from memory first, and the retry then answered
        // "no such client" while the disk still had the live grant.
        let mut next = guard.file.clone();
        let mut families: Vec<String> = next
            .tokens
            .iter()
            .filter(|entry| entry.client_id == client.client_id)
            .map(|entry| entry.family.clone())
            .collect();
        families.sort();
        families.dedup();
        for entry in next
            .tokens
            .iter_mut()
            .filter(|entry| entry.client_id == client.client_id)
        {
            entry.revoked_at = Some(now);
        }
        next.codes
            .retain(|entry| entry.client_id != client.client_id);
        next.pending
            .retain(|entry| entry.client_id != client.client_id);
        next.families
            .retain(|entry| entry.client_id != client.client_id);
        // Revoking frees the registration slot too. The cap counts registered
        // clients, and "revoke one in h4rn" has to make room for the next
        // agent; keeping a dead record would make that advice false.
        next.clients
            .retain(|entry| entry.client_id != client.client_id);
        self.persist_locked(&next)?;
        guard.file = next;
        Ok(families)
    }

    /// Registers a prompt write under `family`, or refuses it when the family
    /// was revoked.
    ///
    /// The check and the registration happen while the state lock is held, so a
    /// revoke either sees this write and waits for it, or marks the family
    /// first and this call refuses. Dropping the ticket is what a revoke waits
    /// for; a running turn that already wrote its prompt holds none and is
    /// allowed to finish, as documented.
    pub fn begin_delivery(self: &Arc<Self>, family: &str) -> Option<OAuthGrantTicket> {
        let now = Utc::now();
        let guard = self.lock();
        if !guard.file.enabled.unwrap_or(false) {
            return None;
        }
        let live = guard
            .file
            .tokens
            .iter()
            .any(|entry| entry.family == family && entry.live(now));
        if !live {
            return None;
        }
        let mut ids = self
            .in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *ids.entry(family.to_owned()).or_default() += 1;
        drop(ids);
        drop(guard);
        Some(OAuthGrantTicket {
            store: Arc::clone(self),
            family: family.to_owned(),
        })
    }

    /// Ends one prompt write's registration.
    fn end_delivery(&self, family: &str) {
        let mut ids = self
            .in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(count) = ids.get_mut(family) else {
            return;
        };
        *count = count.saturating_sub(1);
        if *count == 0 {
            ids.remove(family);
        }
        self.drained.notify_all();
    }

    /// Waits until no prompt write is in flight for any of `families`.
    ///
    /// Blocking: the caller runs it where blocking a thread is acceptable (the
    /// revoke path does so off the async runtime).
    pub fn wait_for_drain(&self, families: &[String]) {
        let mut ids = self
            .in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while families
            .iter()
            .any(|family| ids.get(family).copied().unwrap_or(0) > 0)
        {
            ids = self
                .drained
                .wait(ids)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    /// Makes a family's level something else, so a test can prove admission
    /// reads the family rather than the token issued under it.
    #[doc(hidden)]
    pub fn set_family_level_for_test(
        &self,
        family: &str,
        level: OutsideAgentLevel,
    ) -> Result<(), OAuthError> {
        let mut guard = self.lock();
        let mut next = guard.file.clone();
        let Some(record) = next
            .families
            .iter_mut()
            .find(|record| record.family == family)
        else {
            return Err(OAuthError::NotFound("no such family".into()));
        };
        record.level = level;
        self.persist_locked(&next)?;
        guard.file = next;
        Ok(())
    }

    /// Moves a Full-access family's approval far enough back that its ceiling
    /// has passed, so a test need not wait a day.
    #[doc(hidden)]
    pub fn age_full_access_family_for_test(&self, family: &str) -> Result<(), OAuthError> {
        let mut guard = self.lock();
        let mut next = guard.file.clone();
        let Some(record) = next
            .families
            .iter_mut()
            .find(|record| record.family == family)
        else {
            return Err(OAuthError::NotFound("no such family".into()));
        };
        record.created_at = Utc::now() - FULL_ACCESS_TTL - TimeDelta::seconds(1);
        self.persist_locked(&next)?;
        guard.file = next;
        Ok(())
    }

    /// Ages every rotated refresh record past the ordinary retention window,
    /// while its family stays live through a newer token.
    ///
    /// Test-only: proves a replay record is kept for the family's lifetime
    /// rather than pruned by the record's own age.
    #[doc(hidden)]
    pub fn age_rotated_refresh_records_for_test(&self) -> Result<(), OAuthError> {
        let now = Utc::now();
        let mut guard = self.lock();
        for entry in guard.file.tokens.iter_mut().filter(|entry| {
            entry.kind == TokenKind::Refresh && entry.revoked_at.is_some() && entry.expires_at > now
        }) {
            entry.expires_at = now - TimeDelta::days(61);
        }
        self.persist_locked(&guard.file)
    }

    /// Whether `family` names a grant that is still live.
    ///
    /// A durable record that may later start work for a grant (a PR watch, an
    /// armed resume, a parked prompt, a delegated child) asks this at the moment
    /// that work would start, so a revocation made in between is honoured.
    ///
    /// Any live, unrevoked token in the family keeps it live: an access token
    /// expiring after an hour must not end a watch while the same family's
    /// refresh token can still sign in. A family with no live token (every
    /// token revoked or expired) is gone.
    pub fn grant_is_live(&self, family: &str) -> bool {
        let now = Utc::now();
        let guard = self.lock();
        guard
            .file
            .tokens
            .iter()
            .any(|entry| entry.family == family && entry.live(now))
    }

    /// Expires every access token, so a test can prove a family outlives one.
    ///
    /// Test-only: the refresh tokens stay live, exactly as after the access
    /// token's hour has passed while the thirty-day refresh token has not.
    #[doc(hidden)]
    pub fn expire_access_tokens_for_test(&self) -> Result<(), OAuthError> {
        let now = Utc::now();
        let mut guard = self.lock();
        for entry in guard
            .file
            .tokens
            .iter_mut()
            .filter(|entry| entry.kind == TokenKind::Access)
        {
            entry.expires_at = now - TimeDelta::seconds(1);
        }
        self.persist_locked(&guard.file)
    }

    /// Expires every authorization code, for the replay-after-expiry test.
    #[doc(hidden)]
    pub fn expire_codes_for_test(&self) -> Result<(), OAuthError> {
        let now = Utc::now();
        let mut guard = self.lock();
        for entry in &mut guard.file.codes {
            entry.expires_at = now - TimeDelta::seconds(1);
        }
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
            let level = file
                .families
                .iter()
                .find(|family| family.family == token.family)
                .map(|family| family.level)
                .unwrap_or(token.level);
            agents
                .entry(token.client_id.clone())
                .and_modify(|view| {
                    if level_rank(level) > level_rank(view.level) {
                        view.level = level;
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
                    level,
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

/// Revokes every grant in the store and drops codes and waiting sign-ins.
///
/// Used when the Labs switch is turned off and when a file written before the
/// switch existed is opened. Clients stay registered: the person can turn the
/// switch back on and the next agent signs in again without re-registering,
/// but no token from before the switch works.
fn revoke_all(file: &mut StoreFile, now: DateTime<Utc>) {
    for entry in &mut file.tokens {
        if entry.revoked_at.is_none() {
            entry.revoked_at = Some(now);
        }
    }
    file.codes.clear();
    file.pending.clear();
}

/// Drops spent codes once they can no longer identify a live family.
///
/// One rule for every path that prunes the code list (`approve` and issuance):
/// an unexpired code stays, and an expired one stays while its family still has
/// a token that has not expired, so a replay can always be traced to the family
/// it must revoke.
fn prune_spent_codes(file: &mut StoreFile, now: DateTime<Utc>) {
    let tokens = &file.tokens;
    file.codes.retain(|code| {
        if code.expires_at > now {
            return true;
        }
        let Some(family) = code.family.as_deref() else {
            return false;
        };
        tokens
            .iter()
            .any(|token| token.family == family && token.expires_at > now)
    });
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
    // The level is the family's, re-read on every admission. A new sign-in
    // starts from the level the person approved; a refresh keeps whatever the
    // family holds now, not what its oldest token was issued with.
    let family_level = match file
        .families
        .iter()
        .find(|record| record.family == family)
        .map(|record| record.level)
    {
        Some(existing) => existing,
        None => {
            let created_at = file
                .tokens
                .iter()
                .find(|token| token.family == family)
                .map(|token| token.issued_at)
                .unwrap_or(now);
            file.families.push(FamilyRecord {
                family: family.clone(),
                client_id: client_id.to_owned(),
                level,
                created_at,
            });
            level
        }
    };
    let access_token = format!("h4rn_at_{}", random_hex());
    let refresh_token = format!("h4rn_rt_{}", random_hex());
    let access_expires = family_deadline(file, &family, family_level, now, ACCESS_TTL);
    let refresh_expires = family_deadline(file, &family, family_level, now, REFRESH_TTL);
    file.tokens.push(TokenRecord {
        hash: digest(&access_token),
        kind: TokenKind::Access,
        client_id: client_id.to_owned(),
        level: family_level,
        family: family.clone(),
        resource: resource.clone(),
        issued_at: now,
        expires_at: access_expires,
        revoked_at: None,
        last_used_at: None,
    });
    file.tokens.push(TokenRecord {
        hash: digest(&refresh_token),
        kind: TokenKind::Refresh,
        client_id: client_id.to_owned(),
        level: family_level,
        family,
        resource: resource.clone(),
        issued_at: now,
        expires_at: refresh_expires,
        revoked_at: None,
        last_used_at: None,
    });
    // Keep the file bounded: an expired token older than sixty days says
    // nothing a person still needs. Revoked tokens stay long enough for an
    // in-flight call to see the revocation rather than a "unknown token".
    //
    // A rotated refresh token is the only record that a replay is a replay. It
    // stays as long as its family has any live token, even after its own
    // expiry: otherwise a family kept alive by rotation would forget the old
    // token, and a replay after sixty days would look merely unknown instead
    // of revoking the family.
    let live_families: Vec<String> = file
        .tokens
        .iter()
        .filter(|token| token.live(now))
        .map(|token| token.family.clone())
        .collect();
    file.tokens.retain(|token| {
        token.expires_at > now - TimeDelta::days(60) || live_families.contains(&token.family)
    });
    // A family record with no token left describes nothing.
    let known_families: Vec<String> = file
        .tokens
        .iter()
        .map(|token| token.family.clone())
        .collect();
    file.families
        .retain(|record| known_families.contains(&record.family));
    // A spent code is the only record that its family was issued from it. A
    // replay is an attack, and the replay must revoke that family; forgetting
    // the code at code expiry would turn a replay after ten minutes into an
    // "unknown code" with nothing to revoke. Keep it while any token in its
    // family is still live. One rule, shared with `approve`, so both paths that
    // prune the code list agree.
    prune_spent_codes(file, now);
    file.pending.retain(|pending| pending.expires_at > now);
    TokenPair {
        access_token,
        refresh_token,
        expires_in: ACCESS_TTL.num_seconds() as u64,
        level: family_level,
        resource,
    }
}

/// The expiry one token of `family` may have: its ordinary lifetime, or the
/// family's Full-access ceiling when that is sooner.
fn family_deadline(
    file: &StoreFile,
    family: &str,
    level: OutsideAgentLevel,
    now: DateTime<Utc>,
    lifetime: TimeDelta,
) -> DateTime<Utc> {
    let normal = now + lifetime;
    if level != OutsideAgentLevel::FullAccess {
        return normal;
    }
    let Some(created_at) = file
        .families
        .iter()
        .find(|record| record.family == family)
        .map(|record| record.created_at)
    else {
        return normal;
    };
    normal.min(created_at + FULL_ACCESS_TTL)
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
        // These cases exercise the flow behind the Labs switch; the switch
        // itself, including its default, has its own cases below.
        store.set_enabled(true).expect("turn outside agents on");
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
            .exchange_code(
                &code,
                &verifier,
                &client.client_id,
                None,
                Some("http://127.0.0.1:47800/mcp"),
            )
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
    fn the_switch_is_off_by_default_and_refuses_a_sign_in() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = OAuthStore::open(dir.path()).expect("store");
        assert!(!store.enabled(), "outside agents must start off");
        let client = store
            .register_client(Some("Claude Code"), &["http://localhost:1455/cb".into()])
            .expect("register");
        let (_, challenge) = challenge();
        let error = store
            .begin_authorization(
                &client.client_id,
                "http://localhost:1455/cb",
                None,
                &challenge,
                None,
            )
            .expect_err("a sign-in cannot be created while the switch is off");
        assert!(
            matches!(error, OAuthError::NotFound(_)),
            "unexpected error: {error:?}"
        );
        assert!(store.pending_sign_ins().is_empty());
    }

    #[test]
    fn a_live_token_is_refused_while_the_switch_is_off() {
        let (_dir, store) = store();
        let client = store
            .register_client(None, &["http://localhost:1455/cb".into()])
            .expect("register");
        let pair = pair(&store, &client.client_id, OutsideAgentLevel::ReadOnly);
        assert!(store.verify_access(&pair.access_token, None).is_some());
        // The state a crash between the switch and a revocation could leave:
        // the token is still live in the file, but the switch is off. The
        // bearer gate, not the revocation, must refuse it.
        store.lock().file.enabled = Some(false);
        assert!(store.verify_access(&pair.access_token, None).is_none());
    }

    #[test]
    fn turning_the_switch_on_allows_a_sign_in() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = OAuthStore::open(dir.path()).expect("store");
        store.set_enabled(true).expect("turn on");
        assert!(store.enabled());
        let client = store
            .register_client(None, &["http://localhost:1455/cb".into()])
            .expect("register");
        let pair = pair(&store, &client.client_id, OutsideAgentLevel::ReadOnly);
        assert!(store.verify_access(&pair.access_token, None).is_some());
    }

    #[test]
    fn turning_the_switch_off_ends_every_grant_and_survives_a_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = OAuthStore::open(dir.path()).expect("store");
        store.set_enabled(true).expect("turn on");
        let first = store
            .register_client(None, &["http://localhost:1455/cb".into()])
            .expect("register first");
        let second = store
            .register_client(None, &["http://localhost:1455/cb".into()])
            .expect("register second");
        let first_pair = pair(&store, &first.client_id, OutsideAgentLevel::ReadOnly);
        let second_pair = pair(&store, &second.client_id, OutsideAgentLevel::FullAccess);
        let (_, challenge) = challenge();
        let waiting = store
            .begin_authorization(
                &first.client_id,
                "http://localhost:1455/cb",
                None,
                &challenge,
                None,
            )
            .expect("begin");
        assert_eq!(store.connected_agents().len(), 2);
        assert_eq!(store.pending_sign_ins().len(), 1);

        store.set_enabled(false).expect("turn off");

        assert!(!store.enabled());
        assert!(
            store.connected_agents().is_empty(),
            "a grant outlived the switch"
        );
        assert!(
            store.pending_sign_ins().is_empty(),
            "a sign-in outlived the switch"
        );
        assert!(store.pending(&waiting.id).is_none());
        for (client, pair) in [
            (&first.client_id, &first_pair),
            (&second.client_id, &second_pair),
        ] {
            assert!(store.verify_access(&pair.access_token, None).is_none());
            assert!(
                store.refresh(&pair.refresh_token, client, None).is_err(),
                "a refresh token outlived the switch"
            );
        }

        drop(store);
        let reopened = OAuthStore::open(dir.path()).expect("reopen");
        assert!(!reopened.enabled(), "the switch did not survive a restart");
        assert!(reopened.connected_agents().is_empty());
        assert!(
            reopened
                .verify_access(&first_pair.access_token, None)
                .is_none()
        );
        assert!(
            reopened
                .verify_access(&second_pair.access_token, None)
                .is_none()
        );
        assert!(reopened.pending_sign_ins().is_empty());

        // Turning it back on must not revive the grants it ended.
        reopened.set_enabled(true).expect("turn back on");
        assert!(
            reopened
                .verify_access(&first_pair.access_token, None)
                .is_none()
        );
        assert!(
            reopened
                .verify_access(&second_pair.access_token, None)
                .is_none()
        );
    }

    #[test]
    fn a_file_from_before_the_switch_is_off_and_its_grants_do_not_come_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = OAuthStore::open(dir.path()).expect("store");
        store.set_enabled(true).expect("turn on");
        let client = store
            .register_client(None, &["http://localhost:1455/cb".into()])
            .expect("register");
        let pair = pair(&store, &client.client_id, OutsideAgentLevel::ReadOnly);
        drop(store);

        // An older build wrote this file without the switch at all.
        let path = dir.path().join(FILE);
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("json");
        value
            .as_object_mut()
            .expect("object")
            .remove("enabled")
            .expect("the switch field");
        std::fs::write(&path, serde_json::to_vec_pretty(&value).expect("json")).expect("write");

        let reopened = OAuthStore::open(dir.path()).expect("reopen");
        assert!(!reopened.enabled(), "an old file must default to off");
        assert!(reopened.verify_access(&pair.access_token, None).is_none());
        reopened.set_enabled(true).expect("turn on");
        assert!(
            reopened.verify_access(&pair.access_token, None).is_none(),
            "an old grant came back when the switch was turned on"
        );
    }

    #[test]
    fn a_revocation_survives_a_restart_even_when_use_raced_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = OAuthStore::open(dir.path()).expect("store");
        store.set_enabled(true).expect("turn on");
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

    /// A grant is live while any token in its family is.
    ///
    /// The access token lasts an hour and the refresh token thirty days; a
    /// watch or a parked prompt must not die at the first hour.
    #[test]
    fn a_grant_outlives_its_access_token_while_the_refresh_token_lives() {
        let (_dir, store) = store();
        let client = store
            .register_client(None, &["http://localhost:1455/cb".into()])
            .expect("register");
        let pair = pair(&store, &client.client_id, OutsideAgentLevel::Supervised);
        let family = store
            .lock()
            .file
            .tokens
            .iter()
            .find(|token| token.hash == digest(&pair.access_token))
            .map(|token| token.family.clone())
            .expect("the access token's family");
        assert!(store.grant_is_live(&family));
        assert!(!store.grant_is_live("another-family"));

        store
            .expire_access_tokens_for_test()
            .expect("expire the access token");
        assert!(
            store.grant_is_live(&family),
            "the grant died when its access token expired while the refresh token was live"
        );

        store
            .revoke_token(&pair.refresh_token)
            .expect("revoke family");
        assert!(!store.grant_is_live(&family));
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

    /// A revocation must leave a grant the person can retry.
    ///
    /// The store used to remove the client from memory before the file held
    /// the revocation. A failed write then answered the retry with "no such
    /// client" while the disk still had the live grant.
    #[test]
    fn a_failed_revocation_leaves_the_grant_retryable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = OAuthStore::open(dir.path()).expect("store");
        store.set_enabled(true).expect("turn on");
        let client = store
            .register_client(None, &["http://localhost:1455/cb".into()])
            .expect("register");
        let pair = pair(&store, &client.client_id, OutsideAgentLevel::ReadOnly);

        store.fail_next_persists_for_tests(1);
        let error = store
            .revoke_client(&client.client_id)
            .expect_err("a write that fails is an error");
        assert!(matches!(error, OAuthError::Storage(_)), "{error:?}");
        // Nothing changed: the grant is still connected and a retry finds it.
        assert_eq!(store.connected_agents().len(), 1);
        assert!(store.verify_access(&pair.access_token, None).is_some());

        store
            .revoke_client(&client.client_id)
            .expect("the retry revokes for real");
        assert!(store.verify_access(&pair.access_token, None).is_none());
        assert!(store.connected_agents().is_empty());
        drop(store);

        let reopened = OAuthStore::open(dir.path()).expect("reopen");
        assert!(
            reopened.verify_access(&pair.access_token, None).is_none(),
            "the retry was not durable"
        );
        assert!(reopened.connected_agents().is_empty());
        // A grant revoked once never comes back, and a later revoke is not a
        // silent success for a grant that no longer exists.
        assert!(matches!(
            reopened.revoke_client(&client.client_id),
            Err(OAuthError::NotFound(_))
        ));
    }

    /// A switch-off that cannot be saved leaves the switch on and retryable.
    ///
    /// The switch and the revocations are one write. If that write fails, the
    /// state must stay exactly as the person left it: still on, grants still
    /// working, and the next attempt able to turn it off for real.
    #[test]
    fn a_failed_switch_off_leaves_the_switch_and_the_grants_retryable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = OAuthStore::open(dir.path()).expect("store");
        store.set_enabled(true).expect("turn on");
        let client = store
            .register_client(None, &["http://localhost:1455/cb".into()])
            .expect("register");
        let pair = pair(&store, &client.client_id, OutsideAgentLevel::ReadOnly);

        store.fail_next_persists_for_tests(1);
        let error = store
            .set_enabled(false)
            .expect_err("a write that fails is an error");
        assert!(matches!(error, OAuthError::Storage(_)), "{error:?}");
        // The switch is still on and the grant still answers.
        assert!(store.enabled(), "a failed write turned the switch off");
        assert!(
            store.verify_access(&pair.access_token, None).is_some(),
            "a failed write revoked the grant"
        );

        assert!(
            !store.set_enabled(false).expect("the retry turns it off"),
            "the retry must report the new state"
        );
        assert!(!store.enabled());
        assert!(store.verify_access(&pair.access_token, None).is_none());
        assert!(store.connected_agents().is_empty());
        drop(store);

        let reopened = OAuthStore::open(dir.path()).expect("reopen");
        assert!(!reopened.enabled(), "the retry was not durable");
        assert!(reopened.verify_access(&pair.access_token, None).is_none());
    }

    /// A spent code outlives the code lifetime while its family lives.
    ///
    /// Pruning at code expiry forgot the only record that the family was issued
    /// from a code, so a replay ten minutes later looked like an unknown code
    /// and revoked nothing.
    #[test]
    fn a_code_replay_after_code_expiry_revokes_the_family() {
        let (_dir, store) = store();
        let client = store
            .register_client(None, &["http://localhost:1455/cb".into()])
            .expect("register");
        let (verifier, _) = challenge();
        let code = approved_code(&store, &client.client_id, OutsideAgentLevel::ReadOnly);
        let first = store
            .exchange_code(&code, &verifier, &client.client_id, None, None)
            .expect("first use");

        // The code is now past its lifetime; the family is still live.
        {
            let mut state = store.lock();
            for entry in &mut state.file.codes {
                entry.expires_at = Utc::now() - TimeDelta::seconds(1);
            }
        }
        // Another issue prunes expired records, as any later grant would.
        let rotated = store
            .refresh(&first.refresh_token, &client.client_id, None)
            .expect("rotate");
        assert!(store.verify_access(&rotated.access_token, None).is_some());

        // The replay still knows the family and kills it.
        let error = store
            .exchange_code(&code, &verifier, &client.client_id, None, None)
            .expect_err("a replay is refused");
        assert!(matches!(error, OAuthError::InvalidGrant(_)), "{error:?}");
        assert!(
            store.verify_access(&rotated.access_token, None).is_none(),
            "the reused code did not revoke its family"
        );
        assert!(
            store
                .refresh(&rotated.refresh_token, &client.client_id, None)
                .is_err()
        );
    }

    /// Approving a second sign-in must not prune a spent code while its family
    /// is live: the replay of that code is what revokes the family.
    ///
    /// `approve` used to drop every expired code, so a second approval in the
    /// same client forgot the first family's code. A replay after that was an
    /// unknown code and the family stayed live.
    #[test]
    fn approving_again_keeps_a_spent_code_whose_family_is_live() {
        let (_dir, store) = store();
        let client = store
            .register_client(None, &["http://localhost:1455/cb".into()])
            .expect("register");
        let (verifier, _) = challenge();
        let code = approved_code(&store, &client.client_id, OutsideAgentLevel::ReadOnly);
        let first = store
            .exchange_code(&code, &verifier, &client.client_id, None, None)
            .expect("first use");

        // The first code is past its lifetime, and a second sign-in is approved.
        {
            let mut state = store.lock();
            for entry in &mut state.file.codes {
                entry.expires_at = Utc::now() - TimeDelta::seconds(1);
            }
        }
        let _second = approved_code(&store, &client.client_id, OutsideAgentLevel::ReadOnly);
        assert!(
            store
                .lock()
                .file
                .codes
                .iter()
                .any(|entry| entry.family.is_some()),
            "the second approval pruned the spent code of a live family"
        );

        // Replaying the first code still recognises the family and kills it.
        let error = store
            .exchange_code(&code, &verifier, &client.client_id, None, None)
            .expect_err("a replay is refused");
        assert!(matches!(error, OAuthError::InvalidGrant(_)), "{error:?}");
        assert!(
            store.verify_access(&first.access_token, None).is_none(),
            "the replay after a second approval left the first family live"
        );
    }

    /// A replay whose revocation write fails is reported, not swallowed.
    #[test]
    fn a_failed_replay_revocation_is_reported() {
        let (_dir, store) = store();
        let client = store
            .register_client(None, &["http://localhost:1455/cb".into()])
            .expect("register");
        let (verifier, _) = challenge();
        let code = approved_code(&store, &client.client_id, OutsideAgentLevel::ReadOnly);
        store
            .exchange_code(&code, &verifier, &client.client_id, None, None)
            .expect("first use");

        store.fail_next_persists_for_tests(1);
        let error = store
            .exchange_code(&code, &verifier, &client.client_id, None, None)
            .expect_err("the replay's revocation could not be saved");
        assert!(matches!(error, OAuthError::Storage(_)), "{error:?}");
    }

    /// One audience binding on exchange, refresh and use.
    #[test]
    fn every_token_path_enforces_the_same_resource_binding() {
        let (_dir, store) = store();
        let client = store
            .register_client(None, &["http://localhost:1455/cb".into()])
            .expect("register");
        let (verifier, challenge) = challenge();
        let audience = "http://127.0.0.1:47800/mcp";
        let pending = store
            .begin_authorization(
                &client.client_id,
                "http://localhost:1455/cb",
                None,
                &challenge,
                Some(audience),
            )
            .expect("begin");
        let redirect = store
            .approve(&pending.id, OutsideAgentLevel::ReadOnly)
            .expect("approve");
        let code = Url::parse(&redirect.location)
            .expect("url")
            .query_pairs()
            .find(|(key, _)| key == "code")
            .map(|(_, value)| value.to_string())
            .expect("code");

        // Exchange: an omitted audience is not "any audience".
        let error = store
            .exchange_code(&code, &verifier, &client.client_id, None, None)
            .expect_err("an omitted resource cannot match a bound code");
        assert!(matches!(error, OAuthError::InvalidTarget(_)), "{error:?}");
        let pair = store
            .exchange_code(&code, &verifier, &client.client_id, None, Some(audience))
            .expect("the exact audience works");

        // Use: no audience is not this endpoint's token.
        assert!(
            store
                .verify_access(&pair.access_token, Some(audience))
                .is_some()
        );
        assert!(store.verify_access(&pair.access_token, None).is_none());
        assert!(
            store
                .verify_access(&pair.access_token, Some("http://127.0.0.1:1/mcp"))
                .is_none()
        );

        // Refresh: same rule, and the rotated pair keeps the binding.
        assert!(
            store
                .refresh(&pair.refresh_token, &client.client_id, None)
                .is_err()
        );
        let rotated = store
            .refresh(&pair.refresh_token, &client.client_id, Some(audience))
            .expect("refresh with the bound audience");
        assert!(
            store
                .verify_access(&rotated.access_token, Some(audience))
                .is_some()
        );
    }

    /// A revocation wins a race with a last-used write.
    ///
    /// The store once snapshotted its state, released the lock and wrote the
    /// snapshot later; a use that landed in that window could put a live token
    /// back on disk after the person revoked it. This test parks the use's
    /// write at a gate: with the snapshot shape the revoker finishes first and
    /// the stale write lands after it, which the reopen sees. With the real
    /// shape the write happens under the one state lock, so the revoker waits
    /// and the file never holds a copy older than the revocation.
    #[test]
    fn a_revocation_wins_a_race_with_a_last_used_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = std::sync::Arc::new(OAuthStore::open(dir.path()).expect("store"));
        store.set_enabled(true).expect("turn on");
        let mut last = None;
        for round in 0..3 {
            let client = store
                .register_client(None, &["http://localhost:1455/cb".into()])
                .expect("register");
            let pair = pair(&store, &client.client_id, OutsideAgentLevel::ReadOnly);
            store
                .verify_access(&pair.access_token, None)
                .expect("the first use arms the lazy write");
            let token = pair.access_token.clone();
            let gate = store.arm_last_use_gate_for_tests();
            let revoked = std::sync::Arc::new(AtomicBool::new(false));

            let use_store = std::sync::Arc::clone(&store);
            let use_token = token.clone();
            let use_revoked = std::sync::Arc::clone(&revoked);
            let uses = std::thread::spawn(move || {
                while !use_revoked.load(Ordering::SeqCst) {
                    // Keep the lazy write due on every pass, so a writer that
                    // persists outside the lock has a stale copy to land.
                    {
                        let mut state = use_store.lock();
                        state.last_use_write =
                            Instant::now() - LAST_USE_INTERVAL - Duration::from_secs(1);
                    }
                    let _ = use_store.verify_access(&use_token, None);
                    std::thread::yield_now();
                }
            });

            // One use write is parked inside `persist_locked`.
            gate.wait_until_entered();
            // Later passes must not park, or the revocation could never run.
            store.disarm_last_use_gate_for_tests();
            let revoking = std::sync::Arc::clone(&store);
            let client_id = client.client_id.clone();
            let revoke_revoked = std::sync::Arc::clone(&revoked);
            let (done, done_rx) = std::sync::mpsc::channel();
            let revoker = std::thread::spawn(move || {
                let result = revoking.revoke_client(&client_id);
                // The use loop ends only after the revoke has returned.
                revoke_revoked.store(true, Ordering::SeqCst);
                let _ = done.send(result);
            });
            // The gate is released after a bounded wait, so a shape that keeps
            // the lock until the write finishes cannot deadlock the test.
            let early = done_rx.recv_timeout(Duration::from_secs(1));
            gate.release();
            revoker.join().expect("the revoker");
            uses.join().expect("the use loop");
            let result = match early {
                Ok(result) => result,
                Err(_) => done_rx
                    .recv_timeout(Duration::from_secs(1))
                    .expect("the revoke result"),
            };
            result.expect("the revoke succeeded");
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
                    "round {round}: the revocation was lost in memory"
                );
                row.last_used_at
            };
            last = Some((hash, recorded));
        }

        let (hash, recorded) = last.expect("one round ran");
        drop(store);
        let reopened = OAuthStore::open(dir.path()).expect("reopen");
        {
            let state = reopened.lock();
            let row = state
                .file
                .tokens
                .iter()
                .find(|entry| entry.hash == hash)
                .expect("the revoked token row survived");
            assert!(
                row.revoked_at.is_some(),
                "a use that raced the revocation resurrected the token"
            );
            assert_eq!(
                row.last_used_at, recorded,
                "the reopened store carries a different last-used value"
            );
        }
        assert_eq!(reopened.client_count(), 0, "a revoked client came back");
    }

    /// A durable write must flush the directory entry after the rename.
    ///
    /// Without it the file can come back as the old version after a crash, so a
    /// revocation that reported success would be undone.
    #[test]
    fn a_durable_write_syncs_the_directory_after_the_rename() {
        let (_dir, store) = store();
        let before = store.hooks.dir_syncs.load(Ordering::SeqCst);
        store
            .register_client(Some("Claude Code"), &["http://localhost:1455/cb".into()])
            .expect("register");
        assert!(
            store.hooks.dir_syncs.load(Ordering::SeqCst) > before,
            "the write renamed the file without syncing its directory"
        );
    }

    /// Full access has a ceiling, and a refresh cannot extend past it.
    #[test]
    fn a_full_access_grant_stops_at_its_ceiling() {
        let (_dir, store) = store();
        let client = store
            .register_client(None, &["http://localhost:1455/cb".into()])
            .expect("register");
        let pair = pair(&store, &client.client_id, OutsideAgentLevel::FullAccess);
        let grant = store
            .verify_access(&pair.access_token, None)
            .expect("the pair is live");
        assert_eq!(grant.level, OutsideAgentLevel::FullAccess);

        store
            .age_full_access_family_for_test(&grant.family)
            .expect("age the family");
        let refused = store
            .refresh(&pair.refresh_token, &client.client_id, None)
            .expect_err("a full-access grant past its ceiling must not refresh");
        assert!(
            refused.to_string().contains("24-hour"),
            "the refusal names the ceiling: {refused}"
        );
        assert!(
            store.verify_access(&pair.access_token, None).is_none(),
            "the ceiling passed but the family stayed live"
        );
    }

    /// Admission reads the family's level, not the token issued under it.
    #[test]
    fn a_grant_level_change_applies_without_a_new_token() {
        let (_dir, store) = store();
        let client = store
            .register_client(None, &["http://localhost:1455/cb".into()])
            .expect("register");
        let pair = pair(&store, &client.client_id, OutsideAgentLevel::FullAccess);
        let grant = store
            .verify_access(&pair.access_token, None)
            .expect("the pair is live");
        store
            .set_family_level_for_test(&grant.family, OutsideAgentLevel::Supervised)
            .expect("lower the level");

        let admitted = store
            .verify_access(&pair.access_token, None)
            .expect("the access token is still live");
        assert_eq!(
            admitted.level,
            OutsideAgentLevel::Supervised,
            "admission used the level frozen into the access token"
        );
        let rotated = store
            .refresh(&pair.refresh_token, &client.client_id, None)
            .expect("refresh");
        assert_eq!(
            rotated.level,
            OutsideAgentLevel::Supervised,
            "the refreshed pair kept the old level"
        );
    }

    /// A rotated refresh record is kept while its family is live, even after
    /// the record's own retention window, so its replay still revokes the
    /// family.
    #[test]
    fn a_rotated_refresh_record_survives_while_its_family_is_live() {
        let (_dir, store) = store();
        let client = store
            .register_client(None, &["http://localhost:1455/cb".into()])
            .expect("register");
        let first = pair(&store, &client.client_id, OutsideAgentLevel::Supervised);
        let rotated = store
            .refresh(&first.refresh_token, &client.client_id, None)
            .expect("rotate");
        store
            .age_rotated_refresh_records_for_test()
            .expect("age the rotated record");
        // Any later issuance prunes the token list under the same rule.
        let _second = pair(&store, &client.client_id, OutsideAgentLevel::Supervised);

        let replay = store
            .refresh(&first.refresh_token, &client.client_id, None)
            .expect_err("the replayed refresh token must be refused");
        assert!(
            replay.to_string().contains("already used"),
            "the replay was treated as unknown instead of as a replay: {replay}"
        );
        assert!(
            store.verify_access(&rotated.access_token, None).is_none(),
            "the replay did not revoke the family it belonged to"
        );
    }
}
