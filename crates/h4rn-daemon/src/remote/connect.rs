//! Authenticated h4rn-connect client used by the daemon.
//!
//! The daemon signs in with the OIDC device authorization grant, proves
//! possession of its Ed25519 key on every request (the relay's DPoP-style
//! profile), refreshes its short-lived token, and exposes just the control-plane
//! calls the daemon needs: hosts, addresses, pairings and audit ingest.
//!
//! No prompt, terminal byte or file content ever travels through this module;
//! those go through the Noise session and the relay, which cannot read them.

use std::path::PathBuf;
use std::sync::RwLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::Utc;
use h4rn_protocol::{
    AuditIngestRequest, ConnectDeviceKind, ConnectMeResponse, ConnectSessionTokens,
    DeviceGrantPollRequest, DeviceGrantPollResponse, DeviceGrantStartRequest,
    DeviceGrantStartResponse, DeviceGrantStatus, DeviceRegistration, MachineAddressesUpdate,
    MachineHostInput, MachineHostsUpdate, PairingClaimRequest, PairingClaimResponse,
    PairingCreateRequest, PairingCreateResponse, UsageSyncRequest, UsageSyncResponse,
};
use h4rn_transport::keys::DeviceIdentity;
use reqwest::Method;
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::credentials::Credentials;
use crate::hosts;

/// How early a token is refreshed before it expires.
const REFRESH_SKEW_SECS: i64 = 60;

/// What the terminal shows while the user approves a login.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceGrantSummary {
    /// Where to enter the code.
    pub verification_uri: String,
    /// Same URL with the code pre-filled, when the provider offers it.
    pub verification_uri_complete: Option<String>,
    /// The code to type.
    pub user_code: String,
    /// Seconds until the grant expires.
    pub expires_in: i64,
    /// Minimum poll interval in seconds.
    pub interval: i64,
}

/// Result of a completed login.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginOutcome {
    /// Signed-in user, when the provider released an email.
    pub email: Option<String>,
    /// This daemon's connect device id.
    pub device_id: String,
    /// The machine the daemon owns.
    pub machine_id: String,
}

/// The daemon's connect session and API client.
pub struct ConnectClient {
    data_dir: PathBuf,
    identity: DeviceIdentity,
    http: reqwest::Client,
    credentials: RwLock<Option<Credentials>>,
    connect_url: RwLock<Option<String>>,
}

impl std::fmt::Debug for ConnectClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConnectClient")
            .field("data_dir", &self.data_dir)
            .field("logged_in", &self.is_logged_in())
            .finish_non_exhaustive()
    }
}

impl ConnectClient {
    /// Loads persisted credentials and builds the HTTP client.
    pub fn new(data_dir: impl Into<PathBuf>, identity: DeviceIdentity) -> Result<Self> {
        let data_dir = data_dir.into();
        let credentials = Credentials::load(&data_dir)?;
        let connect_url = credentials
            .as_ref()
            .map(|credentials| credentials.connect_url.clone());
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .context("build the connect HTTP client")?;
        Ok(Self {
            data_dir,
            identity,
            http,
            credentials: RwLock::new(credentials),
            connect_url: RwLock::new(connect_url),
        })
    }

