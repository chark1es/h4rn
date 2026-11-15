//! Content-addressed blob storage, chunked uploads and refcount GC (ADR-008).
//!
//! # Layout
//!
//! ```text
//! <data_dir>/blobs/meta.db             SQLite metadata + refcounts
//! <data_dir>/blobs/ab/cdef...          committed blob bytes
//! <data_dir>/blobs/ab/cdef.thumb.png   256 px thumbnail, when generated
//! <data_dir>/blobs/pending/up_...part  in-flight upload bytes
//! <data_dir>/attachments/th_.../...    per-thread materialised copy
//! ```
//!
//! Blobs are addressed by the BLAKE3 digest of their bytes, so two messages
//! carrying the same file store it once. Refcounts keep a blob alive while a
//! thread references it; the GC only removes rows that are unreferenced and
//! older than the configured grace window, which keeps an upload that has just
//! been committed but not yet attached from being swept between two requests.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use h4rn_protocol::{
    Attachment, AttachmentId, BlobDownload, BlobInfo, BlobRange, ProtoError, ProtoErrorCode,
    ThreadId, UploadState, UploadTicket,
};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use super::upload::AppendError;
use super::upload::{UPLOAD_CHUNK_SIZE, UploadOwner, UploadSession};

/// Bytes returned per binary frame by `GetBlob`.
pub const DOWNLOAD_CHUNK_SIZE: usize = 256 * 1024;

/// Longest display name kept in metadata; the filesystem limit is 255 bytes
/// and the id prefix is added on materialisation.
const MAX_NAME_LEN: usize = 120;

