//! Firebase Cloud Messaging HTTP v1 sender for Android alerts (ADR-051).
//!
//! The operator supplies a Firebase service-account JSON. Connect signs a
//! JWT-bearer assertion with its RSA key, exchanges it for an OAuth2 access
//! token, caches that token until shortly before it expires, and posts **data**
//! messages to the FCM v1 endpoint. Nothing is sent when the account is
//! missing: [`FcmClient::configured`] is the truth the API reports.
//!
//! Both endpoint URLs are configuration so tests can point them at a mock
//! server. The service account private key is never logged or serialised.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use rsa::RsaPrivateKey;
use rsa::pkcs1v15::Pkcs1v15Sign;
use rsa::pkcs8::DecodePrivateKey;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use tokio::sync::Mutex;

use crate::config::Config;

/// A parsed Firebase service account.
#[derive(Clone, Deserialize)]
struct ServiceAccount {
    project_id: String,
    client_email: String,
    private_key: String,
}

impl std::fmt::Debug for ServiceAccount {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServiceAccount")
            .field("project_id", &self.project_id)
            .field("client_email", &self.client_email)
            .field("private_key", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Debug)]
struct CachedToken {
    value: String,
    expires_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: i64,
}

/// What happened to one delivery attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendError {
    /// No service account, so push is off. The caller must not claim delivery.
    Disabled,
    /// FCM says the token is no longer valid; delete it.
    Unregistered,
    /// Anything else, already retried where it made sense.
    Failed(String),
}

/// Which phone platform an alert is for.
///
/// A device row records this, because the two platforms need different
/// transports: FCM data messages for Android, an APNs alert for iOS
/// (ADR-051, ADR-052).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushPlatform {
    /// Android through FCM.
    Android,
    /// iOS through APNs.
    Ios,
}

impl PushPlatform {
    /// Parses a stored registration platform.
    ///
    /// Rows written before iOS existed carry no value and are Android
    /// installs, so an unknown value is Android rather than an error: a relay
    /// must keep alerting an existing phone through a bad label, not go silent.
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some(value) if value.eq_ignore_ascii_case("ios") => Self::Ios,
            _ => Self::Android,
        }
    }

    /// The value stored on a device row.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Android => "android",
            Self::Ios => "ios",
        }
    }
}

/// One alert for one phone.
#[derive(Clone, Debug)]
pub struct PushMessage<'a> {
    /// Push token: an FCM registration token on Android, an APNs device token
    /// on iOS.
    pub token: &'a str,
    /// Which transport carries it.
    pub platform: PushPlatform,
    /// Restrict delivery to this package (Android only).
    pub package_name: Option<&'a str>,
    /// The fixed alert fields. Android renders them itself; iOS gets the
    /// title and body in its alert block and the rest as custom data. Connect
    /// stores no copy.
    pub data: BTreeMap<String, String>,
}

impl PushMessage<'_> {
    /// The FCM HTTP v1 message body (ADR-051).
    fn fcm_body(&self) -> serde_json::Value {
        let mut body = serde_json::json!({
            "message": {
                "token": self.token,
                "data": self.data,
                "android": {
                    "priority": "HIGH",
                    "ttl": "300s",
                }
            }
        });
        if let Some(package) = self.package_name {
            body["message"]["android"]["restricted_package_name"] = package.into();
        }
        body
    }

    /// The APNs payload (ADR-052).
    ///
    /// The alert block carries Connect's English title and body, which iOS
    /// shows as sent (ADR-054), and the custom keys are the documented record
    /// fields only: no prompt, reply, tool output, path or credential can ride
    /// along.
    /// `mutable-content` lets a Notification Service Extension re-check the
    /// phone's flag before the alert is shown.
    pub fn apns_payload(&self) -> serde_json::Value {
        let mut payload = serde_json::json!({
            "aps": {
                "alert": {
                    "title": self.data.get("title").cloned().unwrap_or_default(),
                    "body": self.data.get("body").cloned().unwrap_or_default(),
                },
                "sound": "default",
                "mutable-content": 1,
            }
        });
        for key in IOS_CUSTOM_FIELDS {
            if let Some(value) = self.data.get(*key) {
                payload[*key] = value.clone().into();
            }
        }
        payload
    }
}

/// The only custom keys an iOS payload may carry (ADR-052).
///
/// `title` and `body` are not repeated: they are the alert itself. Everything
/// else in [`PushMessage::data`], including the raw thread titles Android words
/// its alerts from, exists for Android's local rendering.
pub const IOS_CUSTOM_FIELDS: &[&str] = &["h4rn_alert", "state", "thread_id", "count"];

