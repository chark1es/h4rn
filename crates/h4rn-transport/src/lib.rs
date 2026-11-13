//! Multipath, path-agnostic, end-to-end encrypted session transport.
//!
//! One logical session survives a switch between a LAN WebSocket, a Tailscale
//! address and the h4rn-connect relay (ADR-009). Two layers make that work:
//!
//! - [`noise`] runs a fresh `Noise_IK_25519_ChaChaPoly_BLAKE2s` handshake on
//!   every path. Noise state is never reused across paths; a resume token
//!   inside the encrypted payload binds a new path to the existing session
//!   (ADR-010).
//! - [`session`] owns the per-direction sequence numbers, cumulative acks,
//!   bounded retransmit buffer and fragment reassembly. Because the session,
//!   not the socket, assigns sequence numbers, a path switch cannot drop or
//!   duplicate an application frame.
//!
//! The core is a synchronous, clock-injected state machine so it compiles to
//! `wasm32-unknown-unknown` and can be tested without real sockets. Native
//! async plumbing lives behind the `native` feature; the wasm wrapper is a
//! separate crate (`h4rn-transport-wasm`).

#![deny(missing_docs)]

pub mod engine;
pub mod keys;
pub mod noise;
pub mod pairing;
pub mod server_session;
pub mod session;
pub mod wire;

#[cfg(feature = "native")]
pub mod client;
#[cfg(feature = "native")]
pub mod relay;
#[cfg(feature = "native")]
pub mod server;

pub use engine::{EngineConfig, PathId, PathSpec, TransportEngine};
#[cfg(feature = "keychain")]
pub use keys::KeychainOrFile;
pub use keys::{
    DeviceIdentity, DevicePublicIdentity, EncryptedFileKeyStore, FileKeyStore, KeyStore,
    KeyStoreError, MemoryKeyStore, read_allowlist, read_secret_file, remove_secret_file,
    write_secret_file,
};
pub use noise::{Handshake, SecureChannel};
pub use pairing::PairingPayload;
pub use session::{AppFrame, ResumeTicket, SessionLayer};
pub use wire::{Packet, PacketKind, Record, RecordKind};

/// Version of the transport wire format.
pub const TRANSPORT_VERSION: u8 = 1;

/// Maximum application frame accepted before fragmentation, in bytes.
///
/// The application protocol's binary frames are chunked at 256 KiB, so this is
/// large enough for one blob chunk while still bounding memory per session.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 512 * 1024;

/// Maximum plaintext a single Noise transport message can carry.
///
/// Noise caps a message at 65535 bytes including the 16-byte AEAD tag.
pub const MAX_RECORD_PLAINTEXT: usize = 65_519;

/// Bytes of application data per fragment. Leaves room for the record header
/// inside [`MAX_RECORD_PLAINTEXT`].
pub const MAX_FRAGMENT_BYTES: usize = MAX_RECORD_PLAINTEXT - 40;

/// Size bucket ciphertext is padded to, so the relay learns only a coarse
/// message size (ADR-010).
pub const PADDING_BUCKET: usize = 256;