    /// This daemon's transport identity, reused as its proof key.
    pub fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }

    /// The current credentials, if signed in.
    pub fn credentials(&self) -> Option<Credentials> {
        self.credentials
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Whether the daemon has a stored session.
    pub fn is_logged_in(&self) -> bool {
        self.credentials().is_some()
    }

    /// The machine id, when signed in as a daemon.
    pub fn machine_id(&self) -> Option<String> {
        self.credentials().and_then(|credentials| {
            (!credentials.machine_id.is_empty()).then_some(credentials.machine_id)
        })
    }

    /// Sets the connect URL used before the first login.
    pub fn set_connect_url(&self, url: &str) {
        *self
            .connect_url
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(url.trim_end_matches('/').to_owned());
    }

    fn base_url(&self) -> Result<String> {
        self.credentials()
            .map(|credentials| credentials.connect_url.trim_end_matches('/').to_owned())
            .or_else(|| {
                self.connect_url
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone()
            })
            .ok_or_else(|| anyhow::anyhow!("no connect URL: run `h4rnd login --connect <url>`"))
    }

    // ------------------------------------------------------------------
    // Device authorization grant
    // ------------------------------------------------------------------

    /// Starts a device grant for this daemon.
    pub async fn start_device_grant(
        &self,
        connect_url: &str,
        provider: &str,
        machine_name: Option<&str>,
    ) -> Result<(String, DeviceGrantSummary)> {
        self.start_device_grant_as(
            connect_url,
            provider,
            ConnectDeviceKind::Daemon,
            machine_name,
        )
        .await
    }

    /// Starts a device grant for any device kind.
    ///
    /// The daemon registers as [`ConnectDeviceKind::Daemon`]; a native client
    /// registers as desktop, mobile or web. Only a non-daemon device may open a
    /// relay session.
    pub async fn start_device_grant_as(
        &self,
        connect_url: &str,
        provider: &str,
        kind: ConnectDeviceKind,
        machine_name: Option<&str>,
    ) -> Result<(String, DeviceGrantSummary)> {
        let base = connect_url.trim_end_matches('/');
        self.set_connect_url(base);
        let public = self.identity.public();
        let platform = if let Some(distro) = hosts::wsl_distro_name() {
            format!("wsl:{distro}")
        } else {
            std::env::consts::OS.to_owned()
        };
        let name = machine_name
            .map(ToOwned::to_owned)
            .unwrap_or_else(hosts::hostname);
        let request = DeviceGrantStartRequest {
            provider: provider.to_owned(),
            device: DeviceRegistration {
                name: name.clone(),
                kind,
                proof_public_key: URL_SAFE_NO_PAD.encode(public.signing_public),
                noise_static_public_key: Some(URL_SAFE_NO_PAD.encode(public.noise_public)),
                platform: Some(platform),
                machine_name: Some(name),
            },
        };
        let response = self
            .http
            .post(format!("{base}/v1/auth/device/start"))
            .json(&request)
            .send()
            .await
            .context("start the device authorization grant")?;
        let start: DeviceGrantStartResponse = decode(response, "start the device grant").await?;
        Ok((
            start.grant_id,
            DeviceGrantSummary {
                verification_uri: start.verification_uri,
                verification_uri_complete: start.verification_uri_complete,
                user_code: start.user_code,
                expires_in: start.expires_in,
                interval: start.interval,
            },
        ))
    }

    /// Polls a device grant once.
    pub async fn poll_device_grant(&self, grant_id: &str) -> Result<DeviceGrantPollResponse> {
        let base = self.base_url()?;
        let response = self
            .http
            .post(format!("{base}/v1/auth/device/poll"))
            .json(&DeviceGrantPollRequest {
                grant_id: grant_id.to_owned(),
            })
            .send()
            .await
            .context("poll the device authorization grant")?;
        decode(response, "poll the device grant").await
    }

    /// Runs a full device grant, calling `on_code` once the user must act.
    pub async fn login(
        &self,
        connect_url: &str,
        provider: &str,
        machine_name: Option<&str>,
        on_code: impl FnMut(&DeviceGrantSummary),
    ) -> Result<LoginOutcome> {
        self.login_as(
            connect_url,
            provider,
            ConnectDeviceKind::Daemon,
            machine_name,
            on_code,
        )
        .await
    }

    /// Runs a full device grant for a specific device kind.
    pub async fn login_as(
        &self,
        connect_url: &str,
        provider: &str,
        kind: ConnectDeviceKind,
        machine_name: Option<&str>,
        mut on_code: impl FnMut(&DeviceGrantSummary),
    ) -> Result<LoginOutcome> {
        let (grant_id, summary) = self
            .start_device_grant_as(connect_url, provider, kind, machine_name)
            .await?;
        on_code(&summary);
        let mut interval = Duration::from_secs(summary.interval.max(1) as u64);
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(summary.expires_in.max(1) as u64);
        loop {
            if tokio::time::Instant::now() >= deadline {
                bail!("the device code expired before it was approved");
            }
            let poll = self.poll_device_grant(&grant_id).await?;
            match poll.status {
                DeviceGrantStatus::Approved => {
                    let tokens = poll
                        .tokens
                        .ok_or_else(|| anyhow::anyhow!("approved grant returned no tokens"))?;
                    let device = poll
                        .device
                        .ok_or_else(|| anyhow::anyhow!("approved grant returned no device"))?;
                    let user = poll.user;
                    // Only a daemon owns a machine; a desktop, mobile or web
                    // client signs in without one.
                    if kind == ConnectDeviceKind::Daemon && device.machine_id.is_none() {
                        bail!("the relay did not assign this daemon a machine");
                    }
                    let machine_id = device.machine_id.clone().unwrap_or_default();
                    let credentials = Credentials {
                        connect_url: connect_url.trim_end_matches('/').to_owned(),
                        user_id: user
                            .as_ref()
                            .map(|user| user.id.clone())
                            .unwrap_or_default(),
                        device_id: device.id.clone(),
                        machine_id: machine_id.clone(),
                        access_token: tokens.access_token.clone(),
                        refresh_token: tokens.refresh_token.clone(),
                        expires_at: Utc::now().timestamp() + tokens.expires_in,
                        token_type: tokens.token_type.clone(),
                    };
                    credentials.save(&self.data_dir)?;
                    *self
                        .credentials
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(credentials);
                    return Ok(LoginOutcome {
                        email: user.and_then(|user| user.email),
                        device_id: device.id,
                        machine_id,
                    });
                }
                DeviceGrantStatus::Pending => {}
                DeviceGrantStatus::SlowDown => {
                    let retry = poll.retry_after.unwrap_or(5).max(1) as u64;
                    interval = Duration::from_secs(retry);
                }
                DeviceGrantStatus::Denied => bail!("the login request was denied"),
                DeviceGrantStatus::Expired => bail!("the device code expired"),
            }
            tokio::time::sleep(interval).await;
        }
    }

    // ------------------------------------------------------------------
    // Tokens
    // ------------------------------------------------------------------

    /// Refreshes the session tokens, replacing the stored pair.
    pub async fn refresh(&self) -> Result<ConnectSessionTokens> {
        let credentials = self
            .credentials()
            .ok_or_else(|| anyhow::anyhow!("not signed in"))?;
        let base = credentials.connect_url.trim_end_matches('/');
        let response = self
            .http
            .post(format!("{base}/v1/auth/refresh"))
            .json(&h4rn_protocol::RefreshRequest {
                refresh_token: credentials.refresh_token.clone(),
            })
            .send()
            .await
            .context("refresh the connect session")?;
        let tokens: ConnectSessionTokens = decode(response, "refresh the session").await?;
        let refreshed = Credentials {
            access_token: tokens.access_token.clone(),
            refresh_token: tokens.refresh_token.clone(),
            expires_at: Utc::now().timestamp() + tokens.expires_in,
            token_type: tokens.token_type.clone(),
            ..credentials
        };
        refreshed.save(&self.data_dir)?;
        *self
            .credentials
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(refreshed);
        Ok(tokens)
    }

    /// A valid access token, refreshing when it is close to expiry.
    pub async fn fresh_access_token(&self) -> Result<String> {
        let credentials = self
            .credentials()
            .ok_or_else(|| anyhow::anyhow!("not signed in"))?;
        if credentials.expires_within(REFRESH_SKEW_SECS) {
            return Ok(self.refresh().await?.access_token);
        }
        Ok(credentials.access_token)
    }

    /// Signs out and removes the local credentials.
    pub async fn logout(&self) -> Result<()> {
        let Some(credentials) = self.credentials() else {
            return Ok(());
        };
        let logout = h4rn_protocol::LogoutRequest {
            refresh_token: Some(credentials.refresh_token.clone()),
        };
        let result = self
            .authorized_empty(Method::POST, "/v1/auth/logout", Some(&logout))
            .await;
        Credentials::remove(&self.data_dir)?;
        *self
            .credentials
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        // A revoked or expired session already means the goal state; only a
        // transport failure is worth reporting.
        match result {
            Ok(()) => Ok(()),
            Err(error) if error.to_string().contains("status 401") => Ok(()),
            Err(error) => Err(error),
        }
    }

    // ------------------------------------------------------------------
    // Control plane
    // ------------------------------------------------------------------

    /// The signed-in user and devices.
    pub async fn me(&self) -> Result<ConnectMeResponse> {
        self.authorized_json(Method::GET, "/v1/me", None::<&()>)
            .await
    }

    /// The machines owned by the signed-in user.
    pub async fn machines(&self) -> Result<Vec<h4rn_protocol::MachineSummary>> {
        self.authorized_json(Method::GET, "/v1/machines", None::<&()>)
            .await
    }

    /// Builds the authenticated `/relay` WebSocket URL.
    ///
    /// The proof is in the query string because WebSocket clients cannot set
    /// arbitrary headers from a browser; the daemon is native but uses the same
    /// shape as the web client.
    pub async fn relay_websocket_url(&self) -> Result<String> {
        let base = self.base_url()?;
        let token = self.fresh_access_token().await?;
        let (proof, timestamp, nonce) = self.proof_headers(&Method::GET, "/relay");
        let mut url = url::Url::parse(&format!("{base}/relay"))
            .with_context(|| format!("invalid connect URL {base}"))?;
        // h4rn-connect is configured with an `http(s)` public base URL, while a
        // WebSocket client only accepts `ws(s)`. Map the scheme without changing
        // the host or path the proof was computed over.
        let scheme = match url.scheme() {
            "https" => Some("wss"),
            "http" => Some("ws"),
            _ => None,
        };
        if let Some(scheme) = scheme {
            url.set_scheme(scheme)
                .map_err(|_| anyhow::anyhow!("could not build a WebSocket URL from {base}"))?;
        }
        if !matches!(url.scheme(), "ws" | "wss") {
            bail!("the connect URL must be http or https, got {base}");
        }
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("access_token", &token);
            query.append_pair("proof", &proof);
            query.append_pair("timestamp", &timestamp.to_string());
            query.append_pair("nonce", &nonce);
        }
        Ok(url.to_string())
    }

    /// Creates a pairing for this daemon's machine.
    pub async fn create_pairing(&self) -> Result<PairingCreateResponse> {
        let machine_id = self
            .machine_id()
            .ok_or_else(|| anyhow::anyhow!("not signed in"))?;
        let request = PairingCreateRequest { machine_id };
        self.authorized_json(Method::POST, "/v1/pairings", Some(&request))
            .await
    }

    /// Claims a scanned pairing QR as this device.
    pub async fn claim_pairing(
        &self,
        request: PairingClaimRequest,
    ) -> Result<PairingClaimResponse> {
        self.authorized_json(Method::POST, "/v1/pairings/claim", Some(&request))
            .await
    }

    /// Revokes another device in the same account.
    pub async fn revoke_device(&self, device_id: &str) -> Result<()> {
        let path = format!("/v1/devices/{device_id}");
        self.authorized_empty(Method::DELETE, &path, None::<&()>)
            .await
    }

    /// Replaces the host list for this daemon's machine.
    pub async fn update_hosts(&self, hosts: Vec<MachineHostInput>) -> Result<()> {
        let machine_id = self
            .machine_id()
            .ok_or_else(|| anyhow::anyhow!("not signed in"))?;
        let path = format!("/v1/machines/{machine_id}/hosts");
        self.authorized_empty(Method::PUT, &path, Some(&MachineHostsUpdate { hosts }))
            .await
    }

    /// Replaces the advertised address list for this daemon's machine.
    pub async fn update_addresses(&self, addresses: Vec<String>) -> Result<()> {
        let machine_id = self
            .machine_id()
            .ok_or_else(|| anyhow::anyhow!("not signed in"))?;
        let path = format!("/v1/machines/{machine_id}/addresses");
        self.authorized_empty(
            Method::PUT,
            &path,
            Some(&MachineAddressesUpdate { addresses }),
        )
        .await
    }

    /// Mirrors one remote-action audit entry.
    pub async fn post_audit(&self, request: AuditIngestRequest) -> Result<()> {
        self.authorized_empty(Method::POST, "/v1/audit", Some(&request))
            .await
    }

    /// Publishes local skill versions to connect.
    pub async fn skills_push(
        &self,
        request: &h4rn_protocol::SkillSyncPush,
    ) -> Result<h4rn_protocol::SkillSyncPushResponse> {
        self.authorized_json(Method::POST, "/v1/skills/sync/push", Some(request))
            .await
    }

    /// Reads skill versions changed on another machine.
    pub async fn skills_pull(
        &self,
        request: &h4rn_protocol::SkillSyncPull,
    ) -> Result<h4rn_protocol::SkillSyncPullResponse> {
        self.authorized_json(Method::POST, "/v1/skills/sync/pull", Some(request))
            .await
    }

    /// Pushes one idempotent usage batch to connect.
    pub async fn post_usage(&self, request: &UsageSyncRequest) -> Result<UsageSyncResponse> {
        self.authorized_json(Method::POST, "/v1/usage", Some(request))
            .await
    }

    // ------------------------------------------------------------------
    // Proof-authenticated requests
    // ------------------------------------------------------------------

    fn proof_headers(&self, method: &Method, path: &str) -> (String, i64, String) {
        let timestamp = Utc::now().timestamp();
        let nonce = random_nonce();
        let canonical = format!(
            "h4rn-dpop-v1\n{}\n{}\n{timestamp}\n{nonce}",
            method.as_str().to_ascii_uppercase(),
            path
        );
        let signature = self.identity.sign(canonical.as_bytes());
        (URL_SAFE_NO_PAD.encode(signature), timestamp, nonce)
    }

    async fn send_once(
        &self,
        method: Method,
        path: &str,
        body: Option<&[u8]>,
        access_token: &str,
    ) -> Result<reqwest::Response> {
        let base = self.base_url()?;
        let (proof, timestamp, nonce) = self.proof_headers(&method, path);
        let mut request = self
            .http
            .request(method, format!("{base}{path}"))
            .bearer_auth(access_token)
            .header("x-h4rn-proof", proof)
            .header("x-h4rn-timestamp", timestamp.to_string())
            .header("x-h4rn-nonce", nonce);
        if let Some(body) = body {
            request = request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.to_vec());
        }
        request.send().await.context("call h4rn-connect")
    }

    async fn authorized(
        &self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<reqwest::Response> {
        let mut refreshed = false;
        loop {
            let token = self.fresh_access_token().await?;
            let response = self
                .send_once(method.clone(), path, body.as_deref(), &token)
                .await?;
            if response.status() == reqwest::StatusCode::UNAUTHORIZED && !refreshed {
                refreshed = true;
                self.refresh().await?;
                continue;
            }
            if !response.status().is_success() {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                bail!("h4rn-connect returned status {status}: {text}");
            }
            return Ok(response);
        }
    }

    async fn authorized_json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&impl Serialize>,
    ) -> Result<T> {
        let bytes = body
            .map(serde_json::to_vec)
            .transpose()
            .context("serialise request body")?;
        let response = self.authorized(method, path, bytes).await?;
        response
            .json::<T>()
            .await
            .context("decode the h4rn-connect response")
    }

    async fn authorized_empty(
        &self,
        method: Method,
        path: &str,
        body: Option<&impl Serialize>,
    ) -> Result<()> {
        let bytes = body
            .map(serde_json::to_vec)
            .transpose()
            .context("serialise request body")?;
        self.authorized(method, path, bytes).await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl h4rn_skills::SkillSyncApi for ConnectClient {
    async fn push(
        &self,
        request: h4rn_protocol::SkillSyncPush,
    ) -> std::result::Result<h4rn_protocol::SkillSyncPushResponse, h4rn_skills::SkillError> {
        self.skills_push(&request)
            .await
            .map_err(|error| h4rn_skills::SkillError::Sync(error.to_string()))
    }

    async fn pull(
        &self,
        request: h4rn_protocol::SkillSyncPull,
    ) -> std::result::Result<h4rn_protocol::SkillSyncPullResponse, h4rn_skills::SkillError> {
        self.skills_pull(&request)
            .await
            .map_err(|error| h4rn_skills::SkillError::Sync(error.to_string()))
    }
}

async fn decode<T: DeserializeOwned>(response: reqwest::Response, what: &str) -> Result<T> {
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        bail!("{what}: h4rn-connect returned status {status}: {text}");
    }
    response
        .json::<T>()
        .await
        .with_context(|| format!("decode the response from {what}"))
}

fn random_nonce() -> String {
    hex::encode(rand::random::<[u8; 16]>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proof_binds_method_path_timestamp_and_nonce() {
        let identity = DeviceIdentity::generate().unwrap();
        let client = ConnectClient::new("/tmp/h4rn-connect-test", identity.clone()).unwrap();
        let (signature, timestamp, nonce) = client.proof_headers(&Method::GET, "/v1/me");
        let canonical = format!("h4rn-dpop-v1\nGET\n/v1/me\n{timestamp}\n{nonce}");
        let bytes = URL_SAFE_NO_PAD.decode(signature).unwrap();
        let signature: [u8; 64] = bytes.try_into().unwrap();
        DeviceIdentity::verify(&identity.public(), canonical.as_bytes(), &signature).unwrap();
    }

    #[test]
    fn a_client_without_credentials_is_signed_out() {
        let dir = tempfile::tempdir().unwrap();
        let client = ConnectClient::new(dir.path(), DeviceIdentity::generate().unwrap()).unwrap();
        assert!(!client.is_logged_in());
        assert!(client.machine_id().is_none());
    }
}