/// The FCM sender and its cached OAuth2 token.
pub struct FcmClient {
    http: reqwest::Client,
    account: Option<ServiceAccount>,
    token_url: String,
    endpoint: String,
    token: Mutex<Option<CachedToken>>,
}

impl std::fmt::Debug for FcmClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FcmClient")
            .field("configured", &self.configured())
            .finish_non_exhaustive()
    }
}

impl FcmClient {
    /// Builds a sender from deployment configuration.
    ///
    /// `H4RN_CONNECT_FCM_SERVICE_ACCOUNT` may hold the JSON itself or a path
    /// to a file. A malformed value logs an error and leaves push off; it must
    /// not stop the relay from starting.
    pub fn new(config: &Config, http: reqwest::Client) -> Self {
        let account =
            config
                .fcm_service_account
                .as_deref()
                .and_then(|raw| match load_account(raw) {
                    Ok(account) => Some(account),
                    Err(error) => {
                        tracing::error!(%error, "FCM service account is invalid; push is off");
                        None
                    }
                });
        Self {
            http,
            account,
            token_url: config.fcm_token_url.clone(),
            endpoint: config.fcm_url.trim_end_matches('/').to_string(),
            token: Mutex::new(None),
        }
    }

    /// A sender with no account, for tests that must see push off.
    pub fn disabled(http: reqwest::Client) -> Self {
        Self {
            http,
            account: None,
            token_url: String::new(),
            endpoint: String::new(),
            token: Mutex::new(None),
        }
    }

    /// Whether an operator configured a service account.
    pub fn configured(&self) -> bool {
        self.account.is_some()
    }

    /// Sends one data message, refreshing the access token when needed.
    ///
    /// 429 and 5xx responses are retried with exponential backoff. A 401
    /// discards the cached token and retries once with a fresh one.
    pub async fn send(&self, message: &PushMessage<'_>) -> Result<(), SendError> {
        let Some(account) = &self.account else {
            return Err(SendError::Disabled);
        };
        if message.platform != PushPlatform::Android {
            return Err(SendError::Failed(
                "FCM only carries Android alerts; iOS goes through APNs".to_string(),
            ));
        }
        let body = message.fcm_body();
        let encoded = body.to_string();
        if encoded.len() > 4096 {
            return Err(SendError::Failed(
                "push data is larger than FCM's 4096-byte limit".to_string(),
            ));
        }
        let url = format!(
            "{}/v1/projects/{}/messages:send",
            self.endpoint,
            urlencoding(&account.project_id)
        );
        let mut reauthorised = false;
        let mut attempt = 0u32;
        loop {
            let token = match self.access_token(account).await {
                Ok(token) => token,
                Err(error) => return Err(error),
            };
            let response = self
                .http
                .post(&url)
                .bearer_auth(&token)
                .json(&body)
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
                    return Err(SendError::Failed(format!(
                        "FCM request failed: {}",
                        error.without_url()
                    )));
                }
            };
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            if status.is_success() {
                return Ok(());
            }
            if status.as_u16() == 401 && !reauthorised {
                reauthorised = true;
                *self.token.lock().await = None;
                continue;
            }
            if is_unregistered(status.as_u16(), &text) {
                return Err(SendError::Unregistered);
            }
            if (status.as_u16() == 429 || status.is_server_error()) && attempt < 2 {
                attempt += 1;
                retry_sleep(attempt).await;
                continue;
            }
            return Err(SendError::Failed(format!(
                "FCM answered {}: {}",
                status.as_u16(),
                text.chars().take(300).collect::<String>()
            )));
        }
    }

    /// Returns a cached token until shortly before it expires.
    async fn access_token(&self, account: &ServiceAccount) -> Result<String, SendError> {
        let now = Utc::now();
        {
            let cached = self.token.lock().await;
            if let Some(cached) = cached.as_ref()
                && cached.expires_at > now + chrono::Duration::seconds(60)
            {
                return Ok(cached.value.clone());
            }
        }
        let assertion = sign_assertion(account, now).map_err(SendError::Failed)?;
        let response = self
            .http
            .post(&self.token_url)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", assertion.as_str()),
            ])
            .send()
            .await
            .map_err(|error| {
                SendError::Failed(format!("token request failed: {}", error.without_url()))
            })?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(SendError::Failed(format!(
                "token endpoint answered {}: {}",
                status.as_u16(),
                text.chars().take(300).collect::<String>()
            )));
        }
        let body: TokenResponse = serde_json::from_str(&text)
            .map_err(|error| SendError::Failed(format!("token response is not JSON: {error}")))?;
        let expires_in = if body.expires_in > 0 {
            body.expires_in
        } else {
            3600
        };
        *self.token.lock().await = Some(CachedToken {
            value: body.access_token.clone(),
            expires_at: now + chrono::Duration::seconds(expires_in),
        });
        Ok(body.access_token)
    }
}

