//! Guest links: one thread, one browser, no account (ADR-044).
//!
//! A guest link is a bearer URL. Everything here is deliberately separate from
//! the device protocol: the guest has no Noise identity and no account, so the
//! daemon authenticates the token, binds it to one browser with a cookie, and
//! answers a small HTTP surface instead of opening a `/ws` session.
//!
//! Security rules enforced here, never by the page:
//! - only the hash of the link token and of the browser cookie are stored;
//! - both comparisons are constant time;
//! - the link names exactly one thread and every request is checked against it;
//! - the scope can read that thread and add a waiting message, nothing else;
//! - files are limited by count, size and image type, and deleted on dismiss,
//!   revoke, a new link and settling the thread.

use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{DateTime, Duration, Utc};
use h4rn_protocol::{
    AttachmentId, Event, GUEST_COOKIE, GUEST_LINK_TTL_HOURS, GUEST_MAX_FILES,
    GUEST_MAX_MESSAGE_BYTES, GUEST_MAX_PENDING_BYTES, GuestAttachment, GuestLink, GuestLinkState,
    GuestMessageInput, GuestRequest, GuestSettings, GuestSharing, GuestSnapshot, GuestThreadView,
    Item, ThreadId, ThreadSummary,
};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use subtle::ConstantTimeEq;

use crate::blobs::{BlobStore, UploadOwner, sanitize_name};
use crate::journal::JournalError;
use crate::threads::{ManagerError, ThreadManager};

/// Directory, inside the daemon data directory, holding waiting guest files.
const GUEST_DIR: &str = "guest";

/// Most history a first page load asks for.
const RECENT_EVENTS: usize = 200;

/// Methods a guest may use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// Read a page, state or blob.
    Get,
    /// Submit a message.
    Post,
}

/// One request into the guest surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestHttpInput {
    /// HTTP method.
    pub method: Method,
    /// Path the client asked for, beginning with `/guest/`.
    pub path: String,
    /// Value of the `h4rn_guest` cookie, when the browser sent one.
    pub cookie: Option<String>,
    /// Request body (JSON for a message submission).
    pub body: Vec<u8>,
}

/// One answer from the guest surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestHttpOutput {
    /// HTTP status.
    pub status: u16,
    /// Content type.
    pub content_type: &'static str,
    /// Response body.
    pub body: Vec<u8>,
    /// `Set-Cookie` value, when this response binds the browser.
    pub set_cookie: Option<String>,
    /// Extra headers a client should send (security headers, no-store).
    pub headers: Vec<(&'static str, &'static str)>,
}

impl GuestHttpOutput {
    fn json(status: u16, body: &impl Serialize) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec()),
            set_cookie: None,
            headers: vec![("cache-control", "no-store")],
        }
    }

    fn text(status: u16, message: &str) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: message.as_bytes().to_vec(),
            set_cookie: None,
            headers: vec![("cache-control", "no-store")],
        }
    }
}

/// Guest-link and guest-message failures.
#[derive(Debug, thiserror::Error)]
pub enum GuestError {
    /// The link token is unknown, expired, revoked or disabled.
    #[error("{0}")]
    Link(String),
    /// The request is malformed or over a limit.
    #[error("{0}")]
    Invalid(String),
    /// The thread is gone.
    #[error("{0}")]
    NotFound(String),
    /// The database failed.
    #[error("guest store error: {0}")]
    Database(#[from] sqlx::Error),
    /// A file could not be written or deleted.
    #[error("guest file error: {0}")]
    Io(#[from] std::io::Error),
}

impl GuestError {
    fn status(&self) -> u16 {
        match self {
            Self::Link(_) => 410,
            Self::NotFound(_) => 404,
            Self::Invalid(_) => 400,
            Self::Database(_) | Self::Io(_) => 500,
        }
    }
}

/// What a link resolves to for one request.
struct Resolved {
    link: LinkRow,
    state: GuestLinkState,
    /// The browser must be given this cookie.
    bind_cookie: Option<String>,
}

#[derive(Clone, Debug)]
struct LinkRow {
    link_id: String,
    thread_id: ThreadId,
    binding_hash: Option<String>,
    expires_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredAttachment {
    name: String,
    mime: String,
    size: u64,
    /// File name inside the request's directory.
    file: String,
}

#[derive(Clone, Debug)]
struct RequestRow {
    request_id: String,
    thread_id: ThreadId,
    text: String,
    attachments: Vec<StoredAttachment>,
    created_at: DateTime<Utc>,
    status: String,
}

impl RequestRow {
    fn to_proto(&self) -> GuestRequest {
        GuestRequest {
            request_id: self.request_id.clone(),
            thread_id: self.thread_id.clone(),
            text: self.text.clone(),
            attachments: self
                .attachments
                .iter()
                .map(|attachment| GuestAttachment {
                    name: attachment.name.clone(),
                    mime: attachment.mime.clone(),
                    size: attachment.size,
                })
                .collect(),
            created_at: self.created_at,
        }
    }
}

/// A freshly created link, before the caller turns it into a URL.
#[derive(Clone, Debug)]
pub struct CreatedLink {
    /// Link metadata; `url` is empty until the caller knows its base address.
    pub link: GuestLink,
    /// Raw token. It is returned once and never stored.
    pub token: String,
}

/// Guest links, settings and waiting messages for this daemon.
pub struct GuestService {
    pool: SqlitePool,
    data_dir: PathBuf,
    manager: Arc<ThreadManager>,
    blobs: Arc<BlobStore>,
    /// Raw tokens created since this daemon started, by link id.
    ///
    /// Only the hash is durable, so a restart keeps guests working but the
    /// owner must create a new link to copy the URL again.
    tokens: Mutex<std::collections::HashMap<String, String>>,
    /// Base URL of the daemon's own listener, set once it binds.
    local_base: Mutex<Option<String>>,
}

impl std::fmt::Debug for GuestService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuestService")
            .field("data_dir", &self.data_dir)
            .finish_non_exhaustive()
    }
}

impl GuestService {
    /// Opens the guest store in the journal's database.
    pub async fn open(
        data_dir: impl AsRef<Path>,
        pool: Option<&SqlitePool>,
        manager: Arc<ThreadManager>,
        blobs: Arc<BlobStore>,
    ) -> Result<Self, GuestError> {
        let pool = match pool {
            Some(pool) => pool.clone(),
            None => {
                // A daemon built with a custom journal still gets a working
                // guest surface; the store simply does not share that file.
                SqlitePool::connect("sqlite::memory:").await?
            }
        };
        let service = Self {
            pool,
            data_dir: data_dir.as_ref().to_path_buf(),
            manager,
            blobs,
            tokens: Mutex::new(std::collections::HashMap::new()),
            local_base: Mutex::new(None),
        };
        service.ensure_schema().await?;
        std::fs::create_dir_all(service.guest_root())?;
        Ok(service)
    }

