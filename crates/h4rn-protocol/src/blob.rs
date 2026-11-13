//! Attachment blob values shared by the upload protocol and the transcript.
//!
//! Attachments are content-addressed by BLAKE3 (ADR-008). The upload itself is
//! two-phase: [`crate::ClientRequest::BeginUpload`] opens a stream, binary
//! `blob_chunk` frames carry the bytes, and [`crate::ClientRequest::CommitUpload`]
//! verifies the declared hash before the blob becomes addressable. These types
//! are the JSON half of that exchange; the bytes always travel in binary frames.
//!
//! A committed blob is described by [`Attachment`] in transcripts and by
//! [`BlobDownload`] when a client asks to stream it back. `local_path` is the
//! daemon-side materialised path used for harness delivery; clients render from
//! [`Attachment::id`] and never treat the path as a URL.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::ids::AttachmentId;

/// A committed attachment as it appears in a message and in harness delivery.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct Attachment {
    /// BLAKE3 content address.
    pub id: AttachmentId,
    /// Sanitized display name (never a path).
    pub name: String,
    /// Media type sniffed from the bytes, falling back to a hint.
    pub mime: String,
    /// Size in bytes.
    #[ts(type = "number")]
    pub size: u64,
    /// Absolute path of the materialised copy handed to the harness.
    ///
    /// This is a daemon-host path, not a URL. Clients use [`Self::id`] to fetch
    /// bytes through `GetBlob`.
    pub local_path: String,
}

/// A byte range request for [`crate::ClientRequest::GetBlob`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct BlobRange {
    /// First byte to send.
    #[ts(type = "number")]
    pub offset: u64,
    /// Bytes to send; `None` means "to the end of the blob".
    #[serde(default)]
    #[ts(type = "number | null")]
    pub length: Option<u64>,
}

/// Metadata for one committed blob.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct BlobInfo {
    /// Content address.
    pub id: AttachmentId,
    /// Sanitized original name.
    pub name: String,
    /// Sniffed media type.
    pub mime: String,
    /// Original size in bytes.
    #[ts(type = "number")]
    pub size: u64,
    /// When the blob was committed.
    pub created_at: DateTime<Utc>,
    /// Whether a 256 px thumbnail exists.
    pub has_thumbnail: bool,
}

/// The daemon's answer to `BeginUpload`: the stream to write chunks to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct UploadTicket {
    /// Opaque upload id used by commit, abort and query.
    pub upload_id: String,
    /// Binary stream id the client must put in each `blob_chunk` frame.
    #[ts(type = "number")]
    pub stream_id: u32,
    /// Largest chunk the daemon accepts in one frame.
    #[ts(type = "number")]
    pub chunk_size: u32,
}

/// Resumable progress of one upload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct UploadState {
    /// Opaque upload id.
    pub upload_id: String,
    /// Binary stream id, so a reconnecting client can resume writing.
    #[ts(type = "number")]
    pub stream_id: u32,
    /// Bytes accepted so far.
    #[ts(type = "number")]
    pub received_bytes: u64,
    /// Declared total size in bytes.
    #[ts(type = "number")]
    pub size: u64,
    /// Set once `CommitUpload` succeeded, so a retried commit is idempotent.
    pub committed: Option<AttachmentId>,
}

/// The daemon's answer to `GetBlob`: metadata plus the binary stream to read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct BlobDownload {
    /// Content address.
    pub id: AttachmentId,
    /// Sanitized original name.
    pub name: String,
    /// Media type of the bytes that will follow.
    pub mime: String,
    /// Number of bytes that will follow on this stream.
    #[ts(type = "number")]
    pub size: u64,
    /// Offset of the first byte in this stream.
    #[ts(type = "number")]
    pub offset: u64,
    /// Binary stream id the client should collect.
    #[ts(type = "number")]
    pub stream_id: u32,
    /// Whether this stream is the generated thumbnail rather than the original.
    pub thumbnail: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_range_length_defaults_to_the_end() {
        let range: BlobRange = serde_json::from_str(r#"{"offset":10}"#).unwrap();
        assert_eq!(range.length, None);
        assert_eq!(range.offset, 10);
    }

    #[test]
    fn attachment_round_trips_with_a_content_address() {
        let id = AttachmentId::from_content(b"hello");
        let attachment = Attachment {
            id: id.clone(),
            name: "notes.txt".into(),
            mime: "text/plain".into(),
            size: 5,
            local_path: "/tmp/h4rn/attachments/th_1/notes.txt".into(),
        };
        let json = serde_json::to_string(&attachment).unwrap();
        let back: Attachment = serde_json::from_str(&json).unwrap();
        assert_eq!(back, attachment);
        assert!(json.contains(id.as_str()));
    }
}
