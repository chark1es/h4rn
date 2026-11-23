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

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{DateTime, Duration, Utc};
use h4rn_protocol::{
    AttachmentId, Event, GUEST_COOKIE, GUEST_LINK_TTL_HOURS, GUEST_MAX_FILES,
    GUEST_MAX_MESSAGE_BYTES, GUEST_MAX_PENDING_BYTES, GUEST_MAX_PENDING_REQUESTS,
    GUEST_MAX_TEXT_BYTES, GuestAttachment, GuestLink, GuestLinkState, GuestMessageInput,
    GuestRequest, GuestSettings, GuestSharing, GuestSnapshot, GuestThreadView, Item, ThreadId,
    ThreadSummary, guest_page_csp, guest_page_html,
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

/// How long a page-load cookie may be presented before it expires unclaimed.
const PENDING_BINDING_MINUTES: i64 = 15;

/// Most unclaimed page-load cookies one link keeps at once.
const MAX_PENDING_BINDINGS: i64 = 8;

/// How often an access decision that failed on I/O is retried before the
/// request is left for the periodic sweep.
const CLEANUP_ATTEMPTS: usize = 3;

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
    /// Path the client asked for on this daemon, beginning with `/guest/`.
    pub path: String,
    /// Public path the browser used, when a relay fronts the daemon.
    ///
    /// The binding cookie is scoped to this path, so a relay-prefixed link
    /// (`/guest/<machine>/<token>`) keeps its cookie.
    pub public_prefix: Option<String>,
    /// Value of the `h4rn_guest` cookie, when the browser sent one.
    pub cookie: Option<String>,
    /// `Content-Type`, for the JSON requirement on submissions.
    pub content_type: Option<String>,
    /// `Origin`, for the same-origin requirement on submissions.
    pub origin: Option<String>,
    /// `Host` (or its forwarded equivalent), for the same-origin check.
    pub host: Option<String>,
    /// `Sec-Fetch-Site`, when a browser sent a fetch-metadata hint.
    pub fetch_site: Option<String>,
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
    pub headers: Vec<(String, String)>,
}

impl GuestHttpOutput {
    /// A JSON answer with `no-store`.
    pub fn json(status: u16, body: &impl Serialize) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec()),
            set_cookie: None,
            headers: vec![("cache-control".to_owned(), "no-store".to_owned())],
        }
    }

    /// A plain-text answer with `no-store`.
    pub fn text(status: u16, message: &str) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: message.as_bytes().to_vec(),
            set_cookie: None,
            headers: vec![("cache-control".to_owned(), "no-store".to_owned())],
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
    /// A submission was not a same-origin JSON POST.
    #[error("{0}")]
    Forbidden(String),
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
            Self::Forbidden(_) => 403,
            Self::Database(_) | Self::Io(_) => 500,
        }
    }
}