/// Attachment storage failures, mapped to protocol errors at the boundary.
#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    /// The blob or upload does not exist (or was swept).
    #[error("attachment {0} does not exist")]
    NotFound(String),
    /// The upload id is unknown.
    #[error("upload {0} does not exist or was already swept")]
    UploadNotFound(String),
    /// A declared size exceeds the configured limit.
    #[error("attachment is {size} bytes; this daemon accepts at most {max}")]
    TooLarge {
        /// Declared size.
        size: u64,
        /// Configured maximum.
        max: u64,
    },
    /// A frame arrived out of order.
    #[error("chunk {got} arrived before the daemon expected it (expected {expected})")]
    ChunkSequence {
        /// Expected sequence.
        expected: u64,
        /// Sequence received.
        got: u64,
    },
    /// One binary frame was larger than the advertised chunk size.
    #[error("chunk is {got} bytes; the daemon accepts at most {max} per frame")]
    ChunkTooLarge {
        /// Payload length.
        got: usize,
        /// Advertised maximum.
        max: usize,
    },
    /// The upload is shorter than declared.
    #[error("upload is incomplete: received {received} of {size} bytes")]
    Incomplete {
        /// Bytes accepted.
        received: u64,
        /// Declared size.
        size: u64,
    },
    /// The declared digest is not a BLAKE3 address.
    #[error("declared hash {0:?} is not 64 lowercase hex characters")]
    InvalidHash(String),
    /// The bytes do not hash to the declared digest.
    #[error("content hash did not match the declared digest")]
    HashMismatch,
    /// A file that claims to be an image does not contain image bytes.
    #[error("the file claims to be an image but its bytes are not a recognised image")]
    InvalidImage,
    /// The display name was unusable.
    #[error("the attachment name is not usable")]
    InvalidName,
    /// The blob database failed.
    #[error("blob database error: {0}")]
    Database(#[from] sqlx::Error),
    /// A file operation failed.
    #[error("attachment storage error: {0}")]
    Io(#[from] std::io::Error),
}

impl BlobError {
    /// The wire error shown under the composer.
    pub fn into_proto(self) -> ProtoError {
        match self {
            Self::NotFound(id) => ProtoError::new(
                ProtoErrorCode::NotFound,
                format!("attachment {id} is no longer available"),
                "Upload the file again, then send the message",
            ),
            Self::UploadNotFound(id) => ProtoError::new(
                ProtoErrorCode::NotFound,
                format!("upload {id} is no longer available"),
                "Start the upload again; nothing was attached",
            ),
            Self::TooLarge { size, max } => ProtoError::new(
                ProtoErrorCode::InvalidRequest,
                format!(
                    "the attachment is {} MiB; the limit is {} MiB",
                    size / (1024 * 1024),
                    max / (1024 * 1024)
                ),
                "Choose a smaller file or split it before sending",
            ),
            Self::ChunkSequence { expected, got } => ProtoError::new(
                ProtoErrorCode::InvalidRequest,
                format!("upload chunk {got} arrived when the daemon expected {expected}"),
                "QueryUpload for the accepted byte count, then resume from there",
            ),
            Self::ChunkTooLarge { got, max } => ProtoError::new(
                ProtoErrorCode::InvalidRequest,
                format!("upload chunk is {got} bytes; the frame limit is {max}"),
                "Split the file into smaller chunks and retry",
            ),
            Self::Incomplete { received, size } => ProtoError::new(
                ProtoErrorCode::InvalidRequest,
                format!("upload has {received} of {size} bytes"),
                "Send the missing bytes, or abort the upload and retry",
            ),
            Self::InvalidHash(value) => ProtoError::new(
                ProtoErrorCode::InvalidRequest,
                format!("{value:?} is not a BLAKE3 content address"),
                "Recompute the 64-character lowercase BLAKE3 digest and retry",
            ),
            Self::HashMismatch => ProtoError::new(
                ProtoErrorCode::InvalidRequest,
                "the uploaded bytes do not match the declared BLAKE3 digest",
                "Recompute the digest from the exact bytes and retry the commit",
            ),
            Self::InvalidImage => ProtoError::new(
                ProtoErrorCode::InvalidRequest,
                "the file is named like an image but does not contain image data",
                "Choose a real PNG, JPEG, GIF or WebP file, or send it as a document",
            ),
            Self::InvalidName => ProtoError::new(
                ProtoErrorCode::InvalidRequest,
                "the attachment name is not usable",
                "Rename the file with a normal name and retry",
            ),
            Self::Database(error) => ProtoError::new(
                ProtoErrorCode::Internal,
                error.to_string(),
                "Retry the request",
            ),
            Self::Io(error) => ProtoError::new(
                ProtoErrorCode::Internal,
                error.to_string(),
                "Retry the request",
            ),
        }
    }
}

/// Resolves upload ids for the thread manager and materialises them per thread.
#[async_trait]
pub trait AttachmentResolver: Send + Sync {
    /// Looks up committed blobs, writes a per-thread copy and retains them.
    async fn resolve(
        &self,
        thread_id: &ThreadId,
        ids: &[AttachmentId],
    ) -> Result<Vec<Attachment>, BlobError>;
}

/// The daemon's content-addressed blob store.
pub struct BlobStore {
    root: PathBuf,
    attachments_root: PathBuf,
    max_bytes: u64,
    grace: Duration,
    pool: SqlitePool,
    uploads: Mutex<UploadBook>,
    next_stream: AtomicU32,
}

#[derive(Default)]
struct UploadBook {
    by_id: HashMap<String, UploadSession>,
    by_stream: HashMap<u32, String>,
}

/// Refuses an upload the caller does not own, without revealing it exists.
fn check_owner(session: &UploadSession, owner: UploadOwner) -> Result<(), BlobError> {
    if session.owner == owner {
        Ok(())
    } else {
        Err(BlobError::UploadNotFound(session.upload_id.clone()))
    }
}

impl std::fmt::Debug for BlobStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobStore")
            .field("root", &self.root)
            .field("max_bytes", &self.max_bytes)
            .field("grace", &self.grace)
            .finish_non_exhaustive()
    }
}

impl BlobStore {
    /// Opens (or creates) the store under `root`.
    pub async fn open(
        root: impl Into<PathBuf>,
        attachments_root: impl Into<PathBuf>,
        max_bytes: u64,
        grace: Duration,
    ) -> Result<Self, BlobError> {
        let root = root.into();
        let attachments_root = attachments_root.into();
        create_private_dir(&root)?;
        create_private_dir(&root.join("pending"))?;
        create_private_dir(&attachments_root)?;

        let options = SqliteConnectOptions::new()
            .filename(root.join("meta.db"))
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;

        let store = Self {
            root,
            attachments_root,
            max_bytes,
            grace,
            pool,
            uploads: Mutex::new(UploadBook::default()),
            next_stream: AtomicU32::new(1),
        };
        store.ensure_schema().await?;
        Ok(store)
    }

