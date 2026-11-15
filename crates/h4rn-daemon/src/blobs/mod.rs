//! Content-addressed attachment blobs (ADR-008).
//!
//! The store owns three things: the on-disk blob bytes, the SQLite metadata
//! and refcounts, and the in-memory state of uploads that have not been
//! committed yet. `h4rn-daemon` wires it into the WebSocket server (`BeginUpload`
//! and friends) and into the thread manager through [`AttachmentResolver`],
//! which materialises a per-thread copy for harness delivery.

mod store;
pub mod upload;

pub use store::{
    AttachmentResolver, BlobError, BlobStore, DOWNLOAD_CHUNK_SIZE, sanitize_name, spawn_gc,
};
pub use upload::{UPLOAD_CHUNK_SIZE, UploadOwner};
