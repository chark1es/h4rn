//! Device identity and key storage (ADR-016).
//!
//! A device owns two long-lived keys, generated together:
//!
//! - an **X25519 static key** used as the Noise static key, and
//! - an **Ed25519 signing key** used for h4rn-connect proofs.
//!
//! The daemon stores an allowlist of paired device X25519 public keys and
//! rejects a handshake from any key that is not on it. Secrets live in the OS
//! keychain on desktop via the `keychain` feature, in a `0600` file as a
//! documented fallback, and in an injected store on wasm (the JS side persists
//! the encoded identity and hands it back on boot).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use thiserror::Error;
use x25519_dalek::{PublicKey, StaticSecret};

/// Length of every key and token in the transport.
pub const KEY_LEN: usize = 32;

/// Prefix of the encoded identity string.
const ENCODED_PREFIX: &str = "h4rn-id-1";

/// Errors from key generation, storage and parsing.
#[derive(Debug, Error)]
pub enum KeyStoreError {
    /// The OS keychain or file backend failed.
    #[error("key store backend failed: {0}")]
    Backend(String),
    /// The stored value was not a valid identity.
    #[error("stored identity is malformed: {0}")]
    Malformed(String),
    /// The platform has no random source available.
    #[error("no operating-system randomness available")]
    NoRandomness,
}

/// The public half of a device identity, safe to share and log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DevicePublicIdentity {
    /// X25519 Noise static public key.
    pub noise_public: [u8; KEY_LEN],
    /// Ed25519 signing public key.
    pub signing_public: [u8; KEY_LEN],
}

impl DevicePublicIdentity {
    /// Short human-readable fingerprint, for the pair screen and diagnostics.
    pub fn fingerprint(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&self.noise_public);
        hasher.update(&self.signing_public);
        let digest = hasher.finalize();
        let hex = digest.to_hex();
        hex.as_str()[..16].to_owned()
    }

    /// Hex encoding of both public keys.
    pub fn to_hex(&self) -> String {
        format!(
            "{}:{}",
            hex::encode(self.noise_public),
            hex::encode(self.signing_public)
        )
    }

    /// Parses the output of [`Self::to_hex`].
    pub fn from_hex(value: &str) -> Result<Self, KeyStoreError> {
        let mut parts = value.split(':');
        let noise = parts
            .next()
            .ok_or_else(|| KeyStoreError::Malformed("missing noise key".into()))?;
        let signing = parts
            .next()
            .ok_or_else(|| KeyStoreError::Malformed("missing signing key".into()))?;
        Ok(Self {
            noise_public: parse_key(noise)?,
            signing_public: parse_key(signing)?,
        })
    }
}

/// A device's long-lived identity.
#[derive(Clone)]
pub struct DeviceIdentity {
    noise_secret: [u8; KEY_LEN],
    noise_public: [u8; KEY_LEN],
    signing_secret: [u8; KEY_LEN],
    signing_public: [u8; KEY_LEN],
}

impl std::fmt::Debug for DeviceIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeviceIdentity")
            .field("fingerprint", &self.public().fingerprint())
            .finish_non_exhaustive()
    }
}

impl DeviceIdentity {
    /// Generates a fresh identity from the operating system's randomness.
    pub fn generate() -> Result<Self, KeyStoreError> {
        let mut secret = [0u8; KEY_LEN];
        let mut signing = [0u8; KEY_LEN];
        getrandom::fill(&mut secret).map_err(|_| KeyStoreError::NoRandomness)?;
        getrandom::fill(&mut signing).map_err(|_| KeyStoreError::NoRandomness)?;
        Self::from_secret_bytes(secret, signing)
    }

    /// Rebuilds an identity from its two 32-byte secrets.
    pub fn from_secret_bytes(
        noise_secret: [u8; KEY_LEN],
        signing_secret: [u8; KEY_LEN],
    ) -> Result<Self, KeyStoreError> {
        let secret = StaticSecret::from(noise_secret);
        let noise_public = PublicKey::from(&secret).to_bytes();
        let signing = SigningKey::from_bytes(&signing_secret);
        let signing_public = signing.verifying_key().to_bytes();
        Ok(Self {
            noise_secret,
            noise_public,
            signing_secret,
            signing_public,
        })
    }

    /// X25519 static secret used by Noise.
    pub fn noise_secret(&self) -> &[u8; KEY_LEN] {
        &self.noise_secret
    }

    /// Public half of this identity.
    pub fn public(&self) -> DevicePublicIdentity {
        DevicePublicIdentity {
            noise_public: self.noise_public,
            signing_public: self.signing_public,
        }
    }

