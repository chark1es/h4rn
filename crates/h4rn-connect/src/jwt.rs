//! h4rn's own session tokens: short-lived EdDSA access JWTs plus rotating,
//! hashed refresh tokens (ADR-011).
//!
//! OIDC is used to authenticate a human once. Everything after that is h4rn's
//! token: an Ed25519-signed JWT claiming the user, device and session, and a
//! refresh token whose hash is the only thing Postgres stores.

use chrono::{DateTime, Utc};
use ed25519_dalek::Signature;
use serde::{Deserialize, Serialize};

use crate::crypto::{Keystore, b64, b64_decode};
use crate::error::{AppError, Result};

/// JWT issuer claim. Not a URL: the token never leaves this relay.
pub const TOKEN_ISSUER: &str = "h4rn-connect";
/// JWT audience claim.
pub const TOKEN_AUDIENCE: &str = "h4rn";

/// Access-token claims.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessClaims {
    /// Issuer.
    pub iss: String,
    /// User id.
    pub sub: String,
    /// Audience.
    pub aud: String,
    /// Issued at (Unix seconds).
    pub iat: i64,
    /// Expiry (Unix seconds).
    pub exp: i64,
    /// Token id.
    pub jti: String,
    /// Device the token is bound to.
    pub device_id: String,
    /// Session row.
    pub session_id: String,
    /// Refresh-token family.
    pub family_id: String,
    /// Token kind. Always `access`.
    pub typ: String,
    /// Scopes the device holds (`view`, `approve`, `terminal`, `files`,
    /// `admin`). Tokens minted before scopes existed carry none.
    #[serde(default)]
    pub scp: Vec<String>,
}

#[derive(Serialize)]
struct JwtHeader<'a> {
    alg: &'a str,
    typ: &'a str,
    kid: &'a str,
}

/// Issues a signed access token.
pub fn issue_access_token(
    keystore: &Keystore,
    user_id: &str,
    device_id: &str,
    session_id: &str,
    family_id: &str,
    jti: &str,
    scopes: &[String],
    ttl_secs: i64,
) -> Result<String> {
    let now = Utc::now();
    let claims = AccessClaims {
        iss: TOKEN_ISSUER.to_string(),
        sub: user_id.to_string(),
        aud: TOKEN_AUDIENCE.to_string(),
        iat: now.timestamp(),
        exp: now.timestamp() + ttl_secs.max(1),
        jti: jti.to_string(),
        device_id: device_id.to_string(),
        session_id: session_id.to_string(),
        family_id: family_id.to_string(),
        typ: "access".to_string(),
        scp: scopes.to_vec(),
    };
    let header = JwtHeader {
        alg: "EdDSA",
        typ: "JWT",
        kid: keystore.kid(),
    };
    let header = serde_json::to_vec(&header).map_err(|_| AppError::internal("JWT header"))?;
    let claims = serde_json::to_vec(&claims).map_err(|_| AppError::internal("JWT claims"))?;
    let signing_input = format!("{}.{}", b64(&header), b64(&claims));
    let signature = keystore.sign(signing_input.as_bytes());
    Ok(format!("{signing_input}.{}", b64(&signature)))
}

