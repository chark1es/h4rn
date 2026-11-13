//! DPoP-style proof of possession for h4rn session tokens.
//!
//! A bearer token alone is useless: every authenticated request must carry an
//! Ed25519 signature over the method, path and a recent timestamp, made with
//! the device's private key. The relay only stores the public key, so a stolen
//! token cannot be replayed from another machine (ADR-010, ADR-011).
//!
//! This is a deliberately small profile of DPoP. There is no server-issued
//! nonce and no `htm`/`htu` JWT: the proof is a header triple plus a signature,
//! because the relay already terminates TLS and only needs sender binding.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, VerifyingKey};

use crate::crypto::b64_decode;
use crate::error::{AppError, Result};

/// Domain separation for proof signatures.
const PROOF_DOMAIN: &str = "h4rn-dpop-v1";

/// The exact string a device signs.
///
/// `nonce` is a random per-request value. Ed25519 signatures are deterministic,
/// so without it two identical requests in the same second would produce the
/// same signature and the relay's replay cache would reject the second one.
pub fn canonical_proof(method: &str, path: &str, timestamp: i64, nonce: &str) -> String {
    format!(
        "{PROOF_DOMAIN}\n{}\n{}\n{timestamp}\n{nonce}",
        method.to_ascii_uppercase(),
        path
    )
}

/// One DPoP proof to verify.
#[derive(Clone, Copy, Debug)]
pub struct Proof<'a> {
    /// Device public key, base64url (unpadded).
    pub public_key: &'a str,
    /// HTTP method.
    pub method: &'a str,
    /// Request path without the query string.
    pub path: &'a str,
    /// Unix timestamp in seconds.
    pub timestamp: i64,
    /// Random per-request nonce.
    pub nonce: &'a str,
    /// Ed25519 signature, base64url (unpadded).
    pub signature: &'a str,
}

/// Verifies a proof with the device's stored public key.
///
/// `public_key` is base64url (unpadded); `signature` is the base64url Ed25519
/// signature over [`canonical_proof`].
pub fn verify_proof(proof: &Proof<'_>, now: DateTime<Utc>, max_skew_secs: i64) -> Result<()> {
    let Proof {
        public_key,
        method,
        path,
        timestamp,
        nonce,
        signature,
    } = *proof;
    if nonce.is_empty() || nonce.len() > 128 {
        return Err(AppError::unauthorized(
            "proof_invalid",
            "proof nonce must be 1-128 characters",
        ));
    }
    if (now.timestamp() - timestamp).abs() > max_skew_secs.max(1) {
        return Err(AppError::unauthorized(
            "proof_stale",
            "proof timestamp is outside the allowed window",
        ));
    }
    let key_bytes = b64_decode(public_key).ok_or_else(|| {
        AppError::unauthorized("invalid_device_key", "device public key is not base64")
    })?;
    let key_bytes: [u8; 32] = key_bytes
        .as_slice()
        .try_into()
        .map_err(|_| AppError::unauthorized("invalid_device_key", "device key must be 32 bytes"))?;
    let verifying = VerifyingKey::from_bytes(&key_bytes).map_err(|_| {
        AppError::unauthorized("invalid_device_key", "device public key is not Ed25519")
    })?;
    let signature_bytes = b64_decode(signature)
        .ok_or_else(|| AppError::unauthorized("invalid_proof", "proof is not base64"))?;
    let signature_bytes: [u8; 64] = signature_bytes
        .as_slice()
        .try_into()
        .map_err(|_| AppError::unauthorized("invalid_proof", "proof must be 64 bytes"))?;

    let message = canonical_proof(method, path, timestamp, nonce);
    verifying
        .verify_strict(message.as_bytes(), &Signature::from_bytes(&signature_bytes))
        .map_err(|_| AppError::unauthorized("invalid_proof", "proof signature is invalid"))
}