    fn guest_root(&self) -> PathBuf {
        self.data_dir.join(GUEST_DIR)
    }

    /// Records the daemon's own listener address for links that do not use the
    /// relay.
    pub fn set_local_base(&self, base: impl Into<String>) {
        *self
            .local_base
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(base.into().trim_end_matches('/').to_owned());
    }

    /// The daemon's own base URL, when the listener is known.
    pub fn local_base(&self) -> Option<String> {
        self.local_base
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    async fn ensure_schema(&self) -> Result<(), GuestError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS guest_links (
                link_id       TEXT PRIMARY KEY,
                thread_id     TEXT NOT NULL UNIQUE,
                token_hash    TEXT NOT NULL UNIQUE,
                binding_hash  TEXT,
                created_at    TEXT NOT NULL,
                expires_at    TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS guest_settings (
                id      INTEGER PRIMARY KEY CHECK (id = 1),
                enabled INTEGER NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS guest_requests (
                request_id   TEXT PRIMARY KEY,
                link_id      TEXT NOT NULL,
                thread_id    TEXT NOT NULL,
                text         TEXT NOT NULL,
                attachments_json TEXT NOT NULL,
                created_at   TEXT NOT NULL,
                status       TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_guest_requests_thread
             ON guest_requests (thread_id, status)",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Setting and links
    // ------------------------------------------------------------------

    /// Whether guest links are on for this machine.
    pub async fn enabled(&self) -> Result<bool, GuestError> {
        let row = sqlx::query("SELECT enabled FROM guest_settings WHERE id = 1")
            .fetch_optional(&self.pool)
            .await?;
        Ok(row
            .map(|row| row.try_get::<i64, _>("enabled").unwrap_or(0) != 0)
            .unwrap_or(false))
    }

    /// Turns guest links on or off. Off disables every link at once without
    /// deleting them; on makes unexpired links work again.
    pub async fn set_enabled(&self, enabled: bool) -> Result<GuestSettings, GuestError> {
        sqlx::query(
            "INSERT INTO guest_settings (id, enabled) VALUES (1, ?)
             ON CONFLICT(id) DO UPDATE SET enabled = excluded.enabled",
        )
        .bind(i64::from(enabled))
        .execute(&self.pool)
        .await?;
        Ok(GuestSettings { enabled })
    }

    /// Sharing state for one thread.
    pub async fn sharing(
        &self,
        thread_id: &ThreadId,
        base: &str,
    ) -> Result<GuestSharing, GuestError> {
        let enabled = self.enabled().await?;
        let Some(row) = self.link_for_thread(thread_id).await? else {
            return Ok(GuestSharing {
                thread_id: thread_id.clone(),
                links_enabled: enabled,
                link: None,
            });
        };
        let raw = self
            .tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&row.link_id)
            .cloned();
        Ok(GuestSharing {
            thread_id: thread_id.clone(),
            links_enabled: enabled,
            link: Some(GuestLink {
                url: raw.map(|token| self.url_for(base, &token)),
                expires_at: row.expires_at,
                bound: row.binding_hash.is_some(),
            }),
        })
    }

    /// Creates a link for one thread, ending the thread's previous link and
    /// deleting its waiting files.
    pub async fn create(
        &self,
        thread_id: &ThreadId,
        base: &str,
    ) -> Result<CreatedLink, GuestError> {
        if !self.enabled().await? {
            return Err(GuestError::Invalid(
                "Guest links are off. Turn them on in Settings → Labs → Guest links.".into(),
            ));
        }
        // The thread must exist before a link can point at it.
        let snapshot = self
            .manager
            .get_thread(thread_id, u64::MAX)
            .await
            .map_err(|error| GuestError::NotFound(error.to_string()))?;
        let _ = snapshot;
        self.end_for_thread(thread_id).await?;

        let token = random_secret();
        let hash = hash_secret(&token);
        let link_id = format!("link_{}", uuid::Uuid::now_v7());
        let now = Utc::now();
        let expires_at = now + Duration::hours(GUEST_LINK_TTL_HOURS);
        sqlx::query(
            "INSERT INTO guest_links
                (link_id, thread_id, token_hash, binding_hash, created_at, expires_at)
             VALUES (?, ?, ?, NULL, ?, ?)",
        )
        .bind(&link_id)
        .bind(thread_id.as_str())
        .bind(&hash)
        .bind(now.to_rfc3339())
        .bind(expires_at.to_rfc3339())
        .execute(&self.pool)
        .await?;
        self.tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(link_id.clone(), token.clone());
        Ok(CreatedLink {
            link: GuestLink {
                url: Some(self.url_for(base, &token)),
                expires_at,
                bound: false,
            },
            token,
        })
    }

    /// Ends the thread's link and deletes every waiting message and file.
    pub async fn stop_sharing(&self, thread_id: &ThreadId) -> Result<(), GuestError> {
        self.end_for_thread(thread_id).await
    }

    async fn end_for_thread(&self, thread_id: &ThreadId) -> Result<(), GuestError> {
        let Some(link) = self.link_for_thread(thread_id).await? else {
            return Ok(());
        };
        self.end_link(&link).await
    }

    async fn end_link(&self, link: &LinkRow) -> Result<(), GuestError> {
        self.discard_pending(link).await?;
        sqlx::query("DELETE FROM guest_links WHERE link_id = ?")
            .bind(&link.link_id)
            .execute(&self.pool)
            .await?;
        self.tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&link.link_id);
        Ok(())
    }

    /// Deletes every waiting message and file for a link, leaving the link
    /// row itself in place. Used by Stop sharing, a new link, settling and the
    /// first request after the 24 hours pass.
    async fn discard_pending(&self, link: &LinkRow) -> Result<(), GuestError> {
        let pending = self.pending_requests(&link.link_id).await?;
        for request in &pending {
            self.delete_request_files(request)?;
            sqlx::query("DELETE FROM guest_requests WHERE request_id = ?")
                .bind(&request.request_id)
                .execute(&self.pool)
                .await?;
            let _ = self
                .manager
                .emit_guest_request_resolved(&link.thread_id, &request.request_id, false)
                .await;
        }
        Ok(())
    }

    /// Settling (archiving) a thread ends its link.
    pub async fn thread_settled(&self, thread_id: &ThreadId) -> Result<(), GuestError> {
        self.end_for_thread(thread_id).await
    }

    async fn link_for_thread(&self, thread_id: &ThreadId) -> Result<Option<LinkRow>, GuestError> {
        let row = sqlx::query(
            "SELECT link_id, thread_id, binding_hash, created_at, expires_at
             FROM guest_links WHERE thread_id = ?",
        )
        .bind(thread_id.as_str())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_link))
    }

