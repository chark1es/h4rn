//! Keys, envelope encryption and token hashing (ADR-016).
//!
//! The KEK never leaves configuration. Every secret row gets its own random
//! data-encryption key, wrapped by the KEK, so a database dump is not a
//! credential dump and rotating the KEK is a single re-wrap pass.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use ed25519_dalek::{Signer as _, SigningKey, VerifyingKey};
use rand::RngCore as _;
use subtle::ConstantTimeEq as _;

use crate::error::{AppError, Result};

/// Domain separation for pairing secrets.
const PAIRING_DOMAIN: &[u8] = b"h4rn-connect/pairing-secret/v1\0";
/// Domain separation for refresh tokens.
const REFRESH_DOMAIN: &[u8] = b"h4rn-connect/refresh-token/v1\0";
/// Associated data for envelope encryption.
const ENVELOPE_AAD: &[u8] = b"h4rn-connect/envelope/v1\0";

/// Fills `len` bytes from the OS RNG.
pub fn random_bytes(len: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; len];
    rand::rng().fill_bytes(&mut bytes);
    bytes
}

/// Random bytes as URL-safe base64 without padding.
pub fn random_b64(len: usize) -> String {
    URL_SAFE_NO_PAD.encode(random_bytes(len))
}

/// URL-safe base64 without padding.
pub fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Decodes URL-safe or standard base64, with or without padding.
pub fn b64_decode(value: &str) -> Option<Vec<u8>> {
    let trimmed = value.trim();
    URL_SAFE_NO_PAD
        .decode(trimmed)
        .or_else(|_| STANDARD_NO_PAD.decode(trimmed))
        .or_else(|_| STANDARD.decode(trimmed))
        .ok()
}

/// Decodes a 32-byte key from base64 (standard or URL-safe) or hex.
pub fn decode_key(value: &str, name: &str) -> Result<[u8; 32]> {
    let trimmed = value.trim();
    // Try both encodings and accept only the one that yields exactly 32 bytes:
    // a 64-character hex string is also valid base64, so "try base64 first"
    // silently produces a 48-byte key.
    let mut saw_candidate = false;
    for bytes in [b64_decode(trimmed), hex::decode(trimmed).ok()]
        .into_iter()
        .flatten()
    {
        saw_candidate = true;
        if let Ok(key) = <[u8; 32]>::try_from(bytes.as_slice()) {
            return Ok(key);
        }
    }
    if saw_candidate {
        return Err(AppError::config(
            "invalid_key_length",
            format!("{name} must be exactly 32 bytes"),
            Some("Generate one with `openssl rand -base64 32`.".to_string()),
        ));
    }
    Err(AppError::config(
        "invalid_key",
        format!("{name} is not valid base64 or hex"),
        Some("Provide 32 random bytes encoded as base64 or hex.".to_string()),
    ))
}

/// Hashes a high-entropy pairing secret. A keyed hash is unnecessary: the
/// secret is 32 random bytes, so a plain BLAKE3 digest is preimage-resistant.
pub fn hash_pairing_secret(secret: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(PAIRING_DOMAIN);
    hasher.update(secret.as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// Constant-time pairing-secret comparison.
pub fn verify_pairing_secret(secret: &str, stored_hash: &str) -> bool {
    let computed = hash_pairing_secret(secret);
    computed.as_bytes().ct_eq(stored_hash.as_bytes()).into()
}

/// Hashes a refresh token for storage.
pub fn hash_refresh_token(token: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(REFRESH_DOMAIN);
    hasher.update(token.as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// The relay's Ed25519 signing identity.
#[derive(Clone)]
pub struct Keystore {
    signing: SigningKey,
    verifying: VerifyingKey,
    kid: String,
}

impl fmt::Debug for Keystore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Keystore").field("kid", &self.kid).finish()
    }
}

impl Keystore {
    /// Builds the keystore from a 32-byte seed.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        let signing = SigningKey::from_bytes(seed);
        let verifying = signing.verifying_key();
        let kid = blake3::hash(verifying.as_bytes()).to_hex().to_string()[..16].to_string();
        Self {
            signing,
            verifying,
            kid,
        }
    }

    /// Builds the keystore from configuration.
    pub fn from_config(config: &crate::config::Config) -> Result<Self> {
        let value = config.signing_key.as_deref().ok_or_else(|| {
            AppError::config("missing_signing_key", "signing key is missing", None)
        })?;
        Ok(Self::from_seed(&decode_key(value, "signing_key")?))
    }

    /// Signature key id.
    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// Verifying key.
    pub fn verifying_key(&self) -> &VerifyingKey {
        &self.verifying
    }

    /// Signs a message.
    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        self.signing.sign(message).to_bytes()
    }
}

/// One envelope-encrypted secret row.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SealedSecret {
    /// Nonce that wraps the DEK.
    pub dek_nonce: Vec<u8>,
    /// DEK encrypted by the KEK.
    pub wrapped_dek: Vec<u8>,
    /// Nonce for the payload.
    pub nonce: Vec<u8>,
    /// Encrypted payload.
    pub ciphertext: Vec<u8>,
}

/// XChaCha20-Poly1305 envelope encryption under a KEK.
#[derive(Clone)]
pub struct Envelope {
    kek: [u8; 32],
}

impl fmt::Debug for Envelope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Envelope(<redacted>)")
    }
}