    /// The configured maximum blob size.
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    async fn ensure_schema(&self) -> Result<(), BlobError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS blobs (
                id            TEXT PRIMARY KEY,
                name          TEXT NOT NULL,
                mime          TEXT NOT NULL,
                size          INTEGER NOT NULL,
                created_at    TEXT NOT NULL,
                refcount      INTEGER NOT NULL DEFAULT 0,
                has_thumbnail INTEGER NOT NULL DEFAULT 0
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS blob_thread_refs (
                blob_id   TEXT NOT NULL,
                thread_id TEXT NOT NULL,
                refs      INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (blob_id, thread_id)
            )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_blobs_unreferenced
             ON blobs (refcount, created_at)",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Opens a chunked upload and returns the stream id to write to.
    pub fn begin_upload(
        &self,
        owner: UploadOwner,
        name: &str,
        size: u64,
        mime_hint: Option<String>,
    ) -> Result<UploadTicket, BlobError> {
        if size > self.max_bytes {
            return Err(BlobError::TooLarge {
                size,
                max: self.max_bytes,
            });
        }
        let name = sanitize_name(name)?;
        let upload_id = new_upload_id();
        let stream_id = self.next_stream.fetch_add(1, Ordering::SeqCst);
        let path = self.root.join("pending").join(format!("{upload_id}.part"));
        std::fs::write(&path, [])?;
        let session = UploadSession::new(
            upload_id.clone(),
            stream_id,
            name,
            size,
            mime_hint,
            path,
            owner,
        );
        let mut book = self.book();
        book.by_stream.insert(stream_id, upload_id.clone());
        book.by_id.insert(upload_id.clone(), session);
        Ok(UploadTicket {
            upload_id,
            stream_id,
            chunk_size: UPLOAD_CHUNK_SIZE,
        })
    }

    /// Appends one binary chunk and returns the new resumable state.
    pub fn append_chunk(
        &self,
        owner: UploadOwner,
        stream_id: u32,
        seq: u64,
        payload: &[u8],
    ) -> Result<UploadState, BlobError> {
        if payload.len() > UPLOAD_CHUNK_SIZE as usize {
            return Err(BlobError::ChunkTooLarge {
                got: payload.len(),
                max: UPLOAD_CHUNK_SIZE as usize,
            });
        }
        let mut book = self.book();
        let upload_id = book
            .by_stream
            .get(&stream_id)
            .cloned()
            .ok_or_else(|| BlobError::UploadNotFound(format!("stream {stream_id}")))?;
        let session = book
            .by_id
            .get_mut(&upload_id)
            .ok_or_else(|| BlobError::UploadNotFound(upload_id.clone()))?;
        check_owner(session, owner)?;
        session
            .append(seq, payload, self.max_bytes)
            .map_err(|error| match error {
                AppendError::Gap { expected, got } => BlobError::ChunkSequence { expected, got },
                AppendError::TooLargeChunk { got, max } => BlobError::ChunkTooLarge { got, max },
                AppendError::TooLong { total, declared } => BlobError::TooLarge {
                    size: total,
                    max: declared,
                },
                AppendError::Io(error) => BlobError::Io(error),
            })?;
        Ok(session.state())
    }

    /// Verifies and finishes an upload, deduping against an existing blob.
    pub async fn commit_upload(
        &self,
        owner: UploadOwner,
        upload_id: &str,
        declared_blake3: &str,
    ) -> Result<BlobInfo, BlobError> {
        if !is_blake3_hex(declared_blake3) {
            return Err(BlobError::InvalidHash(declared_blake3.to_owned()));
        }
        let (snapshot, already_committed) = {
            let book = self.book();
            let session = book
                .by_id
                .get(upload_id)
                .ok_or_else(|| BlobError::UploadNotFound(upload_id.to_owned()))?;
            check_owner(session, owner)?;
            match &session.committed {
                Some(committed) => (None, Some(committed.clone())),
                None => (
                    Some(Snapshot {
                        path: session.path.clone(),
                        name: session.name.clone(),
                        size: session.size,
                        received: session.received,
                        mime_hint: session.mime_hint.clone(),
                    }),
                    None,
                ),
            }
        };
        if let Some(committed) = already_committed {
            return self.info(&committed).await;
        }
        let snapshot = snapshot.expect("an uncommitted upload has a snapshot");

        if snapshot.received != snapshot.size {
            return Err(BlobError::Incomplete {
                received: snapshot.received,
                size: snapshot.size,
            });
        }

        let bytes = std::fs::read(&snapshot.path)?;
        let digest = blake3::hash(&bytes).to_hex().to_string();
        if digest != declared_blake3 {
            return Err(BlobError::HashMismatch);
        }
        let id = AttachmentId::new(digest);
        let mime = sniff_mime(&bytes, snapshot.mime_hint.as_deref(), &snapshot.name)?;

        let blob_path = self.blob_path(&id);
        if !blob_path.exists() {
            if let Some(parent) = blob_path.parent() {
                create_private_dir(parent)?;
            }
            std::fs::write(&blob_path, &bytes)?;
        }

        let has_thumbnail = if mime.starts_with("image/") {
            write_thumbnail(&blob_path, &bytes)
        } else {
            false
        };

        let created_at = Utc::now();
        sqlx::query(
            "INSERT INTO blobs (id, name, mime, size, created_at, refcount, has_thumbnail)
             VALUES (?, ?, ?, ?, ?, 0, ?)
             ON CONFLICT(id) DO UPDATE SET
                has_thumbnail = MAX(blobs.has_thumbnail, excluded.has_thumbnail)",
        )
        .bind(id.as_str())
        .bind(&snapshot.name)
        .bind(&mime)
        .bind(snapshot.size as i64)
        .bind(created_at.to_rfc3339())
        .bind(i64::from(has_thumbnail))
        .execute(&self.pool)
        .await?;

        // The pending file is no longer needed once the content is in place.
        let _ = std::fs::remove_file(&snapshot.path);
        {
            let mut book = self.book();
            if let Some(session) = book.by_id.get_mut(upload_id) {
                session.committed = Some(id.clone());
            }
        }

        self.info(&id).await
    }

    /// Discards an upload and its partial bytes.
    pub fn abort_upload(&self, owner: UploadOwner, upload_id: &str) -> Result<(), BlobError> {
        {
            let book = self.book();
            let session = book
                .by_id
                .get(upload_id)
                .ok_or_else(|| BlobError::UploadNotFound(upload_id.to_owned()))?;
            check_owner(session, owner)?;
        }
        self.remove_upload(upload_id)
    }

    /// Removes one upload regardless of owner (the sweeper and an authorised
    /// abort use this).
    fn remove_upload(&self, upload_id: &str) -> Result<(), BlobError> {
        let mut book = self.book();
        let session = book
            .by_id
            .remove(upload_id)
            .ok_or_else(|| BlobError::UploadNotFound(upload_id.to_owned()))?;
        book.by_stream.remove(&session.stream_id);
        let _ = std::fs::remove_file(&session.path);
        Ok(())
    }

    /// Resumable progress of one upload.
    pub fn query_upload(
        &self,
        owner: UploadOwner,
        upload_id: &str,
    ) -> Result<UploadState, BlobError> {
        let book = self.book();
        let session = book
            .by_id
            .get(upload_id)
            .ok_or_else(|| BlobError::UploadNotFound(upload_id.to_owned()))?;
        check_owner(session, owner)?;
        Ok(session.state())
    }

    /// Metadata for one committed blob.
    pub async fn info(&self, id: &AttachmentId) -> Result<BlobInfo, BlobError> {
        let row = sqlx::query(
            "SELECT id, name, mime, size, created_at, has_thumbnail FROM blobs WHERE id = ?",
        )
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| BlobError::NotFound(id.to_string()))?;
        row_to_info(&row)
    }