    async fn link_for_token(&self, token: &str) -> Result<Option<LinkRow>, GuestError> {
        let hash = hash_secret(token);
        let row = sqlx::query(
            "SELECT link_id, thread_id, binding_hash, created_at, expires_at, token_hash
             FROM guest_links WHERE token_hash = ?",
        )
        .bind(&hash)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let stored: String = row.try_get("token_hash")?;
        // The index lookup found the row; the comparison still proves the hash
        // matches without branching on the token's bytes.
        if !bool::from(stored.as_bytes().ct_eq(hash.as_bytes())) {
            return Ok(None);
        }
        Ok(Some(row_to_link(row)))
    }

    /// The URL a link should be shared as, given a prefix that already names
    /// the guest surface (`<daemon>/guest` or `<connect>/guest/<machine>`).
    pub fn url_for(&self, prefix: &str, token: &str) -> String {
        format!("{}/{token}", prefix.trim_end_matches('/'))
    }

    // ------------------------------------------------------------------
    // Token resolution
    // ------------------------------------------------------------------

    async fn resolve(
        &self,
        token: &str,
        cookie: Option<&str>,
        bind_allowed: bool,
    ) -> Result<Resolved, GuestError> {
        if !self.enabled().await? {
            return Ok(Resolved {
                link: LinkRow {
                    link_id: String::new(),
                    thread_id: ThreadId::new("disabled"),
                    binding_hash: None,
                    expires_at: Utc::now(),
                },
                state: GuestLinkState::Disabled,
                bind_cookie: None,
            });
        }
        let Some(link) = self.link_for_token(token).await? else {
            return Err(GuestError::Link(
                "This link was stopped or replaced by the owner.".into(),
            ));
        };
        if link.expires_at <= Utc::now() {
            // The link is over: its waiting files must not outlive it. The
            // row stays so the page can still say "expired" instead of
            // "revoked".
            let _ = self.discard_pending(&link).await;
            return Ok(Resolved {
                link,
                state: GuestLinkState::Expired,
                bind_cookie: None,
            });
        }
        match (&link.binding_hash, cookie) {
            (None, _) if bind_allowed => {
                let binding = random_secret();
                let hash = hash_secret(&binding);
                // Two first opens race; only the first wins the NULL slot.
                let updated = sqlx::query(
                    "UPDATE guest_links SET binding_hash = ?
                     WHERE link_id = ? AND binding_hash IS NULL",
                )
                .bind(&hash)
                .bind(&link.link_id)
                .execute(&self.pool)
                .await?;
                if updated.rows_affected() == 0 {
                    // A different browser claimed it first.
                    let fresh = self.link_for_token(token).await?;
                    let bound = fresh.as_ref().is_some_and(|row| row.binding_hash.is_some());
                    return Ok(Resolved {
                        link,
                        state: if bound {
                            GuestLinkState::Unbound
                        } else {
                            GuestLinkState::Active
                        },
                        bind_cookie: None,
                    });
                }
                Ok(Resolved {
                    link,
                    state: GuestLinkState::Active,
                    bind_cookie: Some(binding),
                })
            }
            (None, _) => Ok(Resolved {
                link,
                state: GuestLinkState::Unbound,
                bind_cookie: None,
            }),
            (Some(stored), Some(value)) => {
                let given = hash_secret(value);
                if bool::from(stored.as_bytes().ct_eq(given.as_bytes())) {
                    Ok(Resolved {
                        link,
                        state: GuestLinkState::Active,
                        bind_cookie: None,
                    })
                } else {
                    Ok(Resolved {
                        link,
                        state: GuestLinkState::Unbound,
                        bind_cookie: None,
                    })
                }
            }
            (Some(_), None) => Ok(Resolved {
                link,
                state: GuestLinkState::Unbound,
                bind_cookie: None,
            }),
        }
    }

