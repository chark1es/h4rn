//! One in-flight chunked upload (ADR-008).
//!
//! An upload is deliberately ephemeral: the daemon keeps the partial bytes in
//! `blobs/pending/<upload_id>.part` and the resumable position in memory. A
//! reconnect can ask `QueryUpload` for the bytes already accepted and continue
//! from there; a daemon restart simply drops the upload, which is safe because
//! nothing has been committed yet.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use h4rn_protocol::{AttachmentId, UploadState};

/// Chunk size advertised to clients and enforced on every `blob_chunk` frame.
///
/// One MiB keeps a frame well below the WebSocket write timeout while still
/// moving a 50 MiB attachment in ~50 frames.
pub const UPLOAD_CHUNK_SIZE: u32 = 1024 * 1024;

/// One upload's resumable state.
#[derive(Debug)]
pub struct UploadSession {
    /// Opaque id returned by `BeginUpload`.
    pub upload_id: String,
    /// Binary stream id every chunk frame must carry.
    pub stream_id: u32,
    /// Sanitized original name.
    pub name: String,
    /// Declared total size.
    pub size: u64,
    /// Client's media-type hint; the bytes win at commit time.
    pub mime_hint: Option<String>,
    /// Partial file on disk.
    pub path: PathBuf,
    /// Bytes accepted so far.
    pub received: u64,
    /// Next chunk sequence number expected.
    pub next_seq: u64,
    /// Creation time, used by the pending-upload sweeper.
    pub created_at: DateTime<Utc>,
    /// Set once `CommitUpload` produced an address; makes a retried commit
    /// idempotent instead of hashing the same bytes twice.
    pub committed: Option<AttachmentId>,
}

impl UploadSession {
    /// Creates a session that has not accepted any bytes yet.
    pub fn new(
        upload_id: String,
        stream_id: u32,
        name: String,
        size: u64,
        mime_hint: Option<String>,
        path: PathBuf,
    ) -> Self {
        Self {
            upload_id,
            stream_id,
            name,
            size,
            mime_hint,
            path,
            received: 0,
            next_seq: 0,
            created_at: Utc::now(),
            committed: None,
        }
    }

    /// The wire view of this upload.
    pub fn state(&self) -> UploadState {
        UploadState {
            upload_id: self.upload_id.clone(),
            stream_id: self.stream_id,
            received_bytes: self.received,
            size: self.size,
            committed: self.committed.clone(),
        }
    }

    /// Appends one chunk if it is the next expected sequence number.
    ///
    /// A duplicate sequence number is treated as a retry and ignored, which is
    /// what makes a reconnect after a lost frame cheap. A gap is rejected so
    /// the client can resume from [`Self::state`] rather than silently
    /// committing a file with a hole in it.
    pub fn append(&mut self, seq: u64, payload: &[u8], max_bytes: u64) -> Result<u64, AppendError> {
        if seq < self.next_seq {
            return Ok(self.received);
        }
        if seq > self.next_seq {
            return Err(AppendError::Gap {
                expected: self.next_seq,
                got: seq,
            });
        }
        if payload.len() > UPLOAD_CHUNK_SIZE as usize {
            return Err(AppendError::TooLargeChunk {
                got: payload.len(),
                max: UPLOAD_CHUNK_SIZE as usize,
            });
        }
        let total = self.received.saturating_add(payload.len() as u64);
        if total > self.size {
            return Err(AppendError::TooLong {
                total,
                declared: self.size,
            });
        }
        if total > max_bytes {
            return Err(AppendError::TooLong {
                total,
                declared: max_bytes,
            });
        }

        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(AppendError::Io)?;
        file.write_all(payload).map_err(AppendError::Io)?;
        self.received = total;
        self.next_seq = self.next_seq.saturating_add(1);
        Ok(self.received)
    }
}

/// Why a chunk was rejected.
#[derive(Debug, thiserror::Error)]
pub enum AppendError {
    /// Frames arrived out of order.
    #[error("chunk {got} arrived before the daemon expected it (expected {expected})")]
    Gap {
        /// Expected sequence.
        expected: u64,
        /// Sequence received.
        got: u64,
    },
    /// One frame exceeded the advertised chunk size.
    #[error("chunk is {got} bytes; the daemon accepts at most {max} per frame")]
    TooLargeChunk {
        /// Payload length received.
        got: usize,
        /// Advertised maximum.
        max: usize,
    },
    /// The upload would exceed its declared (or configured) size.
    #[error("upload would reach {total} bytes, beyond the limit of {declared}")]
    TooLong {
        /// Resulting length.
        total: u64,
        /// Declared size or configured maximum.
        declared: u64,
    },
    /// The partial file could not be written.
    #[error("failed to write partial upload: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(path: PathBuf, size: u64) -> UploadSession {
        UploadSession::new("up_1".into(), 7, "a.txt".into(), size, None, path)
    }

    #[test]
    fn chunks_must_arrive_in_order_and_duplicates_are_retries() {
        let dir = tempfile::tempdir().unwrap();
        let mut upload = session(dir.path().join("a.part"), 6);
        assert_eq!(upload.append(0, b"abc", 1024).unwrap(), 3);
        assert_eq!(upload.append(0, b"abc", 1024).unwrap(), 3);
        assert!(matches!(
            upload.append(2, b"def", 1024),
            Err(AppendError::Gap {
                expected: 1,
                got: 2
            })
        ));
        assert_eq!(upload.append(1, b"def", 1024).unwrap(), 6);
        assert_eq!(upload.next_seq, 2);
    }

    #[test]
    fn declared_size_is_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let mut upload = session(dir.path().join("a.part"), 2);
        assert!(matches!(
            upload.append(0, b"abc", 1024),
            Err(AppendError::TooLong { .. })
        ));
    }

    #[test]
    fn state_reports_progress_and_commit_idempotence() {
        let dir = tempfile::tempdir().unwrap();
        let mut upload = session(dir.path().join("a.part"), 4);
        upload.append(0, b"abcd", 1024).unwrap();
        let state = upload.state();
        assert_eq!(state.received_bytes, 4);
        assert_eq!(state.committed, None);
        upload.committed = Some(AttachmentId::from_content(b"abcd"));
        assert!(upload.state().committed.is_some());
    }
}