/// Verifies an access token's signature and registered claims.
pub fn verify_access_token(
    keystore: &Keystore,
    token: &str,
    now: DateTime<Utc>,
) -> Result<AccessClaims> {
    let mut parts = token.split('.');
    let (Some(header_b64), Some(claims_b64), Some(signature_b64), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(AppError::unauthorized(
            "invalid_token",
            "access token is malformed",
        ));
    };

    let header_bytes = b64_decode(header_b64)
        .ok_or_else(|| AppError::unauthorized("invalid_token", "bad JWT header"))?;
    let header: serde_json::Value = serde_json::from_slice(&header_bytes)
        .map_err(|_| AppError::unauthorized("invalid_token", "bad JWT header"))?;
    if header.get("alg").and_then(|v| v.as_str()) != Some("EdDSA") {
        return Err(AppError::unauthorized(
            "invalid_token",
            "unsupported access-token algorithm",
        ));
    }
    let kid_ok = header
        .get("kid")
        .and_then(|v| v.as_str())
        .is_some_and(|kid| kid == keystore.kid());
    if !kid_ok {
        return Err(AppError::unauthorized(
            "invalid_token",
            "access token was issued by another key",
        ));
    }

    let signature_bytes = b64_decode(signature_b64)
        .ok_or_else(|| AppError::unauthorized("invalid_token", "bad JWT signature"))?;
    let signature_bytes: [u8; 64] = signature_bytes
        .as_slice()
        .try_into()
        .map_err(|_| AppError::unauthorized("invalid_token", "bad JWT signature"))?;
    let signing_input = format!("{header_b64}.{claims_b64}");
    keystore
        .verifying_key()
        .verify_strict(
            signing_input.as_bytes(),
            &Signature::from_bytes(&signature_bytes),
        )
        .map_err(|_| AppError::unauthorized("invalid_token", "bad JWT signature"))?;

    let claims_bytes = b64_decode(claims_b64)
        .ok_or_else(|| AppError::unauthorized("invalid_token", "bad JWT claims"))?;
    let claims: AccessClaims = serde_json::from_slice(&claims_bytes)
        .map_err(|_| AppError::unauthorized("invalid_token", "bad JWT claims"))?;

    if claims.iss != TOKEN_ISSUER || claims.aud != TOKEN_AUDIENCE {
        return Err(AppError::unauthorized(
            "invalid_token",
            "access token audience mismatch",
        ));
    }
    if claims.typ != "access" {
        return Err(AppError::unauthorized(
            "invalid_token",
            "not an access token",
        ));
    }
    if claims.exp <= now.timestamp() {
        return Err(AppError::unauthorized(
            "token_expired",
            "access token has expired",
        ));
    }
    if claims.iat > now.timestamp() + 60 {
        return Err(AppError::unauthorized(
            "invalid_token",
            "access token is from the future",
        ));
    }
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;

    #[test]
    fn access_token_roundtrips() {
        let keystore = Keystore::from_seed(&[9u8; 32]);
        let token = issue_access_token(
            &keystore,
            "user-1",
            "device-1",
            "session-1",
            "family-1",
            "jti-1",
            &[],
            600,
        )
        .unwrap();
        let claims = verify_access_token(&keystore, &token, Utc::now()).unwrap();
        assert_eq!(claims.sub, "user-1");
        assert_eq!(claims.device_id, "device-1");
        assert_eq!(claims.jti, "jti-1");
        assert!(claims.scp.is_empty());
    }

    #[test]
    fn access_token_carries_exactly_the_issued_scopes() {
        let keystore = Keystore::from_seed(&[9u8; 32]);
        let scopes = vec!["view".to_string(), "approve".to_string()];
        let token = issue_access_token(&keystore, "u", "d", "s", "f", "j", &scopes, 600).unwrap();
        let claims = verify_access_token(&keystore, &token, Utc::now()).unwrap();
        assert_eq!(claims.scp, scopes);
    }

    #[test]
    fn expired_tokens_are_rejected() {
        let keystore = Keystore::from_seed(&[9u8; 32]);
        let token = issue_access_token(
            &keystore,
            "user-1",
            "device-1",
            "session-1",
            "family-1",
            "jti-1",
            &[],
            60,
        )
        .unwrap();
        let later = Utc::now() + Duration::seconds(61);
        let error = verify_access_token(&keystore, &token, later).unwrap_err();
        assert_eq!(error.body().code, "token_expired");
    }

    #[test]
    fn another_key_or_a_tampered_payload_is_rejected() {
        let keystore = Keystore::from_seed(&[9u8; 32]);
        let other = Keystore::from_seed(&[10u8; 32]);
        let token = issue_access_token(
            &keystore,
            "user-1",
            "device-1",
            "session-1",
            "family-1",
            "jti-1",
            &[],
            600,
        )
        .unwrap();
        assert!(verify_access_token(&other, &token, Utc::now()).is_err());

        let mut parts: Vec<&str> = token.split('.').collect();
        let forged_claims = b64(serde_json::to_vec(&serde_json::json!({
            "iss": TOKEN_ISSUER,
            "sub": "admin",
            "aud": TOKEN_AUDIENCE,
            "iat": Utc::now().timestamp(),
            "exp": Utc::now().timestamp() + 600,
            "jti": "x",
            "device_id": "d",
            "session_id": "s",
            "family_id": "f",
            "typ": "access"
        }))
        .unwrap()
        .as_slice());
        parts[1] = &forged_claims;
        assert!(verify_access_token(&keystore, &parts.join("."), Utc::now()).is_err());
    }
}