    // ------------------------------------------------------------------
    // HTTP surface
    // ------------------------------------------------------------------

    /// Answers one guest request.
    pub async fn handle(&self, input: GuestHttpInput) -> GuestHttpOutput {
        let path = input.path.split('?').next().unwrap_or(&input.path);
        let mut parts = path.trim_start_matches('/').split('/');
        if parts.next() != Some("guest") {
            return GuestHttpOutput::text(404, "Not found.");
        }
        let Some(token) = parts.next().filter(|part| !part.is_empty()) else {
            return GuestHttpOutput::text(404, "Not found.");
        };
        let rest: Vec<&str> = parts.collect();
        let cookie = input.cookie.as_deref();
        let queried_thumbnail = input.path.contains("thumbnail=1");
        match (input.method, rest.as_slice()) {
            (Method::Get, []) => match self.resolve(token, cookie, true).await {
                Ok(resolved) => self.page(resolved, path),
                Err(error) => self.error(error),
            },
            (Method::Get, ["state"]) => match self.resolve(token, cookie, false).await {
                Ok(resolved) => match self.snapshot(&resolved, 0).await {
                    Ok(snapshot) => GuestHttpOutput::json(200, &snapshot),
                    Err(error) => self.error(error),
                },
                Err(error) => self.error(error),
            },
            (Method::Get, ["state", seq]) => {
                let since = seq.parse::<u64>().unwrap_or(0);
                match self.resolve(token, cookie, false).await {
                    Ok(resolved) => match self.snapshot(&resolved, since).await {
                        Ok(snapshot) => GuestHttpOutput::json(200, &snapshot),
                        Err(error) => self.error(error),
                    },
                    Err(error) => self.error(error),
                }
            }
            (Method::Post, ["messages"]) => {
                let input: GuestMessageInput = match serde_json::from_slice(&input.body) {
                    Ok(input) => input,
                    Err(error) => {
                        return GuestHttpOutput::text(
                            400,
                            &format!("The message is not valid JSON: {error}"),
                        );
                    }
                };
                match self.resolve(token, cookie, false).await {
                    Ok(resolved) => match self.submit(&resolved, input).await {
                        Ok(request) => GuestHttpOutput::json(202, &request),
                        Err(error) => self.error(error),
                    },
                    Err(error) => self.error(error),
                }
            }
            (Method::Get, ["blob", id]) => match self.resolve(token, cookie, false).await {
                Ok(resolved) => match self.blob(&resolved, id, queried_thumbnail).await {
                    Ok((mime, bytes)) => GuestHttpOutput {
                        status: 200,
                        content_type: mime,
                        body: bytes,
                        set_cookie: None,
                        headers: vec![("cache-control", "private, max-age=3600")],
                    },
                    Err(error) => self.error(error),
                },
                Err(error) => self.error(error),
            },
            _ => GuestHttpOutput::text(404, "Not found."),
        }
    }