    /// Metadata and bytes for a `GetBlob` response.
    pub async fn download(
        &self,
        id: &AttachmentId,
        range: Option<BlobRange>,
        thumbnail: bool,
    ) -> Result<(BlobDownload, Vec<u8>), BlobError> {
        let info = self.info(id).await?;
        if thumbnail && !info.has_thumbnail {
            return Err(BlobError::NotFound(format!(
                "thumbnail for {}",
                id.as_str()
            )));
        }
        let path = if thumbnail {
            self.thumbnail_path(id)
        } else {
            self.blob_path(id)
        };
        let bytes = std::fs::read(&path)?;
        let offset = range.map(|range| range.offset).unwrap_or(0);
        if offset > bytes.len() as u64 {
            return Err(BlobError::NotFound(format!(
                "byte offset {offset} in {}",
                id.as_str()
            )));
        }
        let start = offset as usize;
        let end = match range.and_then(|range| range.length) {
            Some(length) => start.saturating_add(length as usize).min(bytes.len()),
            None => bytes.len(),
        };
        let slice = bytes[start..end].to_vec();
        let stream_id = self.next_stream.fetch_add(1, Ordering::SeqCst);
        Ok((
            BlobDownload {
                id: id.clone(),
                name: info.name,
                mime: if thumbnail {
                    "image/png".to_owned()
                } else {
                    info.mime
                },
                size: slice.len() as u64,
                offset,
                stream_id,
                thumbnail,
            },
            slice,
        ))
    }