    /// Signs a connect proof with the Ed25519 key.
    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        let signing = SigningKey::from_bytes(&self.signing_secret);
        signing.sign(message).to_bytes()
    }

    /// Verifies a signature from `public` over `message`.
    pub fn verify(
        public: &DevicePublicIdentity,
        message: &[u8],
        signature: &[u8; 64],
    ) -> Result<(), KeyStoreError> {
        let verifying = VerifyingKey::from_bytes(&public.signing_public)
            .map_err(|error| KeyStoreError::Malformed(error.to_string()))?;
        let signature = Signature::from_bytes(signature);
        verifying
            .verify_strict(message, &signature)
            .map_err(|error| KeyStoreError::Malformed(error.to_string()))
    }

    /// Serialises the secret identity for a keystore.
    ///
    /// The output is sensitive: it contains both private keys. Only a
    /// [`KeyStore`] may persist it.
    pub fn encode(&self) -> String {
        format!(
            "{ENCODED_PREFIX}:{}:{}",
            hex::encode(self.noise_secret),
            hex::encode(self.signing_secret)
        )
    }

    /// Parses the output of [`Self::encode`].
    pub fn decode(value: &str) -> Result<Self, KeyStoreError> {
        let mut parts = value.trim().split(':');
        if parts.next() != Some(ENCODED_PREFIX) {
            return Err(KeyStoreError::Malformed("unknown identity prefix".into()));
        }
        let noise = parts
            .next()
            .ok_or_else(|| KeyStoreError::Malformed("missing noise secret".into()))?;
        let signing = parts
            .next()
            .ok_or_else(|| KeyStoreError::Malformed("missing signing secret".into()))?;
        if parts.next().is_some() {
            return Err(KeyStoreError::Malformed("trailing identity data".into()));
        }
        Self::from_secret_bytes(parse_key(noise)?, parse_key(signing)?)
    }
}

fn parse_key(value: &str) -> Result<[u8; KEY_LEN], KeyStoreError> {
    let bytes = hex::decode(value).map_err(|error| KeyStoreError::Malformed(error.to_string()))?;
    if bytes.len() != KEY_LEN {
        return Err(KeyStoreError::Malformed(format!(
            "key is {} bytes, expected {KEY_LEN}",
            bytes.len()
        )));
    }
    let mut key = [0u8; KEY_LEN];
    key.copy_from_slice(&bytes);
    Ok(key)
}

/// Where a device identity is persisted.
///
/// Implementations must be cheap to clone or share; the transport calls `load`
/// once per connection and `store` only when the identity is first created.
pub trait KeyStore: Send + Sync {
    /// Loads the identity, returning `None` when this device has never paired.
    fn load(&self) -> Result<Option<DeviceIdentity>, KeyStoreError>;
    /// Persists a newly generated identity.
    fn store(&self, identity: &DeviceIdentity) -> Result<(), KeyStoreError>;
    /// Removes the identity, for an explicit unpair.
    fn clear(&self) -> Result<(), KeyStoreError>;
}

/// An in-memory store, used by tests and by the wasm client.
///
/// On wasm the JS shell owns persistence: it reads [`DeviceIdentity::encode`]
/// from the session and writes it to `localStorage` (or the Tauri keychain),
/// then constructs a new `MemoryKeyStore` from it on boot.
#[derive(Default)]
pub struct MemoryKeyStore {
    identity: Mutex<Option<DeviceIdentity>>,
}

impl MemoryKeyStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a store already holding `identity`.
    pub fn with_identity(identity: DeviceIdentity) -> Self {
        Self {
            identity: Mutex::new(Some(identity)),
        }
    }
}

impl KeyStore for MemoryKeyStore {
    fn load(&self) -> Result<Option<DeviceIdentity>, KeyStoreError> {
        Ok(self
            .identity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone())
    }

    fn store(&self, identity: &DeviceIdentity) -> Result<(), KeyStoreError> {
        *self
            .identity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(identity.clone());
        Ok(())
    }

    fn clear(&self) -> Result<(), KeyStoreError> {
        *self
            .identity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        Ok(())
    }
}

/// A `0600` file store used when the OS keychain is unavailable.
#[derive(Clone, Debug)]
pub struct FileKeyStore {
    path: PathBuf,
}

impl FileKeyStore {
    /// Creates a store rooted at `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Path of the identity file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl KeyStore for FileKeyStore {
    fn load(&self) -> Result<Option<DeviceIdentity>, KeyStoreError> {
        match std::fs::read_to_string(&self.path) {
            Ok(value) => DeviceIdentity::decode(&value).map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(KeyStoreError::Backend(error.to_string())),
        }
    }