    fn error(&self, error: GuestError) -> GuestHttpOutput {
        GuestHttpOutput::text(error.status(), &error.to_string())
    }

    fn page(&self, resolved: Resolved, path: &str) -> GuestHttpOutput {
        // Scope the cookie to this one link, so two guest links opened in the
        // same browser do not overwrite each other's binding.
        let cookie_path = path.trim_end_matches('/');
        let cookie = resolved.bind_cookie.map(|binding| {
            format!(
                "{GUEST_COOKIE}={binding}; HttpOnly; SameSite=Lax; Path={cookie_path}; Max-Age={}",
                GUEST_LINK_TTL_HOURS * 3600
            )
        });
        let state = match resolved.state {
            GuestLinkState::Active => "active",
            GuestLinkState::Expired => "expired",
            GuestLinkState::Revoked => "revoked",
            GuestLinkState::Disabled => "disabled",
            GuestLinkState::Unbound => "unbound",
        };
        GuestHttpOutput {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: guest_page(state).into_bytes(),
            set_cookie: cookie,
            headers: vec![
                ("cache-control", "no-store"),
                (
                    "content-security-policy",
                    "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
                ),
                ("referrer-policy", "no-referrer"),
                ("x-content-type-options", "nosniff"),
            ],
        }
    }

    /// Reads one thread's visible events and the link's state.
    async fn snapshot(&self, resolved: &Resolved, since: u64) -> Result<GuestSnapshot, GuestError> {
        if resolved.state != GuestLinkState::Active {
            return Ok(GuestSnapshot {
                state: resolved.state,
                thread: None,
                events: Vec::new(),
                next_seq: 0,
                notice: Some(notice_for(resolved.state).to_owned()),
            });
        }
        let snapshot = self
            .manager
            .get_thread(&resolved.link.thread_id, since)
            .await
            .map_err(|error| GuestError::NotFound(error.to_string()))?;
        let mut events = snapshot.events;
        if since == 0 && events.len() > RECENT_EVENTS {
            events = events.split_off(events.len() - RECENT_EVENTS);
        }
        let next_seq = events.last().map(|event| event.seq).unwrap_or(since);
        Ok(GuestSnapshot {
            state: GuestLinkState::Active,
            thread: Some(guest_view(&snapshot.summary)),
            events: events
                .into_iter()
                .filter(|event| !matches!(event.event, Event::ThreadUpserted(_)))
                .map(|event| h4rn_protocol::SeqEvent {
                    seq: event.seq,
                    event: sanitise(event.event),
                })
                .collect(),
            next_seq,
            notice: None,
        })
    }

    /// Stores one guest message and queues it for the owner.
    async fn submit(
        &self,
        resolved: &Resolved,
        input: GuestMessageInput,
    ) -> Result<GuestRequest, GuestError> {
        match resolved.state {
            GuestLinkState::Active => {}
            other => return Err(GuestError::Link(notice_for(other).to_owned())),
        }
        let text = input.text.trim().to_owned();
        if text.is_empty() && input.attachments.is_empty() {
            return Err(GuestError::Invalid(
                "Write a message or attach a file.".into(),
            ));
        }
        if input.attachments.len() > GUEST_MAX_FILES {
            return Err(GuestError::Invalid(format!(
                "A message holds at most {GUEST_MAX_FILES} files."
            )));
        }

        let mut pending = 0u64;
        for request in self.pending_requests(&resolved.link.link_id).await? {
            pending = pending.saturating_add(
                request
                    .attachments
                    .iter()
                    .map(|attachment| attachment.size)
                    .sum::<u64>(),
            );
        }

        let mut decoded = Vec::new();
        let mut total = 0u64;
        for attachment in &input.attachments {
            let bytes = BASE64
                .decode(attachment.data.as_bytes())
                .map_err(|_| GuestError::Invalid("One of the files could not be read.".into()))?;
            total = total.saturating_add(bytes.len() as u64);
            if total > GUEST_MAX_MESSAGE_BYTES {
                return Err(GuestError::Invalid(
                    "A message holds at most 10 MB of files.".into(),
                ));
            }
            let mime = sniff_guest_mime(&attachment.mime, &bytes)?;
            let name = sanitize_name(&attachment.name)
                .map_err(|_| GuestError::Invalid("A file name is not valid.".into()))?;
            decoded.push((name, mime, bytes));
        }
        if pending.saturating_add(total) > GUEST_MAX_PENDING_BYTES {
            return Err(GuestError::Invalid(
                "20 MB of files is already waiting for the owner. Send or dismiss some first."
                    .into(),
            ));
        }

        let request_id = format!("greq_{}", uuid::Uuid::now_v7());
        let directory = self.guest_root().join(&request_id);
        std::fs::create_dir_all(&directory)?;
        let mut stored = Vec::new();
        for (index, (name, mime, bytes)) in decoded.into_iter().enumerate() {
            let file = format!("{index}-{name}");
            let path = directory.join(&file);
            let mut handle = std::fs::File::create(&path)?;
            handle.write_all(&bytes)?;
            handle.sync_all()?;
            stored.push(StoredAttachment {
                name,
                mime,
                size: bytes.len() as u64,
                file,
            });
        }

        let created_at = Utc::now();
        let attachments_json = serde_json::to_string(&stored)
            .map_err(|error| GuestError::Invalid(error.to_string()))?;
        sqlx::query(
            "INSERT INTO guest_requests
                (request_id, link_id, thread_id, text, attachments_json, created_at, status)
             VALUES (?, ?, ?, ?, ?, ?, 'pending')",
        )
        .bind(&request_id)
        .bind(&resolved.link.link_id)
        .bind(resolved.link.thread_id.as_str())
        .bind(&text)
        .bind(&attachments_json)
        .bind(created_at.to_rfc3339())
        .execute(&self.pool)
        .await?;

        let row = RequestRow {
            request_id,
            thread_id: resolved.link.thread_id.clone(),
            text,
            attachments: stored,
            created_at,
            status: "pending".into(),
        };
        self.manager
            .emit_guest_request(&row.thread_id, row.to_proto())
            .await
            .map_err(|error| GuestError::NotFound(error.to_string()))?;
        Ok(row.to_proto())
    }