    /// Increments a blob's refcount and records which thread uses it.
    pub async fn retain(&self, thread_id: &ThreadId, id: &AttachmentId) -> Result<(), BlobError> {
        let updated = sqlx::query("UPDATE blobs SET refcount = refcount + 1 WHERE id = ?")
            .bind(id.as_str())
            .execute(&self.pool)
            .await?;
        if updated.rows_affected() == 0 {
            return Err(BlobError::NotFound(id.to_string()));
        }
        sqlx::query(
            "INSERT INTO blob_thread_refs (blob_id, thread_id, refs) VALUES (?, ?, 1)
             ON CONFLICT(blob_id, thread_id) DO UPDATE SET refs = refs + 1",
        )
        .bind(id.as_str())
        .bind(thread_id.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Deletes unreferenced blobs older than the grace window.
    ///
    /// Returns the number of blobs removed. Pending uploads that were never
    /// committed are swept on the same schedule.
    pub async fn gc(&self) -> Result<usize, BlobError> {
        let cutoff = Utc::now()
            - chrono::Duration::from_std(self.grace).unwrap_or_else(|_| chrono::Duration::zero());
        let cutoff_text = cutoff.to_rfc3339();

        let rows = sqlx::query(
            "SELECT id FROM blobs WHERE refcount <= 0 AND created_at < ? ORDER BY created_at",
        )
        .bind(&cutoff_text)
        .fetch_all(&self.pool)
        .await?;

        let mut removed = 0;
        for row in rows {
            let id: String = row.try_get("id")?;
            let id = AttachmentId::new(id);
            let _ = std::fs::remove_file(self.blob_path(&id));
            let _ = std::fs::remove_file(self.thumbnail_path(&id));
            sqlx::query("DELETE FROM blobs WHERE id = ?")
                .bind(id.as_str())
                .execute(&self.pool)
                .await?;
            sqlx::query("DELETE FROM blob_thread_refs WHERE blob_id = ?")
                .bind(id.as_str())
                .execute(&self.pool)
                .await?;
            removed += 1;
        }

        // In-memory uploads that lost their client before committing.
        let stale: Vec<String> = {
            let book = self.book();
            book.by_id
                .iter()
                .filter(|(_, session)| session.committed.is_none() && session.created_at < cutoff)
                .map(|(id, _)| id.to_owned())
                .collect()
        };
        for upload_id in stale {
            let _ = self.remove_upload(&upload_id);
        }

        // A crash can leave a `.part` file with no session; sweep those too.
        if let Ok(entries) = std::fs::read_dir(self.root.join("pending")) {
            for entry in entries.flatten() {
                let path = entry.path();
                let stale = entry
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .map(|modified| {
                        modified
                            < std::time::SystemTime::now()
                                .checked_sub(self.grace)
                                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
                    })
                    .unwrap_or(false);
                if stale {
                    let _ = std::fs::remove_file(path);
                }
            }
        }

        Ok(removed)
    }

    fn book(&self) -> std::sync::MutexGuard<'_, UploadBook> {
        self.uploads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn blob_path(&self, id: &AttachmentId) -> PathBuf {
        let value = id.as_str();
        let (prefix, rest) = value.split_at(2);
        self.root.join(prefix).join(rest)
    }

    fn thumbnail_path(&self, id: &AttachmentId) -> PathBuf {
        let blob_path = self.blob_path(id);
        thumbnail_beside(&blob_path)
    }
}

#[async_trait]
impl AttachmentResolver for BlobStore {
    async fn resolve(
        &self,
        thread_id: &ThreadId,
        ids: &[AttachmentId],
    ) -> Result<Vec<Attachment>, BlobError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let directory = self.attachments_root.join(thread_id.as_str());
        create_private_dir(&directory)?;

        let mut resolved = Vec::with_capacity(ids.len());
        for id in ids {
            let info = self.info(id).await?;
            let source = self.blob_path(id);
            if !source.is_file() {
                return Err(BlobError::NotFound(id.to_string()));
            }
            let file_name = format!("{}-{}", &id.as_str()[..8], info.name);
            let destination = directory.join(file_name);
            if !destination.exists() {
                // A hard link keeps the per-thread copy free; filesystems that
                // do not allow links (or cross-device roots) fall back to copy.
                if std::fs::hard_link(&source, &destination).is_err() {
                    std::fs::copy(&source, &destination)?;
                }
            }
            self.retain(thread_id, id).await?;
            resolved.push(Attachment {
                id: id.clone(),
                name: info.name,
                mime: info.mime,
                size: info.size,
                local_path: destination.to_string_lossy().into_owned(),
            });
        }
        Ok(resolved)
    }
}

/// Starts the periodic garbage collector.
///
/// The task holds a weak reference so dropping the daemon stops it without a
/// separate shutdown channel.
pub fn spawn_gc(store: &Arc<BlobStore>, interval: Duration) -> tokio::task::JoinHandle<()> {
    let weak = Arc::downgrade(store);
    let interval = if interval.is_zero() {
        Duration::from_secs(3600)
    } else {
        interval
    };
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let Some(store) = weak.upgrade() else {
                break;
            };
            match store.gc().await {
                Ok(0) => {}
                Ok(removed) => tracing::info!(removed, "swept unreferenced attachment blobs"),
                Err(error) => tracing::warn!(%error, "attachment GC failed"),
            }
        }
    })
}