    fn store(&self, identity: &DeviceIdentity) -> Result<(), KeyStoreError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| KeyStoreError::Backend(error.to_string()))?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&self.path)
            .map_err(|error| KeyStoreError::Backend(error.to_string()))?;
        use std::io::Write;
        file.write_all(identity.encode().as_bytes())
            .map_err(|error| KeyStoreError::Backend(error.to_string()))?;
        file.sync_all()
            .map_err(|error| KeyStoreError::Backend(error.to_string()))?;
        Ok(())
    }

    fn clear(&self) -> Result<(), KeyStoreError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(KeyStoreError::Backend(error.to_string())),
        }
    }
}

/// Prefix of an encrypted secret file.
const ENCRYPTED_PREFIX: &str = "h4rn-encv1";

/// Associated data binding ciphertext to the h4rn secret-file format.
const SECRET_AAD: &[u8] = b"h4rn-secret-v1";

/// A `0600` file store encrypted with ChaCha20-Poly1305.
///
/// This is the fallback for headless Linux and WSL, where no OS credential
/// store is available. The encryption key lives in a sibling `<path>.key`
/// file, also `0600`. That defeats accidental disclosure — a file copied into a
/// backup, sync folder or terminal scrollback is useless on its own — but it is
/// **not** a defence against an attacker who can already read the user's data
/// directory: they can read both files. The OS keychain remains the preferred
/// backend; [`KeychainOrFile`] uses it whenever
/// [`keyring::Entry::store_status`] reports a working credential store.
#[derive(Clone, Debug)]
pub struct EncryptedFileKeyStore {
    path: PathBuf,
}

impl EncryptedFileKeyStore {
    /// Creates a store rooted at `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Path of the identity file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl KeyStore for EncryptedFileKeyStore {
    fn load(&self) -> Result<Option<DeviceIdentity>, KeyStoreError> {
        match read_secret_file(&self.path)? {
            Some(bytes) => {
                let value = String::from_utf8(bytes)
                    .map_err(|_| KeyStoreError::Malformed("identity is not UTF-8".into()))?;
                DeviceIdentity::decode(&value).map(Some)
            }
            None => Ok(None),
        }
    }

    fn store(&self, identity: &DeviceIdentity) -> Result<(), KeyStoreError> {
        write_secret_file(&self.path, identity.encode().as_bytes())
    }

    fn clear(&self) -> Result<(), KeyStoreError> {
        remove_secret_file(&self.path)
    }
}

/// Prefers the OS keychain and falls back to an encrypted file.
///
/// The daemon runs on desktops (keychain available) and on headless Linux or
/// WSL (usually no secret service). Selecting once at construction keeps
/// every later read/write on the same backend, so an identity cannot be split
/// across two stores mid-flight.
#[cfg(feature = "keychain")]
#[derive(Clone, Debug)]
pub struct KeychainOrFile {
    keyring: Option<KeyringStore>,
    file: EncryptedFileKeyStore,
}

#[cfg(feature = "keychain")]
impl KeychainOrFile {
    /// Opens the store, probing the OS keychain for `service`/`account`.
    ///
    /// `file_path` is used only when no credential store is available.
    pub fn new(
        service: impl Into<String>,
        account: impl Into<String>,
        file_path: impl Into<PathBuf>,
    ) -> Self {
        let keyring = keyring::Entry::store_status()
            .is_ok()
            .then(|| KeyringStore::new(service, account));
        Self {
            keyring,
            file: EncryptedFileKeyStore::new(file_path),
        }
    }

    /// Which backend was selected, for diagnostics.
    pub fn backend(&self) -> &'static str {
        if self.keyring.is_some() {
            "os-keychain"
        } else {
            "encrypted-file"
        }
    }

    /// Path of the encrypted fallback.
    pub fn file_path(&self) -> &Path {
        self.file.path()
    }
}

#[cfg(feature = "keychain")]
impl KeyStore for KeychainOrFile {
    fn load(&self) -> Result<Option<DeviceIdentity>, KeyStoreError> {
        match &self.keyring {
            Some(store) => match store.load() {
                Ok(identity) => Ok(identity),
                // A broken keychain should not brick a headless host: fall back
                // to the encrypted file, which may already hold the identity.
                Err(_) => self.file.load(),
            },
            None => self.file.load(),
        }
    }