impl Envelope {
    /// Builds the envelope from configuration.
    pub fn from_config(config: &crate::config::Config) -> Result<Self> {
        Ok(Self {
            kek: decode_key(&config.kek, "kek")?,
        })
    }

    /// Builds the envelope from a raw KEK.
    pub fn new(kek: [u8; 32]) -> Self {
        Self { kek }
    }

    fn cipher(&self, key: &[u8; 32]) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new(Key::from_slice(key))
    }

    /// Encrypts `plaintext` with a fresh DEK, binding it to `aad`.
    pub fn seal(&self, plaintext: &[u8], aad: &[u8]) -> Result<SealedSecret> {
        let dek: [u8; 32] = random_bytes(32).try_into().expect("32 bytes");
        let dek_nonce: [u8; 24] = random_bytes(24).try_into().expect("24 bytes");
        let nonce: [u8; 24] = random_bytes(24).try_into().expect("24 bytes");

        let wrapped_dek = self
            .cipher(&self.kek)
            .encrypt(
                XNonce::from_slice(&dek_nonce),
                Payload {
                    msg: &dek,
                    aad: ENVELOPE_AAD,
                },
            )
            .map_err(|_| AppError::internal("wrap DEK"))?;

        let ciphertext = self
            .cipher(&dek)
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|_| AppError::internal("seal secret"))?;

        Ok(SealedSecret {
            dek_nonce: dek_nonce.to_vec(),
            wrapped_dek,
            nonce: nonce.to_vec(),
            ciphertext,
        })
    }

    /// Decrypts a sealed secret. A tampered row or a wrong KEK fails closed.
    pub fn open(&self, sealed: &SealedSecret, aad: &[u8]) -> Result<Vec<u8>> {
        let dek_nonce: [u8; 24] = sealed
            .dek_nonce
            .as_slice()
            .try_into()
            .map_err(|_| AppError::internal("bad DEK nonce"))?;
        let nonce: [u8; 24] = sealed
            .nonce
            .as_slice()
            .try_into()
            .map_err(|_| AppError::internal("bad secret nonce"))?;
        let dek = self
            .cipher(&self.kek)
            .decrypt(
                XNonce::from_slice(&dek_nonce),
                Payload {
                    msg: &sealed.wrapped_dek,
                    aad: ENVELOPE_AAD,
                },
            )
            .map_err(|_| AppError::internal("unwrap DEK (wrong KEK or tampered row)"))?;
        let dek: [u8; 32] = dek
            .as_slice()
            .try_into()
            .map_err(|_| AppError::internal("bad DEK length"))?;
        self.cipher(&dek)
            .decrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &sealed.ciphertext,
                    aad,
                },
            )
            .map_err(|_| AppError::internal("open secret (tampered row)"))
    }

    /// Encrypts a UTF-8 string.
    pub fn seal_str(&self, plaintext: &str, aad: &[u8]) -> Result<SealedSecret> {
        self.seal(plaintext.as_bytes(), aad)
    }

    /// Decrypts a UTF-8 string.
    pub fn open_str(&self, sealed: &SealedSecret, aad: &[u8]) -> Result<String> {
        let bytes = self.open(sealed, aad)?;
        String::from_utf8(bytes).map_err(|_| AppError::internal("secret is not UTF-8"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_roundtrips() {
        let envelope = Envelope::new([7u8; 32]);
        let sealed = envelope
            .seal_str("refresh-token-value", b"oidc:orbitid")
            .unwrap();
        assert_ne!(sealed.ciphertext, b"refresh-token-value");
        assert_eq!(
            envelope.open_str(&sealed, b"oidc:orbitid").unwrap(),
            "refresh-token-value"
        );
    }

    #[test]
    fn envelope_rejects_the_wrong_key_and_tampering() {
        let envelope = Envelope::new([7u8; 32]);
        let other = Envelope::new([8u8; 32]);
        let mut sealed = envelope.seal_str("secret", b"purpose").unwrap();
        assert!(other.open_str(&sealed, b"purpose").is_err());
        sealed.ciphertext[0] ^= 0xff;
        assert!(envelope.open_str(&sealed, b"purpose").is_err());
    }

    #[test]
    fn every_seal_uses_a_fresh_dek_and_nonce() {
        let envelope = Envelope::new([1u8; 32]);
        let first = envelope.seal_str("same", b"aad").unwrap();
        let second = envelope.seal_str("same", b"aad").unwrap();
        assert_ne!(first.wrapped_dek, second.wrapped_dek);
        assert_ne!(first.ciphertext, second.ciphertext);
    }

    #[test]
    fn pairing_secret_hash_verifies_only_the_original() {
        let secret = random_b64(32);
        let hash = hash_pairing_secret(&secret);
        assert!(verify_pairing_secret(&secret, &hash));
        assert!(!verify_pairing_secret(&random_b64(32), &hash));
    }

    #[test]
    fn keystore_signs_with_ed25519() {
        let keystore = Keystore::from_seed(&[3u8; 32]);
        let signature = keystore.sign(b"hello");
        let verifying = keystore.verifying_key();
        assert!(
            verifying
                .verify_strict(b"hello", &ed25519_dalek::Signature::from_bytes(&signature))
                .is_ok()
        );
    }

    #[test]
    fn decodes_base64_and_hex_keys() {
        assert_eq!(decode_key(&random_b64(32), "k").unwrap().len(), 32);
        assert_eq!(decode_key(&"ab".repeat(32), "k").unwrap().len(), 32);
        assert!(decode_key("short", "k").is_err());
    }
}