struct Snapshot {
    path: PathBuf,
    name: String,
    size: u64,
    received: u64,
    mime_hint: Option<String>,
}

fn row_to_info(row: &sqlx::sqlite::SqliteRow) -> Result<BlobInfo, BlobError> {
    let id: String = row.try_get("id")?;
    let created_at: String = row.try_get("created_at")?;
    let created_at = DateTime::parse_from_rfc3339(&created_at)
        .map(|value| value.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now());
    let size: i64 = row.try_get("size")?;
    let has_thumbnail: i64 = row.try_get("has_thumbnail")?;
    Ok(BlobInfo {
        id: AttachmentId::new(id),
        name: row.try_get("name")?,
        mime: row.try_get("mime")?,
        size: size.max(0) as u64,
        created_at,
        has_thumbnail: has_thumbnail != 0,
    })
}

fn new_upload_id() -> String {
    let uuid = ThreadId::new_uuid_v7();
    let suffix = uuid
        .as_str()
        .strip_prefix("th_")
        .unwrap_or_else(|| uuid.as_str());
    format!("up_{suffix}")
}

fn is_blake3_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// Sanitizes a display name without ever treating it as a path.
///
/// Separators and Windows-reserved characters are replaced, leading dots are
/// stripped (so `.`/`..` and hidden-file tricks cannot survive), control
/// characters are dropped and the result is bounded. The returned name is safe
/// to join under a directory.
pub fn sanitize_name(raw: &str) -> Result<String, BlobError> {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw).trim();
    if base.is_empty() {
        return Err(BlobError::InvalidName);
    }
    let mut cleaned = String::with_capacity(base.len().min(MAX_NAME_LEN));
    for character in base.chars() {
        if character.is_control() {
            continue;
        }
        let character = match character {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            other => other,
        };
        cleaned.push(character);
    }
    let cleaned = cleaned.trim().trim_start_matches('.').trim();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return Err(BlobError::InvalidName);
    }
    Ok(truncate_name(cleaned, MAX_NAME_LEN))
}

fn truncate_name(name: &str, max: usize) -> String {
    if name.len() <= max {
        return name.to_owned();
    }
    let extension = Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| format!(".{extension}"))
        .filter(|extension| extension.len() <= 16)
        .unwrap_or_default();
    let keep = max.saturating_sub(extension.len()).max(1);
    let stem: String = name.chars().take(keep).collect();
    let stem = stem.trim_end_matches(['.', ' ']);
    format!("{stem}{extension}")
}