pub(crate) async fn retry_sleep(attempt: u32) {
    let millis = 100u64.saturating_mul(2u64.saturating_pow(attempt.saturating_sub(1)));
    tokio::time::sleep(Duration::from_millis(millis.min(1_000))).await;
}

/// Whether FCM's answer names this token as gone.
///
/// Only the body decides, and only a detail entry that says `UNREGISTERED`
/// counts. A bare top-level `NOT_FOUND`, and a 404 alone, are just as likely to
/// mean a wrong endpoint or project id; deleting every token on that would
/// silence a whole account because of an operator typo.
fn is_unregistered(status: u16, body: &str) -> bool {
    if status != 400 && status != 404 {
        return false;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    value["error"]["details"].as_array().is_some_and(|details| {
        details
            .iter()
            .any(|detail| matches!(detail["errorCode"].as_str(), Some("UNREGISTERED")))
    })
}

fn load_account(raw: &str) -> Result<ServiceAccount, String> {
    let text = if raw.trim_start().starts_with('{') {
        raw.to_string()
    } else {
        std::fs::read_to_string(raw.trim())
            .map_err(|error| format!("could not read the service account file: {error}"))?
    };
    let account: ServiceAccount = serde_json::from_str(&text)
        .map_err(|error| format!("service account is not JSON: {error}"))?;
    if account.project_id.is_empty() || account.client_email.is_empty() {
        return Err("service account has no project_id or client_email".to_string());
    }
    Ok(account)
}

/// Signs the JWT-bearer assertion with RS256.
fn sign_assertion(account: &ServiceAccount, now: DateTime<Utc>) -> Result<String, String> {
    let header = serde_json::json!({"alg": "RS256", "typ": "JWT"});
    let iat = now.timestamp();
    let claims = serde_json::json!({
        "iss": account.client_email,
        "scope": "https://www.googleapis.com/auth/firebase.messaging",
        "aud": "https://oauth2.googleapis.com/token",
        "iat": iat,
        "exp": iat + 3600,
    });
    let encode = |value: &serde_json::Value| -> Result<String, String> {
        let bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
        Ok(URL_SAFE_NO_PAD.encode(bytes))
    };
    let unsigned = format!("{}.{}", encode(&header)?, encode(&claims)?);
    let key = RsaPrivateKey::from_pkcs8_pem(&account.private_key)
        .map_err(|error| format!("service account key is not PKCS#8 PEM: {error}"))?;
    let digest = Sha256::digest(unsigned.as_bytes());
    let signature = key
        .sign(Pkcs1v15Sign::new::<Sha256>(), &digest)
        .map_err(|error| format!("could not sign the FCM assertion: {error}"))?;
    Ok(format!("{unsigned}.{}", URL_SAFE_NO_PAD.encode(signature)))
}

/// Percent-encodes a Firebase project id for a URL path segment.
fn urlencoding(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// Shared sender handle used by the alert service.
pub type SharedFcm = Arc<FcmClient>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_account_is_off() {
        let client = FcmClient::disabled(reqwest::Client::new());
        assert!(!client.configured());
    }

    #[test]
    fn a_device_row_without_a_platform_is_android() {
        // Every row written before iOS existed was an Android install.
        assert_eq!(PushPlatform::parse(None), PushPlatform::Android);
        assert_eq!(PushPlatform::parse(Some("")), PushPlatform::Android);
        assert_eq!(PushPlatform::parse(Some("android")), PushPlatform::Android);
        assert_eq!(PushPlatform::parse(Some(" iOS ")), PushPlatform::Ios);
        assert_eq!(PushPlatform::parse(Some("ios")), PushPlatform::Ios);
        // A bad label must not silence an existing phone.
        assert_eq!(PushPlatform::parse(Some("watchos")), PushPlatform::Android);
    }

    fn message(platform: PushPlatform) -> PushMessage<'static> {
        PushMessage {
            token: "device-token",
            platform,
            package_name: Some("dev.h4rn.app"),
            data: BTreeMap::from([
                ("h4rn_alert".to_string(), "agent".to_string()),
                ("state".to_string(), "approval".to_string()),
                ("thread_id".to_string(), "th_one".to_string()),
                ("title".to_string(), "Fix the thing".to_string()),
                ("body".to_string(), "h4rn · Needs approval".to_string()),
                ("count".to_string(), "1".to_string()),
            ]),
        }
    }

    #[test]
    fn the_android_shape_is_a_string_only_data_message() {
        let body = message(PushPlatform::Android).fcm_body();
        assert_eq!(body["message"]["token"], "device-token");
        assert_eq!(body["message"]["data"]["title"], "Fix the thing");
        assert_eq!(body["message"]["data"]["thread_id"], "th_one");
        assert_eq!(
            body["message"]["android"]["restricted_package_name"],
            "dev.h4rn.app"
        );
        assert!(body["message"].get("apns").is_none());
        // The display text is composed by the phone, not the server: a data
        // message has no notification block.
        assert!(body["message"].get("notification").is_none());
    }

    #[test]
    fn the_ios_shape_is_an_apns_alert_with_documented_fields_only() {
        let payload = message(PushPlatform::Ios).apns_payload();
        assert_eq!(payload["aps"]["alert"]["title"], "Fix the thing");
        assert_eq!(payload["aps"]["alert"]["body"], "h4rn · Needs approval");
        assert_eq!(payload["aps"]["sound"], "default");
        assert_eq!(payload["aps"]["mutable-content"], 1);

        // The custom keys are exactly the documented record plus the marker.
        let mut keys: Vec<&str> = payload
            .as_object()
            .expect("payload object")
            .keys()
            .map(String::as_str)
            .filter(|key| *key != "aps")
            .collect();
        keys.sort_unstable();
        let mut expected = IOS_CUSTOM_FIELDS.to_vec();
        expected.sort_unstable();
        assert_eq!(keys, expected, "an undocumented field reached the payload");
        assert_eq!(payload["thread_id"], "th_one");
        assert_eq!(payload["count"], "1");
        assert_eq!(payload["state"], "approval");
        assert_eq!(payload["h4rn_alert"], "agent");
        // The title and body are not duplicated as custom data.
        assert!(payload.get("title").is_none());
        assert!(payload.get("body").is_none());
    }

    #[tokio::test]
    async fn fcm_refuses_an_ios_message_without_making_a_request() {
        // A mislabelled device row must fail loudly, not send an APNs token to
        // FCM and have Google reject it as a bad registration token.
        let client = FcmClient {
            http: reqwest::Client::new(),
            account: Some(ServiceAccount {
                project_id: "demo".to_string(),
                client_email: "relay@demo.iam.gserviceaccount.test".to_string(),
                private_key: String::new(),
            }),
            token_url: "http://127.0.0.1:1/token".to_string(),
            endpoint: "http://127.0.0.1:1".to_string(),
            token: Mutex::new(None),
        };
        let error = client
            .send(&message(PushPlatform::Ios))
            .await
            .expect_err("iOS is not an FCM target");
        assert!(matches!(error, SendError::Failed(ref reason) if reason.contains("APNs")));
    }

    #[test]
    fn an_inline_account_is_parsed_without_logging_the_key() {
        let raw = serde_json::json!({
            "project_id": "demo",
            "client_email": "relay@demo.iam.gserviceaccount.test",
            "private_key": "test-key-material",
        })
        .to_string();
        let account = load_account(&raw).expect("inline JSON");
        assert_eq!(account.project_id, "demo");
        assert!(!format!("{account:?}").contains("test-key-material"));
    }

    #[test]
    fn a_missing_file_is_an_error_not_a_silent_success() {
        let error = load_account("/nonexistent/h4rn-service-account.json").unwrap_err();
        assert!(error.contains("could not read"));
    }

    #[test]
    fn only_a_detail_that_names_the_token_unregistered_is_gone() {
        // A wrong endpoint or project id also answers 404, and FCM's own
        // top-level status can say NOT_FOUND for a token it still has.
        assert!(!is_unregistered(404, ""));
        assert!(!is_unregistered(404, "<html>not found</html>"));
        assert!(!is_unregistered(
            404,
            r#"{"error":{"code":404,"message":"Requested entity was not found.","status":"NOT_FOUND"}}"#
        ));
        assert!(!is_unregistered(
            400,
            r#"{"error":{"status":"UNREGISTERED"}}"#
        ));
        assert!(is_unregistered(
            400,
            r#"{"error":{"status":"NOT_FOUND","details":[{"errorCode":"UNREGISTERED"}]}}"#
        ));
        assert!(is_unregistered(
            404,
            r#"{"error":{"details":[{"errorCode":"UNREGISTERED"}]}}"#
        ));
        // A detail for another reason leaves the token alone.
        assert!(!is_unregistered(
            400,
            r#"{"error":{"details":[{"errorCode":"INVALID_ARGUMENT"}]}}"#
        ));
        assert!(!is_unregistered(429, "quota"));
        assert!(!is_unregistered(500, "boom"));
    }
}