/// What a link resolves to for one request.
struct Resolved {
    link: LinkRow,
    state: GuestLinkState,
    /// The browser must be given this cookie (an unclaimed page load).
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
    link_id: String,
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
    /// Serializes the quota check and request insert, so two submissions
    /// cannot both reserve the last of the pending budget.
    submit_lock: tokio::sync::Mutex<()>,
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
            submit_lock: tokio::sync::Mutex::new(()),
        };
        service.ensure_schema().await?;
        std::fs::create_dir_all(service.guest_root())?;
        // A link that expired while the daemon was off must not keep its files
        // waiting forever.
        service.sweep_expired().await?;
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
        // Databases created before the binding was deferred to the first state
        // request need the extra column; the error when it exists is expected.
        let _ = sqlx::query("ALTER TABLE guest_links ADD COLUMN pending_hash TEXT")
            .execute(&self.pool)
            .await;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS guest_bindings (
                link_id       TEXT NOT NULL,
                pending_hash  TEXT NOT NULL UNIQUE,
                created_at    TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_guest_bindings_link
             ON guest_bindings (link_id)",
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
                size_bytes   INTEGER NOT NULL DEFAULT 0,
                created_at   TEXT NOT NULL,
                status       TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;
        let _ = sqlx::query(
            "ALTER TABLE guest_requests ADD COLUMN size_bytes INTEGER NOT NULL DEFAULT 0",
        )
        .execute(&self.pool)
        .await;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_guest_requests_thread
             ON guest_requests (thread_id, status)",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Removes links whose 24 hours passed, and the files waiting for them.
    ///
    /// Runs at startup and periodically, so an expired link and its attachments
    /// do not outlive the link just because nobody opened the page again.
    pub async fn sweep_expired(&self) -> Result<usize, GuestError> {
        let rows = sqlx::query(
            "SELECT link_id, thread_id, binding_hash, created_at, expires_at
             FROM guest_links WHERE expires_at <= ?",
        )
        .bind(Utc::now().to_rfc3339())
        .fetch_all(&self.pool)
        .await?;
        let mut removed = 0;
        for row in rows {
            let link = row_to_link(row);
            self.end_link(&link).await?;
            removed += 1;
        }
        self.sweep_finished_requests().await?;
        Ok(removed)
    }

    /// Deletes files for requests whose link is gone or whose turn settled.
    ///
    /// Access revocation and file cleanup are deliberately separate: ending a
    /// link deletes its row first, so a failing disk never keeps a link alive.
    /// This pass retries the cleanup later.
    async fn sweep_finished_requests(&self) -> Result<(), GuestError> {
        let rows = sqlx::query(
            "SELECT request_id, link_id, thread_id, text, attachments_json, size_bytes, created_at, status
             FROM guest_requests",
        )
        .fetch_all(&self.pool)
        .await?;
        for row in rows {
            let request = row_to_request(row)?;
            let linked: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM guest_links WHERE link_id = ?")
                    .bind(&request.link_id)
                    .fetch_one(&self.pool)
                    .await?;
            if request.status == "pending" && linked > 0 {
                continue;
            }
            // The request is no longer visible to anyone: remove its files,
            // then the row. A failed delete leaves both for the next sweep.
            match self.delete_request_files(&request) {
                Ok(()) => {
                    sqlx::query("DELETE FROM guest_requests WHERE request_id = ?")
                        .bind(&request.request_id)
                        .execute(&self.pool)
                        .await?;
                }
                Err(error) => {
                    tracing::warn!(
                        request = %request.request_id,
                        %error,
                        "guest cleanup is waiting for the next sweep"
                    );
                }
            }
        }
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

    /// Turns guest links on or off.
    ///
    /// Turning the Labs switch off *ends* every link: the token stops working,
    /// waiting messages are discarded and their files are deleted. Turning it
    /// back on does not revive them; the owner shares again.
    pub async fn set_enabled(&self, enabled: bool) -> Result<GuestSettings, GuestError> {
        sqlx::query(
            "INSERT INTO guest_settings (id, enabled) VALUES (1, ?)
             ON CONFLICT(id) DO UPDATE SET enabled = excluded.enabled",
        )
        .bind(i64::from(enabled))
        .execute(&self.pool)
        .await?;
        if !enabled {
            let rows = sqlx::query(
                "SELECT link_id, thread_id, binding_hash, created_at, expires_at
                 FROM guest_links",
            )
            .fetch_all(&self.pool)
            .await?;
            for row in rows {
                self.end_link(&row_to_link(row)).await?;
            }
            self.sweep_finished_requests().await?;
        }
        Ok(GuestSettings { enabled })
    }

    /// Sharing state for one thread.
    ///
    /// `include_url` is false for a device without the admin scope: the link's
    /// existence and expiry are metadata, but the bearer URL is a credential
    /// only an admin may read.
    pub async fn sharing(
        &self,
        thread_id: &ThreadId,
        base: &str,
        include_url: bool,
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
                url: raw
                    .filter(|_| include_url)
                    .map(|token| self.url_for(base, &token)),
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
        if !guest_base_allowed(base) {
            return Err(GuestError::Invalid(
                "A direct guest link needs HTTPS. Link this machine to Connect to share with a teammate on another machine."
                    .into(),
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

    /// Ends a link, revoking access first and cleaning files second.
    ///
    /// The row is deleted before any file work: a disk error must never leave a
    /// token live. Waiting messages are settled afterwards, and anything that
    /// could not be removed is retried by the periodic sweep.
    async fn end_link(&self, link: &LinkRow) -> Result<(), GuestError> {
        sqlx::query("DELETE FROM guest_links WHERE link_id = ?")
            .bind(&link.link_id)
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM guest_bindings WHERE link_id = ?")
            .bind(&link.link_id)
            .execute(&self.pool)
            .await?;
        self.tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&link.link_id);
        self.discard_pending(link).await;
        Ok(())
    }

    /// Settles every waiting message for a link: files first, then the row,
    /// then the event that removes its card.
    ///
    /// A failed file delete leaves the row for [`Self::sweep_finished_requests`]
    /// instead of failing the revocation that already happened.
    async fn discard_pending(&self, link: &LinkRow) {
        let pending = match self.pending_requests(&link.link_id).await {
            Ok(pending) => pending,
            Err(error) => {
                tracing::warn!(%error, "could not list waiting guest messages");
                return;
            }
        };
        for request in &pending {
            let mut deleted = false;
            for attempt in 0..CLEANUP_ATTEMPTS {
                match self.delete_request_files(request) {
                    Ok(()) => {
                        deleted = true;
                        break;
                    }
                    Err(error) if attempt + 1 == CLEANUP_ATTEMPTS => {
                        tracing::warn!(
                            request = %request.request_id,
                            %error,
                            "guest files could not be deleted; the sweep will retry"
                        );
                    }
                    Err(_) => {}
                }
            }
            if !deleted {
                // Keep the row so the sweep knows there is still work to do,
                // but take the card away: the request is no longer actionable.
                let _ = sqlx::query(
                    "UPDATE guest_requests SET status = 'dismissed' WHERE request_id = ?",
                )
                .bind(&request.request_id)
                .execute(&self.pool)
                .await;
                let _ = self
                    .manager
                    .emit_guest_request_resolved(&link.thread_id, &request.request_id, false)
                    .await;
                continue;
            }
            let _ = sqlx::query("DELETE FROM guest_requests WHERE request_id = ?")
                .bind(&request.request_id)
                .execute(&self.pool)
                .await;
            let _ = self
                .manager
                .emit_guest_request_resolved(&link.thread_id, &request.request_id, false)
                .await;
        }
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
        offer_pending: bool,
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
            // "revoked" until the sweep removes it.
            self.discard_pending(&link).await;
            return Ok(Resolved {
                link,
                state: GuestLinkState::Expired,
                bind_cookie: None,
            });
        }
        let link = match &link.binding_hash {
            Some(stored) => {
                let active = cookie.is_some_and(|value| {
                    bool::from(stored.as_bytes().ct_eq(hash_secret(value).as_bytes()))
                });
                return Ok(Resolved {
                    link,
                    state: if active {
                        GuestLinkState::Active
                    } else {
                        GuestLinkState::Unbound
                    },
                    bind_cookie: None,
                });
            }
            None => link,
        };

        // An unbound link: a cookie that matches an unclaimed page load binds
        // the browser now, while the page GET itself only offers one.
        if let Some(value) = cookie
            && self.claim_pending_binding(&link, value).await?
        {
            return Ok(Resolved {
                link,
                state: GuestLinkState::Active,
                bind_cookie: None,
            });
        }
        if offer_pending {
            let pending = random_secret();
            self.offer_pending_binding(&link, &pending).await?;
            return Ok(Resolved {
                link,
                state: GuestLinkState::Active,
                bind_cookie: Some(pending),
            });
        }
        Ok(Resolved {
            link,
            state: GuestLinkState::Unbound,
            bind_cookie: None,
        })
    }

    /// Records one page-load cookie without claiming the link.
    ///
    /// A link-preview crawler loads the page but never presents the cookie, so
    /// the link stays unclaimed until a browser asks for its state.
    async fn offer_pending_binding(&self, link: &LinkRow, pending: &str) -> Result<(), GuestError> {
        let now = Utc::now();
        sqlx::query("DELETE FROM guest_bindings WHERE link_id = ? AND created_at < ?")
            .bind(&link.link_id)
            .bind((now - Duration::minutes(PENDING_BINDING_MINUTES)).to_rfc3339())
            .execute(&self.pool)
            .await?;
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM guest_bindings WHERE link_id = ?")
                .bind(&link.link_id)
                .fetch_one(&self.pool)
                .await?;
        if count >= MAX_PENDING_BINDINGS {
            sqlx::query(
                "DELETE FROM guest_bindings WHERE pending_hash IN (
                    SELECT pending_hash FROM guest_bindings
                    WHERE link_id = ? ORDER BY created_at ASC LIMIT ?
                )",
            )
            .bind(&link.link_id)
            .bind(count - MAX_PENDING_BINDINGS + 1)
            .execute(&self.pool)
            .await?;
        }
        sqlx::query(
            "INSERT OR IGNORE INTO guest_bindings (link_id, pending_hash, created_at)
             VALUES (?, ?, ?)",
        )
        .bind(&link.link_id)
        .bind(hash_secret(pending))
        .bind(now.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Binds an unclaimed link to the browser that presents a page-load cookie.
    async fn claim_pending_binding(
        &self,
        link: &LinkRow,
        cookie: &str,
    ) -> Result<bool, GuestError> {
        let hash = hash_secret(cookie);
        let pending: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM guest_bindings WHERE link_id = ? AND pending_hash = ?",
        )
        .bind(&link.link_id)
        .bind(&hash)
        .fetch_one(&self.pool)
        .await?;
        if pending == 0 {
            return Ok(false);
        }
        let updated = sqlx::query(
            "UPDATE guest_links SET binding_hash = ?
             WHERE link_id = ? AND binding_hash IS NULL",
        )
        .bind(&hash)
        .bind(&link.link_id)
        .execute(&self.pool)
        .await?;
        if updated.rows_affected() == 0 {
            return Ok(false);
        }
        sqlx::query("DELETE FROM guest_bindings WHERE link_id = ?")
            .bind(&link.link_id)
            .execute(&self.pool)
            .await?;
        Ok(true)
    }

    // ------------------------------------------------------------------
    // HTTP surface
    // ------------------------------------------------------------------

    /// Checks a submission before its body is read.
    ///
    /// The full submission checks again; this one exists so an upload from an
    /// unknown browser, or a cross-site form, is refused without buffering its
    /// bytes.
    pub async fn check_submission(&self, input: &GuestHttpInput) -> Result<(), GuestHttpOutput> {
        if let Err(error) = same_origin(input) {
            return Err(self.error(error));
        }
        match self
            .resolve(guest_token(&input.path), input.cookie.as_deref(), false)
            .await
        {
            Ok(resolved) if resolved.state == GuestLinkState::Active => Ok(()),
            Ok(resolved) => {
                Err(self.error(GuestError::Link(notice_for(resolved.state).to_owned())))
            }
            Err(error) => Err(self.error(error)),
        }
    }

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
            (Method::Get, []) => {
                // A page load never claims the link; a preview crawler cannot
                // bind it just by fetching the URL.
                match self.resolve(token, cookie, true).await {
                    Ok(resolved) => self.page(resolved, path, input.public_prefix.as_deref()),
                    Err(error) => self.error(error),
                }
            }
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
                if let Err(error) = same_origin(&input) {
                    return self.error(error);
                }
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
                        headers: vec![(
                            "cache-control".to_owned(),
                            "private, max-age=3600".to_owned(),
                        )],
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

    fn page(&self, resolved: Resolved, path: &str, public_prefix: Option<&str>) -> GuestHttpOutput {
        // Scope the cookie to the path the browser used (a relay adds its
        // machine segment), so a relay-backed link can present its cookie.
        let cookie_path = public_prefix
            .map(|prefix| prefix.trim_end_matches('/'))
            .filter(|prefix| prefix.starts_with("/guest/"))
            .map(str::to_owned)
            .unwrap_or_else(|| path.trim_end_matches('/').to_owned());
        let cookie = resolved.bind_cookie.map(|binding| {
            format!(
                "{GUEST_COOKIE}={binding}; HttpOnly; Secure; SameSite=Strict; Path={cookie_path}; Max-Age={}",
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
            body: guest_page_html(state).into_bytes(),
            set_cookie: cookie,
            headers: vec![
                ("cache-control".to_owned(), "no-store".to_owned()),
                ("content-security-policy".to_owned(), guest_page_csp()),
                ("referrer-policy".to_owned(), "no-referrer".to_owned()),
                ("x-content-type-options".to_owned(), "nosniff".to_owned()),
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
                .filter_map(|entry| {
                    let seq = entry.seq;
                    guest_event(entry.event).map(|event| h4rn_protocol::SeqEvent { seq, event })
                })
                .collect(),
            next_seq,
            notice: None,
        })
    }

    /// Stores one guest message and queues it for the owner.
    ///
    /// The submission is one atomic decision: the link must still be live and
    /// enabled, the pending budget must have room, and the row and its quota
    /// are written together. Files are written before the row exists, so a
    /// racing Stop sharing can never observe a half-written request.
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
        if text.len() > GUEST_MAX_TEXT_BYTES {
            return Err(GuestError::Invalid(format!(
                "A message holds at most {} KB of text.",
                GUEST_MAX_TEXT_BYTES / 1024
            )));
        }
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
        // Text is delivered with the files and shares their budget.
        if total.saturating_add(text.len() as u64) > GUEST_MAX_PENDING_BYTES {
            return Err(GuestError::Invalid(
                "20 MB of messages is already waiting for the owner. Send or dismiss some first."
                    .into(),
            ));
        }

        let request_id = format!("greq_{}", uuid::Uuid::now_v7());
        let directory = self.guest_root().join(&request_id);
        std::fs::create_dir_all(&directory)?;
        let mut stored = Vec::new();
        let write = (|| -> Result<(), GuestError> {
            for (index, (name, mime, bytes)) in decoded.iter().enumerate() {
                let file = format!("{index}-{name}");
                let path = directory.join(&file);
                let mut handle = std::fs::File::create(&path)?;
                handle.write_all(bytes)?;
                handle.sync_all()?;
                stored.push(StoredAttachment {
                    name: name.clone(),
                    mime: mime.clone(),
                    size: bytes.len() as u64,
                    file,
                });
            }
            Ok(())
        })();
        if let Err(error) = write {
            // A failed write must not leave a half-written request behind.
            let _ = std::fs::remove_dir_all(&directory);
            return Err(error);
        }

        let created_at = Utc::now();
        let attachments_json = serde_json::to_string(&stored)
            .map_err(|error| GuestError::Invalid(error.to_string()))?;
        let size_bytes = total.saturating_add(text.len() as u64);

        // Reserve quota and insert in one transaction. The in-process lock
        // makes the check and insert one decision for concurrent submissions;
        // the link guard makes a racing Stop sharing, a new link, an archive
        // or the Labs switch fail the insert instead of leaving an orphan
        // request.
        let _reservation = self.submit_lock.lock().await;
        let mut transaction = self.pool.begin().await?;
        let pending: (i64, i64) = sqlx::query_as(
            "SELECT COUNT(*), COALESCE(SUM(size_bytes), 0) FROM guest_requests
             WHERE link_id = ? AND status = 'pending'",
        )
        .bind(&resolved.link.link_id)
        .fetch_one(&mut *transaction)
        .await?;
        if pending.0 >= GUEST_MAX_PENDING_REQUESTS as i64
            || pending.1.saturating_add(size_bytes as i64) > GUEST_MAX_PENDING_BYTES as i64
        {
            transaction.rollback().await?;
            let _ = std::fs::remove_dir_all(&directory);
            return Err(GuestError::Invalid(
                "Too many messages are already waiting for the owner. Send or dismiss some first."
                    .into(),
            ));
        }
        let inserted = sqlx::query(
            "INSERT INTO guest_requests
                (request_id, link_id, thread_id, text, attachments_json, size_bytes, created_at, status)
             SELECT ?, ?, ?, ?, ?, ?, ?, 'pending'
             WHERE EXISTS (
                 SELECT 1 FROM guest_links
                 WHERE link_id = ? AND expires_at > ?
             ) AND EXISTS (
                 SELECT 1 FROM guest_settings WHERE id = 1 AND enabled = 1
             )",
        )
        .bind(&request_id)
        .bind(&resolved.link.link_id)
        .bind(resolved.link.thread_id.as_str())
        .bind(&text)
        .bind(&attachments_json)
        .bind(size_bytes as i64)
        .bind(created_at.to_rfc3339())
        .bind(&resolved.link.link_id)
        .bind(Utc::now().to_rfc3339())
        .execute(&mut *transaction)
        .await?;
        if inserted.rows_affected() == 0 {
            transaction.rollback().await?;
            let _ = std::fs::remove_dir_all(&directory);
            return Err(GuestError::Link(
                "The owner stopped sharing this thread.".into(),
            ));
        }
        transaction.commit().await?;

        let row = RequestRow {
            request_id,
            link_id: resolved.link.link_id.clone(),
            thread_id: resolved.link.thread_id.clone(),
            text,
            attachments: stored,
            created_at,
            status: "pending".into(),
        };
        if let Err(error) = self
            .manager
            .emit_guest_request(&row.thread_id, row.to_proto())
            .await
        {
            // The thread disappeared between the guard and the event; the
            // request must not outlive the thread it names.
            let _ = std::fs::remove_dir_all(&directory);
            let _ = sqlx::query("DELETE FROM guest_requests WHERE request_id = ?")
                .bind(&row.request_id)
                .execute(&self.pool)
                .await;
            return Err(GuestError::NotFound(error.to_string()));
        }
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
            "SELECT request_id, link_id, thread_id, text, attachments_json, size_bytes, created_at, status
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
            "SELECT request_id, link_id, thread_id, text, attachments_json, size_bytes, created_at, status
             FROM guest_requests WHERE request_id = ?",
        )
        .bind(request_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(row_to_request).transpose()
    }

    /// Sends one waiting guest message as a normal turn.
    ///
    /// The claim is one conditional update, so a concurrent Send or Dismiss
    /// cannot run the message twice: exactly one caller moves it out of
    /// `pending`, and the rest see "no longer exists".
    pub async fn send_request(
        &self,
        thread_id: &ThreadId,
        request_id: &str,
    ) -> Result<(), GuestError> {
        let claimed = sqlx::query(
            "UPDATE guest_requests SET status = 'sending'
             WHERE request_id = ? AND thread_id = ? AND status = 'pending'",
        )
        .bind(request_id)
        .bind(thread_id.as_str())
        .execute(&self.pool)
        .await?;
        if claimed.rows_affected() == 0 {
            return Err(GuestError::NotFound(
                "That request no longer exists.".into(),
            ));
        }
        let Some(request) = self.request_by_id(request_id).await? else {
            return Err(GuestError::NotFound(
                "That request no longer exists.".into(),
            ));
        };
        let result = async {
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
            Ok::<(), GuestError>(())
        }
        .await;
        match result {
            Ok(()) => {
                self.delete_request_files_best_effort(&request);
                sqlx::query("UPDATE guest_requests SET status = 'sent' WHERE request_id = ?")
                    .bind(&request.request_id)
                    .execute(&self.pool)
                    .await?;
                let _ = self
                    .manager
                    .emit_guest_request_resolved(&request.thread_id, &request.request_id, true)
                    .await;
                Ok(())
            }
            Err(error) => {
                // The turn did not start: give the owner the chance to retry.
                let _ = sqlx::query(
                    "UPDATE guest_requests SET status = 'pending' WHERE request_id = ?",
                )
                .bind(&request.request_id)
                .execute(&self.pool)
                .await;
                Err(error)
            }
        }
    }

    /// Discards one waiting guest message and its files.
    ///
    /// Dismiss claims the request with the same conditional update as Send, so
    /// a request can never be both sent and dismissed.
    pub async fn dismiss_request(
        &self,
        thread_id: &ThreadId,
        request_id: &str,
    ) -> Result<(), GuestError> {
        let claimed = sqlx::query(
            "UPDATE guest_requests SET status = 'dismissed'
             WHERE request_id = ? AND thread_id = ? AND status = 'pending'",
        )
        .bind(request_id)
        .bind(thread_id.as_str())
        .execute(&self.pool)
        .await?;
        if claimed.rows_affected() == 0 {
            return Err(GuestError::NotFound(
                "That request no longer exists.".into(),
            ));
        }
        let Some(request) = self.request_by_id(request_id).await? else {
            return Err(GuestError::NotFound(
                "That request no longer exists.".into(),
            ));
        };
        self.delete_request_files_best_effort(&request);
        let _ = self
            .manager
            .emit_guest_request_resolved(&request.thread_id, &request.request_id, false)
            .await;
        Ok(())
    }

    /// Deletes a request's files, leaving the row for the sweep if the disk
    /// refuses. Access has already been decided by the caller.
    fn delete_request_files_best_effort(&self, request: &RequestRow) {
        for attempt in 0..CLEANUP_ATTEMPTS {
            match self.delete_request_files(request) {
                Ok(()) => {
                    let _ = request;
                    return;
                }
                Err(error) if attempt + 1 == CLEANUP_ATTEMPTS => {
                    tracing::warn!(
                        request = %request.request_id,
                        %error,
                        "guest files could not be deleted; the sweep will retry"
                    );
                }
                Err(_) => {}
            }
        }
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
        link_id: row.try_get("link_id")?,
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

/// Projects one thread event for a guest.
///
/// This is an allow-list: a new event variant stays invisible until it is
/// deliberately added here. The guest sees the conversation of one thread,
/// never the workspace, approvals, subagent threads, filesystem events or MCP
/// administration.
fn guest_event(event: Event) -> Option<Event> {
    match event {
        Event::TurnStarted {
            thread_id,
            turn_id,
            started_at,
        } => Some(Event::TurnStarted {
            thread_id,
            turn_id,
            started_at,
        }),
        Event::MessageDelta {
            thread_id,
            turn_id,
            item_id,
            text,
        } => Some(Event::MessageDelta {
            thread_id,
            turn_id,
            item_id,
            text,
        }),
        Event::ItemCompleted {
            thread_id,
            turn_id,
            item,
        } => Some(Event::ItemCompleted {
            thread_id,
            turn_id,
            item: guest_item(item)?,
        }),
        Event::ToolCall {
            thread_id,
            turn_id,
            tool_call,
        } => Some(Event::ToolCall {
            thread_id,
            turn_id,
            tool_call: guest_tool_call(tool_call),
        }),
        Event::Plan {
            thread_id,
            turn_id,
            entries,
        } => Some(Event::Plan {
            thread_id,
            turn_id,
            entries,
        }),
        Event::AgentStateChanged { thread_id, state } => {
            Some(Event::AgentStateChanged { thread_id, state })
        }
        Event::TurnCompleted {
            thread_id,
            turn_id,
            usage,
            status,
            ..
        } => Some(Event::TurnCompleted {
            thread_id,
            turn_id,
            usage,
            status,
            changes: None,
        }),
        Event::GuestRequested { thread_id, request } => {
            Some(Event::GuestRequested { thread_id, request })
        }
        Event::GuestRequestResolved {
            thread_id,
            request_id,
            sent,
        } => Some(Event::GuestRequestResolved {
            thread_id,
            request_id,
            sent,
        }),
        Event::Error {
            thread_id,
            message,
            hint,
            retryable,
        } => Some(Event::Error {
            thread_id,
            message,
            hint,
            retryable,
        }),
        _ => None,
    }
}

/// Projects one completed item, dropping a child thread's row.
fn guest_item(item: Item) -> Option<Item> {
    match item {
        Item::Message {
            item_id,
            role,
            text,
            mut attachments,
        } => {
            for attachment in &mut attachments {
                // The daemon-host path is not part of the conversation.
                attachment.local_path.clear();
            }
            Some(Item::Message {
                item_id,
                role,
                text,
                attachments,
            })
        }
        Item::ToolCall { item_id, tool_call } => Some(Item::ToolCall {
            item_id,
            tool_call: guest_tool_call(tool_call),
        }),
        // A delegated row names and follows another thread, which a guest
        // may not see.
        Item::Delegated { .. } => None,
    }
}

/// Projects a tool call so structured file changes never reach a guest.
///
/// Raw arguments are always dropped: an edit's arguments hold the file's
/// content, and Codex's `apply_patch` puts a unified diff in them. Output from
/// a file-editing tool, or output that is itself a diff, is dropped too; the
/// one-line summary and the added/removed counts stay.
fn guest_tool_call(mut call: h4rn_protocol::ToolCall) -> h4rn_protocol::ToolCall {
    call.input = None;
    if edits_files(&call.name) || call.output.as_deref().is_some_and(looks_like_diff) {
        call.output = None;
    }
    call
}

/// Whether a tool name changes files and may therefore return file content.
fn edits_files(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "edit",
        "write",
        "patch",
        "create_file",
        "delete_file",
        "notebook",
    ]
    .iter()
    .any(|needle| name.contains(needle))
}

/// Whether a tool result is a unified diff rather than ordinary output.
fn looks_like_diff(output: &str) -> bool {
    output.lines().any(|line| {
        line.starts_with("diff --git")
            || line.starts_with("*** Begin Patch")
            || line.starts_with("@@ ")
            || line.starts_with("+++ ")
            || line.starts_with("--- ")
    })
}

/// The link token from an internal guest path (`/guest/<token>/...`).
fn guest_token(path: &str) -> &str {
    path.trim_start_matches('/').split('/').nth(1).unwrap_or("")
}

/// Whether a submission is a same-origin JSON POST.
///
/// A guest submission changes state, so it must never be reachable from another
/// site's page. Browsers send `Origin` on cross-origin POSTs, and a JSON body
/// forces a preflight that the guest surface never answers; this closes both
/// doors and rejects form posts that would not carry JSON.
fn same_origin(input: &GuestHttpInput) -> Result<(), GuestError> {
    let json = input.content_type.as_deref().is_some_and(|value| {
        value
            .split(';')
            .next()
            .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
    });
    if !json {
        return Err(GuestError::Forbidden(
            "Send the message as JSON from the guest page.".into(),
        ));
    }
    if input.fetch_site.as_deref() == Some("cross-site") {
        return Err(GuestError::Forbidden(
            "A guest message must come from the guest page itself.".into(),
        ));
    }
    if let Some(origin) = input.origin.as_deref() {
        let authority = origin
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(origin)
            .trim_end_matches('/');
        let host = input.host.as_deref().unwrap_or("");
        if host.is_empty() || !authority.eq_ignore_ascii_case(host) {
            return Err(GuestError::Forbidden(
                "A guest message must come from the guest page itself.".into(),
            ));
        }
    }
    Ok(())
}

/// Whether bytes of an image are one of the four allowed formats, or
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
    // A claimed image is only accepted when the bytes themselves are one of
    // the four formats. The client's MIME type is never enough.
    match sniffed.as_deref() {
        Some(mime @ ("image/png" | "image/jpeg" | "image/gif" | "image/webp")) => {
            Ok(mime.to_owned())
        }
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

/// Whether a guest link may name this base address.
///
/// A direct link is a bearer token over plain HTTP if it names a LAN address,
/// so only HTTPS and loopback qualify. Loopback links are honest about being
/// usable only on the machine that created them; the share dialog says so.
fn guest_base_allowed(base: &str) -> bool {
    let base = base.trim().to_ascii_lowercase();
    if base.starts_with("https://") {
        return true;
    }
    let Some(rest) = base.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or("");
    authority.starts_with("127.0.0.1")
        || authority.starts_with("localhost")
        || authority.starts_with("[::1]")
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
        let page = guest_page_html("active");
        assert!(page.contains("__STATE__") || page.contains("active"));
        assert!(!page.contains("h4rn_guest="));
    }

    fn file_change_tool() -> h4rn_protocol::ToolCall {
        h4rn_protocol::ToolCall {
            tool_call_id: "tc_patch".into(),
            name: "apply_patch".into(),
            status: h4rn_protocol::ToolCallStatus::Completed,
            summary: "src/main.rs".into(),
            input: Some(serde_json::json!({
                "changes": [{ "path": "src/main.rs", "diff": "@@ -1 +1 @@\n-old\n+new" }]
            })),
            output: Some("@@ -1 +1 @@\n-old\n+new".into()),
            started_at: Utc::now(),
            completed_at: Some(Utc::now()),
            stats: None,
        }
    }

    #[test]
    fn a_file_change_tool_call_loses_its_structured_diff() {
        let thread_id = ThreadId::new("th_guest");
        let turn_id = h4rn_protocol::TurnId::new("turn_guest");
        for event in [
            Event::ToolCall {
                thread_id: thread_id.clone(),
                turn_id: turn_id.clone(),
                tool_call: file_change_tool(),
            },
            Event::ItemCompleted {
                thread_id,
                turn_id,
                item: Item::ToolCall {
                    item_id: "tc_patch".into(),
                    tool_call: file_change_tool(),
                },
            },
        ] {
            let projected = guest_event(event).expect("a tool call is shown");
            let json = serde_json::to_string(&projected).expect("serialise");
            assert!(!json.contains("\"diff\""), "{json}");
            assert!(!json.contains("-old"), "{json}");
            assert!(!json.contains("@@ -1 +1 @@"), "{json}");
            assert!(json.contains("apply_patch"), "the row still names the tool");
        }
    }

    #[test]
    fn an_edit_tools_plain_output_is_also_dropped() {
        let mut call = file_change_tool();
        call.name = "Edit".into();
        call.input = None;
        call.output = Some("new file content".into());
        let projected = guest_tool_call(call);
        assert!(projected.output.is_none());
        // A read tool keeps its output: the guest sees tool output, just not
        // file content from an editing tool.
        let mut read = file_change_tool();
        read.name = "Read".into();
        read.output = Some("hello".into());
        assert_eq!(guest_tool_call(read).output.as_deref(), Some("hello"));
    }

    #[test]
    fn a_guest_projection_drops_workspace_and_approval_events() {
        let summary = ThreadSummary {
            thread_id: ThreadId::new("th_guest"),
            parent_thread_id: None,
            host: h4rn_protocol::HostId::new("host"),
            harness: h4rn_protocol::HarnessKind::ClaudeCode,
            agent_id: None,
            title: "shared".into(),
            workspace_path: "/home/secret/work".into(),
            branch: None,
            model: "fake-model".into(),
            runtime_mode: h4rn_protocol::RuntimeMode::ApprovalRequired,
            reasoning_effort: None,
            reasoning_efforts: Vec::new(),
            models: Vec::new(),
            fast: false,
            tab: h4rn_protocol::ThreadTab::Working,
            placed_by_user: false,
            pinned: false,
            snoozed_until: None,
            last_activity: Utc::now(),
            agent_state: h4rn_protocol::AgentState::Idle,
            usage: h4rn_protocol::Usage::default(),
            ttft_ms: None,
            context_window: None,
            context_fill_percent: None,
            needs_attention: false,
            pending_guest_requests: 0,
            unread: false,
            resume_at: None,
            limit: None,
        };
        assert!(guest_event(Event::ThreadUpserted(summary)).is_none());
        assert!(
            guest_event(Event::ApprovalRequested {
                thread_id: ThreadId::new("th_guest"),
                turn_id: h4rn_protocol::TurnId::new("turn"),
                approval_id: "a1".into(),
                subagent_id: None,
                kind: h4rn_protocol::ApprovalKind::FileChange,
                prompt: "allow?".into(),
                options: Vec::new(),
            })
            .is_none()
        );
        assert!(
            guest_event(Event::FsChanged {
                workspace: "/home/secret/work".into(),
                path: "src/main.rs".into(),
                change: h4rn_protocol::FsChangeKind::Modified,
                at: Utc::now(),
            })
            .is_none()
        );
    }

    #[test]
    fn a_direct_link_is_https_or_loopback_only() {
        assert!(guest_base_allowed("https://h4rn.example/guest"));
        assert!(guest_base_allowed("http://127.0.0.1:47810/guest"));
        assert!(guest_base_allowed("http://[::1]:47810/guest"));
        assert!(!guest_base_allowed("http://192.168.1.9:47810/guest"));
        assert!(!guest_base_allowed("http://h4rn.example/guest"));
    }

    fn post_input(origin: Option<&str>, host: Option<&str>, json: bool) -> GuestHttpInput {
        GuestHttpInput {
            method: Method::Post,
            path: "/guest/tok/messages".into(),
            public_prefix: None,
            cookie: None,
            content_type: Some(if json {
                "application/json".into()
            } else {
                "text/plain".into()
            }),
            origin: origin.map(str::to_owned),
            host: host.map(str::to_owned),
            fetch_site: None,
            body: Vec::new(),
        }
    }

    #[test]
    fn a_guest_post_must_be_same_origin_and_json() {
        assert!(same_origin(&post_input(None, None, true)).is_ok());
        assert!(
            same_origin(&post_input(
                Some("http://127.0.0.1:9"),
                Some("127.0.0.1:9"),
                true
            ))
            .is_ok()
        );
        assert!(
            same_origin(&post_input(
                Some("https://evil.example"),
                Some("h4rn.test"),
                true
            ))
            .is_err()
        );
        assert!(same_origin(&post_input(None, None, false)).is_err());
    }

    #[test]
    fn the_page_renders_every_link_state() {
        for state in ["active", "expired", "revoked", "disabled", "unbound"] {
            let page = guest_page_html(state);
            assert!(
                page.contains(&format!("data-state=\"{state}\"")),
                "missing {state}"
            );
        }
    }
}