    async fn blob(
        &self,
        resolved: &Resolved,
        id: &str,
        thumbnail: bool,
    ) -> Result<(&'static str, Vec<u8>), GuestError> {
        if resolved.state != GuestLinkState::Active {
            return Err(GuestError::Link(notice_for(resolved.state).to_owned()));
        }
        let id = AttachmentId::new(id);
        if !self
            .blob_belongs_to_thread(&resolved.link.thread_id, &id)
            .await?
        {
            return Err(GuestError::NotFound(
                "That attachment is not part of this thread.".into(),
            ));
        }
        match self.blobs.download(&id, None, thumbnail).await {
            Ok((download, bytes)) => Ok((leak_mime(download.mime), bytes)),
            Err(_) => Err(GuestError::NotFound("That attachment is gone.".into())),
        }
    }

    async fn blob_belongs_to_thread(
        &self,
        thread_id: &ThreadId,
        id: &AttachmentId,
    ) -> Result<bool, GuestError> {
        let snapshot = self
            .manager
            .get_thread(thread_id, 0)
            .await
            .map_err(|error| GuestError::NotFound(error.to_string()))?;
        for event in snapshot.events {
            if let Event::ItemCompleted {
                item: Item::Message { attachments, .. },
                ..
            } = event.event
            {
                if attachments.iter().any(|attachment| &attachment.id == id) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    // ------------------------------------------------------------------
    // Owner actions
    // ------------------------------------------------------------------

    async fn pending_requests(&self, link_id: &str) -> Result<Vec<RequestRow>, GuestError> {
        let rows = sqlx::query(
            "SELECT request_id, link_id, thread_id, text, attachments_json, created_at, status
             FROM guest_requests WHERE link_id = ? AND status = 'pending'
             ORDER BY created_at ASC",
        )
        .bind(link_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_request).collect()
    }

    async fn request_by_id(&self, request_id: &str) -> Result<Option<RequestRow>, GuestError> {
        let row = sqlx::query(
            "SELECT request_id, link_id, thread_id, text, attachments_json, created_at, status
             FROM guest_requests WHERE request_id = ?",
        )
        .bind(request_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(row_to_request).transpose()
    }

    /// Sends one waiting guest message as a normal turn.
    pub async fn send_request(
        &self,
        thread_id: &ThreadId,
        request_id: &str,
    ) -> Result<(), GuestError> {
        let Some(request) = self.request_by_id(request_id).await? else {
            return Err(GuestError::NotFound(
                "That request no longer exists.".into(),
            ));
        };
        if &request.thread_id != thread_id || request.status != "pending" {
            return Err(GuestError::NotFound(
                "That request no longer exists.".into(),
            ));
        }
        let mut ids = Vec::new();
        for attachment in &request.attachments {
            ids.push(
                self.upload_attachment(&request.request_id, attachment)
                    .await?,
            );
        }
        self.manager
            .send_message(thread_id, request.text.clone(), ids)
            .await
            .map_err(|error| match error {
                ManagerError::NotFound(message) => GuestError::NotFound(message),
                other => GuestError::Invalid(other.to_string()),
            })?;
        self.finish_request(&request, true).await
    }

    /// Discards one waiting guest message and its files.
    pub async fn dismiss_request(
        &self,
        thread_id: &ThreadId,
        request_id: &str,
    ) -> Result<(), GuestError> {
        let Some(request) = self.request_by_id(request_id).await? else {
            return Err(GuestError::NotFound(
                "That request no longer exists.".into(),
            ));
        };
        if &request.thread_id != thread_id || request.status != "pending" {
            return Err(GuestError::NotFound(
                "That request no longer exists.".into(),
            ));
        }
        self.finish_request(&request, false).await
    }

    async fn finish_request(&self, request: &RequestRow, sent: bool) -> Result<(), GuestError> {
        self.delete_request_files(request)?;
        sqlx::query("UPDATE guest_requests SET status = ? WHERE request_id = ?")
            .bind(if sent { "sent" } else { "dismissed" })
            .bind(&request.request_id)
            .execute(&self.pool)
            .await?;
        let _ = self
            .manager
            .emit_guest_request_resolved(&request.thread_id, &request.request_id, sent)
            .await;
        Ok(())
    }

    async fn upload_attachment(
        &self,
        request_id: &str,
        attachment: &StoredAttachment,
    ) -> Result<AttachmentId, GuestError> {
        let path = self.guest_root().join(request_id).join(&attachment.file);
        let bytes = std::fs::read(&path)?;
        if bytes.len() as u64 > GUEST_MAX_MESSAGE_BYTES {
            return Err(GuestError::Invalid("That file is too large.".into()));
        }
        let ticket = self
            .blobs
            .begin_upload(
                UploadOwner::Local,
                &attachment.name,
                bytes.len() as u64,
                Some(attachment.mime.clone()),
            )
            .map_err(|error| GuestError::Invalid(error.to_string()))?;
        for (seq, chunk) in bytes
            .chunks(crate::blobs::UPLOAD_CHUNK_SIZE as usize)
            .enumerate()
        {
            self.blobs
                .append_chunk(UploadOwner::Local, ticket.stream_id, seq as u64, chunk)
                .map_err(|error| GuestError::Invalid(error.to_string()))?;
        }
        let digest = blake3::hash(&bytes).to_hex().to_string();
        let info = self
            .blobs
            .commit_upload(UploadOwner::Local, &ticket.upload_id, &digest)
            .await
            .map_err(|error| GuestError::Invalid(error.to_string()))?;
        Ok(info.id)
    }

    fn delete_request_files(&self, request: &RequestRow) -> Result<(), GuestError> {
        let directory = self.guest_root().join(&request.request_id);
        match std::fs::remove_dir_all(&directory) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Deletes every waiting file for a thread and reports the requests as
    /// dismissed. Used when a thread is settled or the daemon shuts down.
    pub async fn delete_pending_for_thread(&self, thread_id: &ThreadId) -> Result<(), GuestError> {
        if let Some(link) = self.link_for_thread(thread_id).await? {
            self.end_link(&link).await?;
        }
        Ok(())
    }
}

fn row_to_link(row: sqlx::sqlite::SqliteRow) -> LinkRow {
    LinkRow {
        link_id: row.try_get("link_id").unwrap_or_default(),
        thread_id: ThreadId::new(row.try_get::<String, _>("thread_id").unwrap_or_default()),
        binding_hash: row.try_get("binding_hash").unwrap_or_default(),
        expires_at: row
            .try_get::<String, _>("expires_at")
            .ok()
            .and_then(|value| DateTime::parse_from_rfc3339(&value).ok())
            .map(|value| value.with_timezone(&Utc))
            .unwrap_or_else(Utc::now),
    }
}

fn row_to_request(row: sqlx::sqlite::SqliteRow) -> Result<RequestRow, GuestError> {
    let attachments: Vec<StoredAttachment> = row
        .try_get::<String, _>("attachments_json")
        .ok()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default();
    Ok(RequestRow {
        request_id: row.try_get("request_id")?,
        thread_id: ThreadId::new(row.try_get::<String, _>("thread_id")?),
        text: row.try_get("text")?,
        attachments,
        created_at: DateTime::parse_from_rfc3339(&row.try_get::<String, _>("created_at")?)
            .map(|value| value.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
        status: row.try_get("status")?,
    })
}

fn guest_view(summary: &ThreadSummary) -> GuestThreadView {
    GuestThreadView {
        thread_id: summary.thread_id.clone(),
        title: summary.title.clone(),
        harness: summary.harness,
        model: summary.model.clone(),
        updated_at: summary.last_activity,
    }
}

/// Removes what a guest must not see from one event.
fn sanitise(event: Event) -> Event {
    match event {
        Event::TurnCompleted {
            thread_id,
            turn_id,
            usage,
            status,
            ..
        } => Event::TurnCompleted {
            thread_id,
            turn_id,
            usage,
            status,
            changes: None,
        },
        Event::ItemCompleted {
            thread_id,
            turn_id,
            item:
                Item::Message {
                    item_id,
                    role,
                    text,
                    mut attachments,
                },
        } => {
            for attachment in &mut attachments {
                // The daemon-host path is not part of the conversation.
                attachment.local_path.clear();
            }
            Event::ItemCompleted {
                thread_id,
                turn_id,
                item: Item::Message {
                    item_id,
                    role,
                    text,
                    attachments,
                },
            }
        }
        other => other,
    }
}

/// Whether the bytes of an image are one of the four allowed formats, or
/// whether a non-image file may pass.
fn sniff_guest_mime(hint: &str, bytes: &[u8]) -> Result<String, GuestError> {
    let sniffed = infer::get(bytes).map(|kind| kind.mime_type().to_owned());
    let claims_image = hint.starts_with("image/")
        || sniffed
            .as_deref()
            .is_some_and(|mime| mime.starts_with("image/"));
    if !claims_image {
        let mime = sniffed
            .or_else(|| {
                (!hint.is_empty() && hint != "application/octet-stream").then(|| hint.to_owned())
            })
            .unwrap_or_else(|| "application/octet-stream".to_owned());
        return Ok(mime);
    }
    let mime = sniffed.unwrap_or_else(|| hint.to_owned());
    match mime.as_str() {
        "image/png" | "image/jpeg" | "image/gif" | "image/webp" => Ok(mime.to_owned()),
        _ => Err(GuestError::Invalid(
            "Images must be PNG, JPEG, GIF or WebP.".into(),
        )),
    }
}

fn leak_mime(mime: String) -> &'static str {
    match mime.as_str() {
        "image/png" => "image/png",
        "image/jpeg" => "image/jpeg",
        "image/gif" => "image/gif",
        "image/webp" => "image/webp",
        "application/pdf" => "application/pdf",
        "text/plain" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn notice_for(state: GuestLinkState) -> &'static str {
    match state {
        GuestLinkState::Active => "",
        GuestLinkState::Expired => "This link expired. Ask the owner for a new one.",
        GuestLinkState::Revoked => "The owner stopped sharing this thread.",
        GuestLinkState::Disabled => "Guest links are off on this machine right now.",
        GuestLinkState::Unbound => "This link was opened in another browser.",
    }
}

fn random_secret() -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
}

fn hash_secret(secret: &str) -> String {
    blake3::hash(secret.as_bytes()).to_hex().to_string()
}

/// The guest page: no framework, no external asset, no fake row.
///
/// The state word is rendered from the server's answer; the timeline and the
/// composer are filled only by the thread's own events.
fn guest_page(state: &str) -> String {
    GUEST_PAGE.replace("__STATE__", state)
}

const GUEST_PAGE: &str = include_str!("guest_page.html");

/// Returns the attachment ids a guest page may display for one thread.
///
/// Kept public for tests: the guest blob route and this set must agree.
pub async fn visible_attachment_ids(
    manager: &ThreadManager,
    thread_id: &ThreadId,
) -> Result<BTreeSet<String>, ManagerError> {
    let snapshot = manager.get_thread(thread_id, 0).await?;
    let mut ids = BTreeSet::new();
    for event in snapshot.events {
        if let Event::ItemCompleted {
            item: Item::Message { attachments, .. },
            ..
        } = event.event
        {
            for attachment in attachments {
                ids.insert(attachment.id.as_str().to_owned());
            }
        }
    }
    Ok(ids)
}

impl From<JournalError> for GuestError {
    fn from(error: JournalError) -> Self {
        GuestError::Invalid(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_hashes_are_stable_and_never_the_token() {
        let token = random_secret();
        assert!(token.len() >= 40);
        assert_ne!(token, random_secret());
        let hash = hash_secret(&token);
        assert_eq!(hash, hash_secret(&token));
        assert_ne!(hash, token);
        assert!(bool::from(
            hash.as_bytes().ct_eq(hash_secret(&token).as_bytes())
        ));
        assert!(!bool::from(
            hash.as_bytes().ct_eq(hash_secret("other").as_bytes())
        ));
    }

    #[test]
    fn image_types_are_narrow_and_other_files_pass() {
        let png = b"\x89PNG\r\n\x1a\n";
        assert_eq!(sniff_guest_mime("image/png", png).unwrap(), "image/png");
        // A BMP claims to be an image but is not one of the four.
        let bmp = b"BM\x00\x00\x00\x00";
        assert!(sniff_guest_mime("image/bmp", bmp).is_err());
        // A text file is not an image and keeps its sniffed type.
        assert_eq!(
            sniff_guest_mime("text/plain", b"hello").unwrap(),
            "text/plain"
        );
    }

    #[test]
    fn the_page_never_contains_a_token() {
        let page = guest_page("active");
        assert!(page.contains("__STATE__") || page.contains("active"));
        assert!(!page.contains("h4rn_guest="));
    }

    #[test]
    fn the_page_renders_every_link_state() {
        for state in ["active", "expired", "revoked", "disabled", "unbound"] {
            let page = guest_page(state);
            assert!(
                page.contains(&format!("data-state=\"{state}\"")),
                "missing {state}"
            );
        }
    }
}
