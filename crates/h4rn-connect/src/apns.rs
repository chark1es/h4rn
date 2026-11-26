//! Apple Push Notification service sender for iOS alerts (ADR-052).
//!
//! The operator supplies an APNs auth key (`.p8`), its key id and the Apple
//! Developer team id. Connect signs an ES256 provider token with the key,
//! caches it until shortly before Apple's one-hour limit, and posts an alert
//! payload over HTTP/2 to `https://api.push.apple.com/3/device/<token>`.
//! Nothing is sent when any of the three values is missing:
//! [`ApnsClient::configured`] is the truth the API reports.
//!
//! The endpoint is configuration so tests can point it at a mock server. The
//! signing key is never logged or serialised.
//!
//! iOS is sent APNs directly rather than through FCM. The mobile shell links
//! its Swift code as a local Swift Package through `swift-rs`, which links the
//! package's own archive only; it cannot link the Firebase Messaging SDK's
//! dependency archives, so an FCM-for-iOS token is not obtainable from this
//! build. The payload shape and behaviour are the ones FCM would have carried
//! (ADR-052).

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use p256::ecdsa::SigningKey;
use p256::ecdsa::signature::Signer;
use p256::pkcs8::DecodePrivateKey;
use tokio::sync::Mutex;

use crate::config::Config;
use crate::push::{SendError, retry_sleep};

/// How long a signed provider token is reused. Apple rejects tokens older than
/// an hour and recommends refreshing well inside that.
const JWT_LIFETIME: chrono::Duration = chrono::Duration::minutes(50);

/// How long APNs may hold an alert for a phone that is offline. Matches the
/// Android message's 300-second TTL.
const ALERT_TTL_SECS: i64 = 300;

struct ApnsKey {
    key_id: String,
    team_id: String,
    signing_key: SigningKey,
}

struct CachedJwt {
    value: String,
    expires_at: DateTime<Utc>,
}

impl std::fmt::Debug for CachedJwt {
    /// A provider token is a bearer credential: it must never reach a log.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CachedJwt")
            .field("value", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// The APNs sender and its cached provider token.
pub struct ApnsClient {
    http: reqwest::Client,
    key: Option<ApnsKey>,
    endpoint: String,
    topic: String,
    jwt: Mutex<Option<CachedJwt>>,
    /// How many provider tokens this client has signed.
    ///
    /// ES256 is deterministic, so two signatures over the same claims are
    /// byte-identical and a test cannot tell a cache hit from a fresh signing
    /// by comparing them. This counter is the observable difference.
    signings: std::sync::atomic::AtomicU64,
}

impl std::fmt::Debug for ApnsClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ApnsClient")
            .field("configured", &self.configured())
            .field("topic", &self.topic)
            .finish_non_exhaustive()
    }
}