fn sniff_mime(bytes: &[u8], hint: Option<&str>, name: &str) -> Result<String, BlobError> {
    let hinted = hint.unwrap_or_default().trim().to_ascii_lowercase();
    let by_extension = image_mime_from_name(name);
    let declared_image = hinted.starts_with("image/") || by_extension.is_some();
    let detected = infer::get(bytes).map(|kind| kind.mime_type().to_owned());

    if declared_image {
        return match detected {
            Some(mime) if mime.starts_with("image/") => Ok(mime),
            _ => Err(BlobError::InvalidImage),
        };
    }
    Ok(detected
        .filter(|mime| !mime.is_empty())
        .or_else(|| (!hinted.is_empty()).then_some(hinted))
        .unwrap_or_else(|| "application/octet-stream".to_owned()))
}

fn image_mime_from_name(name: &str) -> Option<&'static str> {
    let extension = Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())?
        .to_ascii_lowercase();
    match extension.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

fn write_thumbnail(blob_path: &Path, bytes: &[u8]) -> bool {
    let Ok(image) = image::load_from_memory(bytes) else {
        return false;
    };
    let thumbnail = image.thumbnail(256, 256);
    let path = thumbnail_beside(blob_path);
    thumbnail
        .save_with_format(&path, image::ImageFormat::Png)
        .is_ok()
}

fn thumbnail_beside(blob_path: &Path) -> PathBuf {
    blob_path.with_file_name(format!(
        "{}.thumb.png",
        blob_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("blob")
    ))
}

fn create_private_dir(path: &Path) -> Result<(), BlobError> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_cannot_escape_their_directory() {
        assert_eq!(sanitize_name("../../etc/passwd").unwrap(), "passwd");
        assert_eq!(
            sanitize_name("C:\\Users\\a\\report.csv").unwrap(),
            "report.csv"
        );
        assert_eq!(
            sanitize_name("..").unwrap_err().to_string(),
            "the attachment name is not usable"
        );
        assert_eq!(
            sanitize_name("  ").unwrap_err().to_string(),
            "the attachment name is not usable"
        );
        assert_eq!(sanitize_name("a\u{0}b.txt").unwrap(), "ab.txt");
    }

    #[test]
    fn long_names_keep_their_extension() {
        let name = format!("{}.png", "x".repeat(300));
        let sanitized = sanitize_name(&name).unwrap();
        assert!(sanitized.len() <= MAX_NAME_LEN);
        assert!(sanitized.ends_with(".png"));
    }

    #[test]
    fn image_claims_are_checked_against_the_bytes() {
        let png = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        assert_eq!(
            sniff_mime(&png, Some("image/png"), "x.png").unwrap(),
            "image/png"
        );
        assert!(matches!(
            sniff_mime(b"not an image", Some("image/png"), "x.png"),
            Err(BlobError::InvalidImage)
        ));
        assert!(matches!(
            sniff_mime(b"not an image", None, "x.png"),
            Err(BlobError::InvalidImage)
        ));
    }

    #[test]
    fn plain_text_falls_back_to_the_hint() {
        assert_eq!(
            sniff_mime(b"hello", Some("text/plain"), "notes.txt").unwrap(),
            "text/plain"
        );
        assert_eq!(
            sniff_mime(b"hello", None, "notes").unwrap(),
            "application/octet-stream"
        );
    }

    #[tokio::test]
    async fn blob_paths_are_sharded_by_the_first_two_hex_characters() {
        let store = BlobStore {
            root: PathBuf::from("/data/blobs"),
            attachments_root: PathBuf::from("/data/attachments"),
            max_bytes: 1,
            grace: Duration::ZERO,
            pool: SqlitePoolOptions::new()
                .connect_lazy("sqlite::memory:")
                .unwrap(),
            uploads: Mutex::new(UploadBook::default()),
            next_stream: AtomicU32::new(1),
        };
        let id = AttachmentId::new("ab".to_owned() + &"c".repeat(62));
        assert_eq!(
            store.blob_path(&id),
            PathBuf::from("/data/blobs/ab/").join("c".repeat(62))
        );
    }
}