/// In-memory replay cache for proof signatures.
///
/// The timestamp window already bounds replay to a minute; this closes it to
/// one use per signature per process. A multi-instance deployment needs a
/// shared cache for that strict guarantee, which is documented rather than
/// silently assumed.
#[derive(Debug, Default)]
pub struct ReplayCache {
    seen: Mutex<HashMap<String, i64>>,
}

impl ReplayCache {
    /// Records a proof and returns `true` the first time it is seen.
    pub fn first_use(&self, proof: &str, timestamp: i64, now: i64) -> bool {
        let mut seen = self.seen.lock().expect("replay cache poisoned");
        if seen.len() > 4096 {
            seen.retain(|_, seen_at| *seen_at + 300 >= now);
        }
        seen.insert(proof.to_string(), timestamp).is_none()
    }

    /// Number of cached proofs, for tests.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.seen.lock().expect("replay cache poisoned").len()
    }

    /// Whether the cache holds no proofs.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer as _, SigningKey};

    use super::*;
    use crate::crypto::b64;

    fn proof_for(seed: [u8; 32], method: &str, path: &str, ts: i64) -> (String, String) {
        let signing = SigningKey::from_bytes(&seed);
        let message = canonical_proof(method, path, ts, "nonce-1");
        let signature = signing.sign(message.as_bytes());
        (
            b64(signing.verifying_key().as_bytes()),
            b64(&signature.to_bytes()),
        )
    }

    fn check(
        key: &str,
        method: &str,
        path: &str,
        ts: i64,
        nonce: &str,
        signature: &str,
    ) -> Result<()> {
        verify_proof(
            &Proof {
                public_key: key,
                method,
                path,
                timestamp: ts,
                nonce,
                signature,
            },
            Utc::now(),
            60,
        )
    }

    #[test]
    fn a_valid_proof_passes() {
        let ts = Utc::now().timestamp();
        let (key, proof) = proof_for([4u8; 32], "GET", "/v1/me", ts);
        check(&key, "GET", "/v1/me", ts, "nonce-1", &proof).unwrap();
    }

    #[test]
    fn method_path_timestamp_nonce_and_key_are_all_bound() {
        let ts = Utc::now().timestamp();
        let (key, proof) = proof_for([4u8; 32], "GET", "/v1/me", ts);
        assert_eq!(
            check(&key, "POST", "/v1/me", ts, "nonce-1", &proof)
                .unwrap_err()
                .body()
                .code,
            "invalid_proof"
        );
        assert_eq!(
            check(&key, "GET", "/v1/devices", ts, "nonce-1", &proof)
                .unwrap_err()
                .body()
                .code,
            "invalid_proof"
        );
        assert_eq!(
            check(&key, "GET", "/v1/me", ts + 1, "nonce-1", &proof)
                .unwrap_err()
                .body()
                .code,
            "invalid_proof"
        );
        assert_eq!(
            check(&key, "GET", "/v1/me", ts, "nonce-2", &proof)
                .unwrap_err()
                .body()
                .code,
            "invalid_proof"
        );
        let (other_key, _) = proof_for([5u8; 32], "GET", "/v1/me", ts);
        assert_eq!(
            check(&other_key, "GET", "/v1/me", ts, "nonce-1", &proof)
                .unwrap_err()
                .body()
                .code,
            "invalid_proof"
        );
    }

    #[test]
    fn stale_proofs_are_rejected() {
        let ts = Utc::now().timestamp() - 120;
        let (key, proof) = proof_for([4u8; 32], "GET", "/v1/me", ts);
        assert_eq!(
            check(&key, "GET", "/v1/me", ts, "nonce-1", &proof)
                .unwrap_err()
                .body()
                .code,
            "proof_stale"
        );
    }

    #[test]
    fn replay_cache_accepts_a_proof_once() {
        let cache = ReplayCache::default();
        let now = Utc::now().timestamp();
        assert!(cache.first_use("sig", now, now));
        assert!(!cache.first_use("sig", now, now));
        assert!(cache.first_use("other", now, now));
        assert_eq!(cache.len(), 2);
    }
}