    fn store(&self, identity: &DeviceIdentity) -> Result<(), KeyStoreError> {
        match &self.keyring {
            Some(store) => store.store(identity),
            None => self.file.store(identity),
        }
    }

    fn clear(&self) -> Result<(), KeyStoreError> {
        match &self.keyring {
            Some(store) => {
                let result = store.clear();
                let _ = self.file.clear();
                result
            }
            None => self.file.clear(),
        }
    }
}

/// Writes `plaintext` to `path`, encrypted under a sibling key file.
///
/// Used for the transport identity and by the daemon for its connect session
/// (refresh token). The file is `0600` on Unix and written atomically enough
/// for a single-user daemon: truncate, write, `fsync`.
pub fn write_secret_file(path: &Path, plaintext: &[u8]) -> Result<(), KeyStoreError> {
    let key = load_or_create_file_key(path)?;
    let cipher = ChaCha20Poly1305::new((&key).into());
    let mut nonce_bytes = [0u8; 12];
    getrandom::fill(&mut nonce_bytes).map_err(|_| KeyStoreError::NoRandomness)?;
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce_bytes),
            Payload {
                msg: plaintext,
                aad: SECRET_AAD,
            },
        )
        .map_err(|_| KeyStoreError::Backend("encrypting the secret failed".into()))?;
    let encoded = format!(
        "{ENCRYPTED_PREFIX}:{}:{}",
        hex::encode(nonce_bytes),
        hex::encode(ciphertext)
    );
    write_private_file(path, encoded.as_bytes())
}

/// Reads and decrypts a file written by [`write_secret_file`].
///
/// Returns `Ok(None)` when the file does not exist.
pub fn read_secret_file(path: &Path) -> Result<Option<Vec<u8>>, KeyStoreError> {
    let encoded = match std::fs::read_to_string(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(KeyStoreError::Backend(error.to_string())),
    };
    let mut parts = encoded.trim().split(':');
    if parts.next() != Some(ENCRYPTED_PREFIX) {
        return Err(KeyStoreError::Malformed(
            "unknown secret-file prefix".into(),
        ));
    }
    let nonce = parts
        .next()
        .ok_or_else(|| KeyStoreError::Malformed("missing secret nonce".into()))?;
    let ciphertext = parts
        .next()
        .ok_or_else(|| KeyStoreError::Malformed("missing secret ciphertext".into()))?;
    if parts.next().is_some() {
        return Err(KeyStoreError::Malformed("trailing secret-file data".into()));
    }
    let nonce = hex::decode(nonce).map_err(|error| KeyStoreError::Malformed(error.to_string()))?;
    if nonce.len() != 12 {
        return Err(KeyStoreError::Malformed(
            "secret nonce is not 12 bytes".into(),
        ));
    }
    let ciphertext =
        hex::decode(ciphertext).map_err(|error| KeyStoreError::Malformed(error.to_string()))?;
    let key = load_or_create_file_key(path)?;
    let cipher = ChaCha20Poly1305::new((&key).into());
    cipher
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &ciphertext,
                aad: SECRET_AAD,
            },
        )
        .map(Some)
        .map_err(|_| KeyStoreError::Malformed("secret file failed to decrypt".into()))
}

/// Reads a paired-device allowlist written by the daemon.
///
/// Each line is one hex-encoded X25519 public key. A missing file is an empty
/// allowlist, and malformed lines are skipped so a corrupt entry cannot lock
/// every already-paired device out.
pub fn read_allowlist(path: &Path) -> Result<Vec<[u8; KEY_LEN]>, KeyStoreError> {
    let Some(bytes) = read_secret_file(path)? else {
        return Ok(Vec::new());
    };
    let text = String::from_utf8(bytes)
        .map_err(|_| KeyStoreError::Malformed("allowlist is not UTF-8".into()))?;
    let mut keys = Vec::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        match hex::decode(line) {
            Ok(bytes) if bytes.len() == KEY_LEN => {
                let mut key = [0u8; KEY_LEN];
                key.copy_from_slice(&bytes);
                keys.push(key);
            }
            _ => tracing::warn!("skipping a malformed allowlist entry"),
        }
    }
    Ok(keys)
}

/// Removes a secret file and its key file.
pub fn remove_secret_file(path: &Path) -> Result<(), KeyStoreError> {
    for target in [path.to_path_buf(), key_path(path)] {
        match std::fs::remove_file(&target) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(KeyStoreError::Backend(error.to_string())),
        }
    }
    Ok(())
}

fn key_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(".key");
    PathBuf::from(value)
}