impl ApnsClient {
    /// Builds a sender from deployment configuration.
    ///
    /// `H4RN_CONNECT_APNS_KEY` may hold the PEM itself or a path to the file.
    /// A malformed or partial configuration logs an error and leaves iOS push
    /// off; it must not stop the relay from starting.
    pub fn new(config: &Config, http: reqwest::Client) -> Self {
        let key = match (
            config.apns_key.as_deref(),
            config.apns_key_id.as_deref(),
            config.apns_team_id.as_deref(),
        ) {
            (None, None, None) => None,
            (Some(raw), Some(key_id), Some(team_id)) => match load_key(raw) {
                Ok(signing_key) => Some(ApnsKey {
                    key_id: key_id.trim().to_string(),
                    team_id: team_id.trim().to_string(),
                    signing_key,
                }),
                Err(error) => {
                    tracing::error!(%error, "APNs key is invalid; iOS push is off");
                    None
                }
            },
            _ => {
                tracing::warn!(
                    "iOS push is off: H4RN_CONNECT_APNS_KEY, H4RN_CONNECT_APNS_KEY_ID and \
                     H4RN_CONNECT_APNS_TEAM_ID must all be set"
                );
                None
            }
        };
        Self {
            http,
            key,
            endpoint: config.apns_url.trim_end_matches('/').to_string(),
            topic: config.apns_topic.clone(),
            jwt: Mutex::new(None),
            signings: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// A sender with no key, for tests that must see iOS push off.
    pub fn disabled(http: reqwest::Client) -> Self {
        Self {
            http,
            key: None,
            endpoint: String::new(),
            topic: String::new(),
            jwt: Mutex::new(None),
            signings: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Whether an operator configured a complete APNs key.
    pub fn configured(&self) -> bool {
        self.key.is_some()
    }

    /// How many provider tokens this client has signed.
    ///
    /// Exposed so tests can prove a second send reused the cached token rather
    /// than signing an identical one; ES256 signatures are deterministic.
    #[doc(hidden)]
    pub fn signings(&self) -> u64 {
        self.signings.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Sends one alert to one APNs device token.
    ///
    /// 429 and 5xx responses are retried with backoff, and an expired provider
    /// token is re-signed once. `410` (or a body that says `Unregistered`)
    /// means the token is gone; everything else is a failure that leaves the
    /// registration in place.
    pub async fn send(&self, token: &str, payload: &serde_json::Value) -> Result<(), SendError> {
        let Some(key) = &self.key else {
            return Err(SendError::Disabled);
        };
        let encoded = payload.to_string();
        if encoded.len() > crate::push::APNS_MAX_PAYLOAD_BYTES {
            return Err(SendError::Failed(
                "the APNs payload is larger than Apple's 4096-byte limit".to_string(),
            ));
        }
        let url = format!("{}/3/device/{}", self.endpoint, token);
        let expiration = (Utc::now() + chrono::Duration::seconds(ALERT_TTL_SECS))
            .timestamp()
            .to_string();
        let mut refreshed = false;
        let mut attempt = 0u32;
        loop {
            let jwt = match self.provider_token(key).await {
                Ok(jwt) => jwt,
                Err(error) => return Err(error),
            };
            let response = self
                .http
                .post(&url)
                .header("authorization", format!("bearer {jwt}"))
                .header("apns-topic", &self.topic)
                .header("apns-push-type", "alert")
                .header("apns-priority", "10")
                .header("apns-expiration", &expiration)
                .header("content-type", "application/json")
                .body(encoded.clone())
                .send()
                .await;
            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    if attempt < 2 {
                        attempt += 1;
                        retry_sleep(attempt).await;
                        continue;
                    }
                    return Err(SendError::Failed(request_failure(error)));
                }
            };
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            if status.as_u16() == 200 {
                return Ok(());
            }
            let reason = error_reason(&text);
            if status.as_u16() == 403
                && reason.as_deref() == Some("ExpiredProviderToken")
                && !refreshed
            {
                refreshed = true;
                *self.jwt.lock().await = None;
                continue;
            }
            if is_unregistered(status.as_u16(), reason.as_deref()) {
                return Err(SendError::Unregistered);
            }
            if (status.as_u16() == 429 || status.is_server_error()) && attempt < 2 {
                attempt += 1;
                retry_sleep(attempt).await;
                continue;
            }
            return Err(SendError::Failed(format!(
                "APNs answered {}: {}",
                status.as_u16(),
                text.chars().take(300).collect::<String>()
            )));
        }
    }

    /// Returns a cached provider token until shortly before Apple's ceiling.
    async fn provider_token(&self, key: &ApnsKey) -> Result<String, SendError> {
        let now = Utc::now();
        {
            let cached = self.jwt.lock().await;
            if let Some(cached) = cached.as_ref()
                && cached.expires_at > now + chrono::Duration::seconds(60)
            {
                return Ok(cached.value.clone());
            }
        }
        let value = sign_provider_token(key, now).map_err(SendError::Failed)?;
        self.signings
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        *self.jwt.lock().await = Some(CachedJwt {
            value: value.clone(),
            expires_at: now + JWT_LIFETIME,
        });
        Ok(value)
    }
}

/// Parses an APNs error body's `reason`, the only part that classifies it.
fn error_reason(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value["reason"].as_str().map(ToString::to_string))
}

/// Describes a failed request without its URL.
///
/// The URL ends in the device token, which is a delivery credential; an error
/// string reaches logs, so it must not carry the token with it.
fn request_failure(error: reqwest::Error) -> String {
    format!("APNs request failed: {}", error.without_url())
}

/// The number of hex characters an APNs device token may have.
///
/// Apple currently issues 64; the range leaves room for a longer format while
/// still refusing an FCM token or a truncated value.
const MIN_DEVICE_TOKEN_CHARS: usize = 64;
const MAX_DEVICE_TOKEN_CHARS: usize = 200;

/// Whether a value looks like an APNs device token.
///
/// A registration that stores something else would make every send fail with
/// `BadDeviceToken` and never delete the row, so the shape is checked when the
/// phone registers rather than on each alert.
pub fn is_valid_device_token(token: &str) -> bool {
    (MIN_DEVICE_TOKEN_CHARS..=MAX_DEVICE_TOKEN_CHARS).contains(&token.len())
        && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Whether APNs says this device token is gone for good.
///
/// Only a `410` or an explicit `Unregistered` reason counts. `BadDeviceToken`
/// is not enough: a development build sent to the production host (or the
/// reverse) answers with it for a token that is still valid elsewhere, and
/// deleting every registration on that would silence a phone because of an
/// operator misconfiguration.
fn is_unregistered(status: u16, reason: Option<&str>) -> bool {
    reason == Some("Unregistered") || status == 410
}

fn load_key(raw: &str) -> Result<SigningKey, String> {
    let text = if raw.contains("BEGIN PRIVATE KEY") {
        raw.to_string()
    } else {
        std::fs::read_to_string(raw.trim())
            .map_err(|error| format!("could not read the APNs key file: {error}"))?
    };
    SigningKey::from_pkcs8_pem(&text)
        .map_err(|error| format!("APNs key is not PKCS#8 PEM: {error}"))
}

/// Signs an ES256 provider token.
fn sign_provider_token(key: &ApnsKey, now: DateTime<Utc>) -> Result<String, String> {
    let header = serde_json::json!({"alg": "ES256", "kid": key.key_id});
    let claims = serde_json::json!({
        "iss": key.team_id,
        "iat": now.timestamp(),
    });
    let encode = |value: &serde_json::Value| -> Result<String, String> {
        let bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
        Ok(URL_SAFE_NO_PAD.encode(bytes))
    };
    let unsigned = format!("{}.{}", encode(&header)?, encode(&claims)?);
    let signature: p256::ecdsa::Signature = key.signing_key.sign(unsigned.as_bytes());
    Ok(format!(
        "{unsigned}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    ))
}

/// Shared sender handle used by the alert service.
pub type SharedApns = Arc<ApnsClient>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_key_is_off() {
        let client = ApnsClient::disabled(reqwest::Client::new());
        assert!(!client.configured());
    }

    #[test]
    fn a_partial_configuration_is_off() {
        let config = Config {
            apns_key: Some("partial-key-material".to_string()),
            apns_key_id: Some("KEYID12345".to_string()),
            ..Config::default()
        };
        let client = ApnsClient::new(&config, reqwest::Client::new());
        assert!(
            !client.configured(),
            "two of three values must not enable it"
        );
    }

    #[test]
    fn an_inline_key_is_parsed_without_logging_it() {
        let key = SigningKey::random(&mut rand_core::OsRng);
        let pem = p256::pkcs8::EncodePrivateKey::to_pkcs8_pem(&key, p256::pkcs8::LineEnding::LF)
            .expect("encode PKCS#8 PEM");
        let client = ApnsClient::new(
            &Config {
                apns_key: Some(pem.to_string()),
                apns_key_id: Some("KEYID12345".to_string()),
                apns_team_id: Some("TEAM123456".to_string()),
                ..Config::default()
            },
            reqwest::Client::new(),
        );
        assert!(client.configured());
        let debug = format!("{client:?}");
        assert!(!debug.contains("PRIVATE KEY"));
        assert!(!debug.contains("KEYID12345"));
    }

    #[test]
    fn a_missing_key_file_is_an_error_not_a_silent_success() {
        let error = load_key("/nonexistent/h4rn-apns-key.p8").unwrap_err();
        assert!(error.contains("could not read"));
    }

    #[test]
    fn the_provider_token_is_a_verifiable_es256_jwt() {
        use p256::ecdsa::VerifyingKey;
        use p256::ecdsa::signature::Verifier;

        let key = SigningKey::random(&mut rand_core::OsRng);
        let apns_key = ApnsKey {
            key_id: "KEYID12345".to_string(),
            team_id: "TEAM123456".to_string(),
            signing_key: key.clone(),
        };
        let jwt = sign_provider_token(&apns_key, Utc::now()).expect("sign");
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).expect("header b64"))
                .expect("header json");
        assert_eq!(header["alg"], "ES256");
        assert_eq!(header["kid"], "KEYID12345");
        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).expect("claims b64"))
                .expect("claims json");
        assert_eq!(claims["iss"], "TEAM123456");

        let signature = p256::ecdsa::Signature::from_slice(
            &URL_SAFE_NO_PAD.decode(parts[2]).expect("signature b64"),
        )
        .expect("raw signature");
        let verifying = VerifyingKey::from(&key);
        verifying
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .expect("signature verifies");
    }

    #[test]
    fn a_cached_provider_token_never_prints_itself() {
        let cached = CachedJwt {
            value: "header.claims.signature".to_string(),
            expires_at: Utc::now(),
        };
        let debug = format!("{cached:?}");
        assert!(!debug.contains("signature"), "{debug}");
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn an_ios_device_token_must_be_hex_and_the_right_size() {
        assert!(is_valid_device_token(&"ab".repeat(32)));
        assert!(is_valid_device_token(&"CD".repeat(100)));
        // Too short, too long, not hex, or not a token at all.
        assert!(!is_valid_device_token(&"ab".repeat(31)));
        assert!(!is_valid_device_token(&"ab".repeat(101)));
        assert!(!is_valid_device_token(&"zz".repeat(32)));
        assert!(!is_valid_device_token("fcm-token:APA91b-something"));
        assert!(!is_valid_device_token(""));
    }

    #[tokio::test]
    async fn a_request_error_does_not_name_the_device_token() {
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("client");
        let error = client
            .post("http://127.0.0.1:1/3/device/deadbeefcafe")
            .send()
            .await
            .expect_err("nothing listens on port 1");
        assert!(error.url().is_some(), "the failure must have carried a URL");
        let described = request_failure(error);
        assert!(!described.contains("127.0.0.1"), "{described}");
        assert!(!described.contains("deadbeefcafe"), "{described}");
    }

    #[test]
    fn only_a_gone_token_is_deleted() {
        assert!(is_unregistered(410, None));
        assert!(is_unregistered(400, Some("Unregistered")));
        // A development/production mismatch answers BadDeviceToken for a token
        // that is still valid on the other host; keeping the row is safer.
        assert!(!is_unregistered(400, Some("BadDeviceToken")));
        assert!(!is_unregistered(403, Some("InvalidProviderToken")));
        assert!(!is_unregistered(429, None));
        assert!(!is_unregistered(500, None));
        assert!(!is_unregistered(200, None));
    }

    #[test]
    fn an_error_body_without_a_reason_is_unknown() {
        assert_eq!(error_reason(""), None);
        assert_eq!(error_reason("<html>bad gateway</html>"), None);
        assert_eq!(
            error_reason(r#"{"reason":"Unregistered"}"#),
            Some("Unregistered".to_string())
        );
    }
}