fn load_or_create_file_key(path: &Path) -> Result<[u8; KEY_LEN], KeyStoreError> {
    let key_path = key_path(path);
    match std::fs::read_to_string(&key_path) {
        Ok(value) => parse_key(value.trim()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut key = [0u8; KEY_LEN];
            getrandom::fill(&mut key).map_err(|_| KeyStoreError::NoRandomness)?;
            write_private_file(&key_path, hex::encode(key).as_bytes())?;
            Ok(key)
        }
        Err(error) => Err(KeyStoreError::Backend(error.to_string())),
    }
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), KeyStoreError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| KeyStoreError::Backend(error.to_string()))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| KeyStoreError::Backend(error.to_string()))?;
    file.write_all(bytes)
        .map_err(|error| KeyStoreError::Backend(error.to_string()))?;
    file.sync_all()
        .map_err(|error| KeyStoreError::Backend(error.to_string()))?;
    Ok(())
}

/// OS keychain storage (ADR-016).
///
/// Enabled by the `keychain` feature because the Linux backend links a secret
/// service client. The daemon and desktop shell prefer this store and fall back
/// to [`EncryptedFileKeyStore`] when [`keyring::Entry::store_status`] reports
/// that no credential store is available.
#[cfg(feature = "keychain")]
#[derive(Clone, Debug)]
pub struct KeyringStore {
    service: String,
    account: String,
}

#[cfg(feature = "keychain")]
impl KeyringStore {
    /// Creates a store for `service`/`account`.
    pub fn new(service: impl Into<String>, account: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            account: account.into(),
        }
    }

    fn entry(&self) -> Result<keyring::Entry, KeyStoreError> {
        keyring::Entry::new(&self.service, &self.account)
            .map_err(|error| KeyStoreError::Backend(error.to_string()))
    }
}

#[cfg(feature = "keychain")]
impl KeyStore for KeyringStore {
    fn load(&self) -> Result<Option<DeviceIdentity>, KeyStoreError> {
        match self.entry()?.get_password() {
            Ok(value) => DeviceIdentity::decode(&value).map(Some),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(KeyStoreError::Backend(error.to_string())),
        }
    }

    fn store(&self, identity: &DeviceIdentity) -> Result<(), KeyStoreError> {
        self.entry()?
            .set_password(&identity.encode())
            .map_err(|error| KeyStoreError::Backend(error.to_string()))
    }

    fn clear(&self) -> Result<(), KeyStoreError> {
        match self.entry()?.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(KeyStoreError::Backend(error.to_string())),
        }
    }
}

/// Loads the identity from `store`, generating and persisting one on first use.
pub fn load_or_create(store: &dyn KeyStore) -> Result<DeviceIdentity, KeyStoreError> {
    if let Some(identity) = store.load()? {
        return Ok(identity);
    }
    let identity = DeviceIdentity::generate()?;
    store.store(&identity)?;
    Ok(identity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_round_trips_through_encoding() {
        let identity = DeviceIdentity::generate().unwrap();
        let decoded = DeviceIdentity::decode(&identity.encode()).unwrap();
        assert_eq!(decoded.public(), identity.public());
    }

    #[test]
    fn public_hex_round_trips() {
        let identity = DeviceIdentity::generate().unwrap();
        let public = identity.public();
        assert_eq!(
            DevicePublicIdentity::from_hex(&public.to_hex()).unwrap(),
            public
        );
    }

    #[test]
    fn signatures_verify_and_reject_tampering() {
        let identity = DeviceIdentity::generate().unwrap();
        let message = b"pair me";
        let signature = identity.sign(message);
        assert!(DeviceIdentity::verify(&identity.public(), message, &signature).is_ok());
        assert!(DeviceIdentity::verify(&identity.public(), b"other", &signature).is_err());
        let other = DeviceIdentity::generate().unwrap();
        assert!(DeviceIdentity::verify(&other.public(), message, &signature).is_err());
    }

    #[test]
    fn memory_store_load_or_create_is_stable() {
        let store = MemoryKeyStore::new();
        let first = load_or_create(&store).unwrap();
        let second = load_or_create(&store).unwrap();
        assert_eq!(first.public(), second.public());
        store.clear().unwrap();
        assert!(store.load().unwrap().is_none());
    }

    #[test]
    fn file_store_round_trips_and_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileKeyStore::new(dir.path().join("identity"));
        let identity = load_or_create(&store).unwrap();
        assert_eq!(store.load().unwrap().unwrap().public(), identity.public());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        store.clear().unwrap();
        assert!(store.load().unwrap().is_none());
    }
}
