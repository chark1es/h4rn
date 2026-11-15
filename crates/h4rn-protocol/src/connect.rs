//! Client-facing types for the h4rn-connect relay (ADR-010, ADR-011, ADR-018).
//!
//! These types are the JSON contract between the desktop, mobile and web
//! clients and `h4rn-connect`. The daemon also speaks them for pairing and for
//! its relay control channel. They live in `h4rn-protocol` for the same reason
//! every other wire type does (ADR-002): one definition, generated TypeScript.
//!
//! # Relay framing
//!
//! The relay is deliberately dumb. A [`RelayFrame`] carries an opaque payload
//! for one Noise session; connect routes it by `conn_id` and never inspects it
//! (ADR-010). Only [`RelayFrameKind::Open`] has a payload the relay parses: it
//! names the target machine so connect can authorise and route the session.
//!
//! Payload bytes travel as base64 in JSON because control frames and data
//! frames share one WebSocket. The relay is the fallback path, so the
//! encoding overhead is paid only when the direct and tailnet paths are
//! unavailable (ADR-009).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Base64 (URL-safe, no padding) for opaque byte payloads.
pub mod relay_base64 {
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use serde::{Deserialize, Deserializer, Serializer};

    /// Serialises bytes as URL-safe base64 without padding.
    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&URL_SAFE_NO_PAD.encode(bytes))
    }

    /// Deserialises URL-safe base64 without padding into bytes.
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let value = String::deserialize(deserializer)?;
        URL_SAFE_NO_PAD
            .decode(value.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

/// Which relay frame this is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum RelayFrameKind {
    /// Ask the relay to route a new session to a machine. The payload is a
    /// JSON [`RelayOpen`] naming the machine.
    Open,
    /// Opaque Noise bytes for an established session.
    Data,
    /// Tear the session down. The payload is optional and opaque to the relay.
    Close,
    /// Round-trip probe. The relay echoes it back to the sender.
    Ping,
}

/// A single relay frame.
///
/// `conn_id` is chosen by the client and must be unique among the device's
/// live sessions. The relay uses it as the routing key and writes no payload
/// to disk or logs (ADR-010).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct RelayFrame {
    /// Routing key for one client/daemon session.
    pub conn_id: String,
    /// Frame kind.
    pub kind: RelayFrameKind,
    /// Opaque bytes (base64 in JSON). Parsed only for [`RelayFrameKind::Open`].
    #[serde(with = "relay_base64")]
    #[ts(type = "string")]
    pub payload: Vec<u8>,
}

impl RelayFrame {
    /// Builds a frame carrying opaque bytes.
    pub fn data(conn_id: impl Into<String>, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            conn_id: conn_id.into(),
            kind: RelayFrameKind::Data,
            payload: payload.into(),
        }
    }

    /// Builds a close frame with no payload.
    pub fn close(conn_id: impl Into<String>) -> Self {
        Self {
            conn_id: conn_id.into(),
            kind: RelayFrameKind::Close,
            payload: Vec::new(),
        }
    }

    /// Builds an open frame for `machine_id` on the default client route.
    pub fn open(conn_id: impl Into<String>, machine_id: impl Into<String>) -> Self {
        Self::open_service(conn_id, machine_id, RelayService::Secure)
    }

    /// Builds an open frame for `machine_id` on `service`.
    ///
    /// A daemon-to-daemon MCP session must use [`RelayService::SecureMcp`] so
    /// the target daemon routes it to `/secure-mcp` and never to the client
    /// `/secure` route.
    pub fn open_service(
        conn_id: impl Into<String>,
        machine_id: impl Into<String>,
        service: RelayService,
    ) -> Self {
        let payload = serde_json::to_vec(&RelayOpen {
            machine_id: machine_id.into(),
            device_id: None,
            service,
        })
        .expect("RelayOpen is always serialisable");
        Self {
            conn_id: conn_id.into(),
            kind: RelayFrameKind::Open,
            payload,
        }
    }
}

/// Which route an opened relay session is for.
///
/// The relay forwards the value unchanged; the target daemon chooses the
/// listener. `Secure` is the original client protocol route. `SecureMcp` is
/// the peer-only MCP tunnel route, which refuses user device keys.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum RelayService {
    /// The client `/secure` route.
    #[default]
    Secure,
    /// The peer-only `/secure-mcp` route.
    SecureMcp,
}

impl RelayService {
    /// Stable spelling used in audits and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Secure => "secure",
            Self::SecureMcp => "secure_mcp",
        }
    }
}

/// The payload of a [`RelayFrameKind::Open`] frame.
///
/// A client sends `machine_id` and the service route; the relay fills
/// `device_id` from the authenticated socket before forwarding the open to the
/// daemon, so a client can never impersonate another device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct RelayOpen {
    /// Machine the session targets.
    pub machine_id: String,
    /// Device opening the session. Set by the relay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    /// Route the session must reach. Defaults to `secure` for older clients.
    #[serde(default)]
    pub service: RelayService,
}

/// Events the relay sends to a connected device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RelayServerEvent {
    /// A routed frame, either from a peer or from the relay itself.
    Frame {
        /// The frame.
        frame: RelayFrame,
    },
    /// Machine presence changed. Broadcast to every device of the owner.
    Presence {
        /// Presence details.
        presence: PresenceUpdate,
    },
    /// A device was paired with this machine. Sent to the daemon.
    DevicePaired {
        /// Machine id.
        machine_id: String,
        /// The newly paired device, including its Noise static public key.
        device: ConnectDevice,
    },
    /// A device was revoked. Sent to every daemon the device was paired with.
    DeviceRevoked {
        /// Machine id.
        machine_id: String,
        /// Revoked device id.
        device_id: String,
    },
    /// The complete paired-device list, sent to a daemon when it connects.
    Snapshot {
        /// Machine id.
        machine_id: String,
        /// Every live paired device.
        devices: Vec<ConnectDevice>,
    },
    /// A relay-level error, scoped to a session when `conn_id` is present.
    Error {
        /// Session the error refers to, when applicable.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conn_id: Option<String>,
        /// The error body.
        error: ConnectErrorBody,
    },
}

/// The fixed error envelope every connect endpoint returns.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ConnectErrorBody {
    /// Stable machine-readable code, e.g. `pairing_expired`.
    pub code: String,
    /// Human-readable message.
    pub message: String,
    /// What the caller can do about it.
    pub hint: Option<String>,
}

impl ConnectErrorBody {
    /// Builds an error body.
    pub fn new(code: impl Into<String>, message: impl Into<String>, hint: Option<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            hint,
        }
    }
}

/// What kind of client a device is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ConnectDeviceKind {
    /// A daemon, one per machine (or per WSL distribution).
    Daemon,
    /// The Tauri desktop application.
    Desktop,
    /// The Tauri mobile application.
    Mobile,
    /// The browser build.
    Web,
}

impl ConnectDeviceKind {
    /// The database/wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Daemon => "daemon",
            Self::Desktop => "desktop",
            Self::Mobile => "mobile",
            Self::Web => "web",
        }
    }

    /// Parses a stored spelling.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "daemon" => Some(Self::Daemon),
            "desktop" => Some(Self::Desktop),
            "mobile" => Some(Self::Mobile),
            "web" => Some(Self::Web),
            _ => None,
        }
    }
}

/// A paired device as the API exposes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ConnectDevice {
    /// Device id (UUID v7).
    pub id: String,
    /// Device kind.
    pub kind: ConnectDeviceKind,
    /// Human display name.
    pub name: String,
    /// Machine this device belongs to: the daemon's machine, or the machine a
    /// client is paired with.
    pub machine_id: Option<String>,
    /// Ed25519 proof-of-possession key, base64url.
    pub proof_public_key: String,
    /// X25519 Noise static public key, base64url. Required for daemons and for
    /// clients that want the relay fallback.
    pub noise_static_public_key: Option<String>,
    /// Free-form platform label (`linux`, `macos`, `windows`, `ios`, `android`,
    /// `web`).
    pub platform: Option<String>,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last relay connection, if any.
    pub last_seen: Option<DateTime<Utc>>,
    /// Revocation time, if revoked.
    pub revoked_at: Option<DateTime<Utc>>,
    /// Scopes the device was paired with on this machine. An empty list is a
    /// record without recorded authority, which the daemon reads as `view`
    /// only: missing authorisation is never widened.
    #[serde(default)]
    pub scopes: Vec<PairScope>,
}

/// A user account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ConnectUser {
    /// User id (UUID v7).
    pub id: String,
    /// Primary email, when the provider released one.
    pub email: Option<String>,
    /// Display name.
    pub display_name: Option<String>,
    /// Avatar URL.
    pub avatar_url: Option<String>,
}

/// h4rn-issued session tokens.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ConnectSessionTokens {
    /// Short-lived EdDSA access token, bound to the device key by DPoP-style
    /// proof on every request.
    pub access_token: String,
    /// Rotating refresh token. Persist the successor before using it: reuse
    /// outside the grace window revokes the family (ADR-011).
    pub refresh_token: String,
    /// Access-token lifetime in seconds.
    pub expires_in: i64,
    /// Always `Bearer`.
    pub token_type: String,
}

/// Response to `GET /v1/me`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ConnectMeResponse {
    /// The signed-in user.
    pub user: ConnectUser,
    /// The caller's devices.
    pub devices: Vec<ConnectDevice>,
}

/// Whether a machine host is the primary OS or a WSL distribution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum MachineHostKind {
    /// The machine's own operating system.
    Host,
    /// A WSL distribution reached through the Windows host's daemon.
    Wsl,
}

/// One addressable host on a machine (ADR-019).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct MachineHost {
    /// Host id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Host kind.
    pub kind: MachineHostKind,
    /// WSL distribution name, when `kind = wsl`.
    pub distro: Option<String>,
}

/// One host a daemon reports for its machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct MachineHostInput {
    /// Display name.
    pub name: String,
    /// Host kind.
    pub kind: MachineHostKind,
    /// WSL distribution name, when `kind = wsl`.
    pub distro: Option<String>,
}

/// Body of `PUT /v1/machines/:id/hosts`.
///
/// A daemon replaces the full list on each report: WSL distributions come and
/// go, and a merge would leave stale hosts behind (ADR-019).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct MachineHostsUpdate {
    /// Complete host list for the machine.
    pub hosts: Vec<MachineHostInput>,
}

/// A machine and its live presence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct MachineSummary {
    /// Machine id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Whether the daemon currently has a relay connection.
    pub online: bool,
    /// Last daemon activity.
    pub last_seen: Option<DateTime<Utc>>,
    /// The daemon device that owns this machine.
    pub daemon_device_id: Option<String>,
    /// Addressable hosts, including WSL distributions.
    pub hosts: Vec<MachineHost>,
    /// Advertised connection candidates (LAN, tailnet, relay), most preferred
    /// first. A client probes them in this order after the relay tells it the
    /// machine is online.
    #[serde(default)]
    pub addresses: Vec<String>,
}

/// Body of `PUT /v1/machines/:id/addresses`.
///
/// The daemon replaces the full list on each report: a laptop changes networks
/// and a tailnet address appears or disappears, so a merge would leave stale
/// candidates that clients keep probing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct MachineAddressesUpdate {
    /// Complete address list for the machine, preferred first.
    pub addresses: Vec<String>,
}

/// Body of `POST /v1/audit`.
///
/// The daemon writes the entry locally first and mirrors it here when the relay
/// is reachable. Metadata must never contain prompts, payloads or secrets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct AuditIngestRequest {
    /// Action name, e.g. `thread.created`.
    pub action: String,
    /// Target type, e.g. `thread`.
    pub target_type: Option<String>,
    /// Target id.
    pub target_id: Option<String>,
    /// Extra structured metadata. Never contains payloads or secrets.
    #[serde(default)]
    pub metadata: serde_json::Value,
    /// When it happened; the relay uses its own clock when absent.
    pub at: Option<DateTime<Utc>>,
}

/// A presence change broadcast to the owner's devices.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PresenceUpdate {
    /// Machine whose presence changed.
    pub machine_id: String,
    /// Online or offline.
    pub online: bool,
    /// When the daemon was last seen.
    pub last_seen: Option<DateTime<Utc>>,
}

/// What a paired device may do on a machine.
///
/// A pairing grants a set of scopes, connect records them next to the
/// device-machine link, signs them into the device's access token, and relays
/// them to the daemon, which enforces them per request. `view` is always held.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum PairScope {
    /// Read threads, diffs, usage and status. Always granted.
    View,
    /// Start and message threads, answer approvals and control runs.
    Approve,
    /// Attach to and type into terminals.
    Terminal,
    /// Browse, read and edit workspace files.
    Files,
    /// Change configuration: MCP, skills, proxy, forge accounts and policy.
    Admin,
}

impl PairScope {
    /// Every scope.
    pub const ALL: [Self; 5] = [
        Self::View,
        Self::Approve,
        Self::Terminal,
        Self::Files,
        Self::Admin,
    ];

    /// The set granted when a pairing request names none: everything but
    /// `admin`.
    pub const DEFAULT: [Self; 4] = [Self::View, Self::Approve, Self::Terminal, Self::Files];

    /// The least authority a pairing gets when it names no scopes: read and
    /// reply. Terminal, files and admin need an explicit request.
    pub const MINIMAL: [Self; 2] = [Self::View, Self::Approve];

    /// The database/wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::View => "view",
            Self::Approve => "approve",
            Self::Terminal => "terminal",
            Self::Files => "files",
            Self::Admin => "admin",
        }
    }

    /// Parses a stored spelling.
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|scope| scope.as_str() == value)
    }

    /// Sorts, de-duplicates and adds the always-held `view`.
    pub fn normalize(scopes: impl IntoIterator<Item = Self>) -> Vec<Self> {
        let mut set: std::collections::BTreeSet<Self> = scopes.into_iter().collect();
        set.insert(Self::View);
        set.into_iter().collect()
    }
}

/// A relay-provisioned device the host has not admitted yet.
///
/// This covers typed codes and QR pairings: in both the host learns the device
/// key from the relay, so it must confirm before the key is allowlisted. The
/// daemon computes `sas` from its own key and the device key connect gave it.
/// The person compares it with the string the new device shows after claiming;
/// a mismatch means the relay substituted a key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PairingConfirmation {
    /// Connect device id of the new device.
    pub device_id: String,
    /// Its display name.
    pub device_name: String,
    /// Twelve digits in groups of four, derived in Rust.
    pub sas: String,
    /// When the daemon first saw the device.
    pub paired_at: DateTime<Utc>,
}

/// Alphabet of the pairing short code: Crockford base32 (no I, L, O, U).
pub const SHORT_CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// Characters in a pairing short code (40 bits).
pub const SHORT_CODE_LEN: usize = 8;

/// Normalises a typed short code to its canonical 8 characters.
///
/// Upper-cases, drops hyphens and whitespace and applies Crockford's folding
/// (`I`, `L` read as `1`; `O` reads as `0`). Returns `None` when the result is
/// not exactly [`SHORT_CODE_LEN`] alphabet characters.
pub fn normalize_short_code(input: &str) -> Option<String> {
    let mut out = String::with_capacity(SHORT_CODE_LEN);
    for ch in input.chars() {
        if ch == '-' || ch.is_whitespace() {
            continue;
        }
        let ch = match ch.to_ascii_uppercase() {
            'I' | 'L' => '1',
            'O' => '0',
            other => other,
        };
        if !ch.is_ascii() || !SHORT_CODE_ALPHABET.contains(&(ch as u8)) {
            return None;
        }
        out.push(ch);
    }
    (out.len() == SHORT_CODE_LEN).then_some(out)
}

/// Pairing state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum PairingState {
    /// Waiting for a device to claim it.
    Pending,
    /// Claimed exactly once.
    Claimed,
    /// Expired before being claimed.
    Expired,
    /// Revoked by the owner.
    Revoked,
}

/// A pairing record as the API exposes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PairingInfo {
    /// Pairing id.
    pub id: String,
    /// Target machine.
    pub machine_id: String,
    /// Current state.
    pub state: PairingState,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Expiry time (default five minutes).
    pub expires_at: DateTime<Utc>,
    /// Device that claimed it, once claimed.
    pub claimed_by_device_id: Option<String>,
    /// Scopes the claiming device will hold. Always contains `view`.
    #[serde(default)]
    pub scopes: Vec<PairScope>,
}

/// The QR payload a daemon prints and a new device scans.
///
/// Wire form:
/// `h4rn://pair?c=<connect url>&m=<machine id>&k=<daemon noise pubkey>&s=<secret>&e=<expiry unix secs>`
///
/// The secret is the trust anchor for the first Noise handshake: connect only
/// relays it, and never learns the daemon's Noise private key (ADR-010).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PairingPayload {
    /// Public base URL of the connect deployment.
    pub connect_url: String,
    /// Machine id.
    pub machine_id: String,
    /// Daemon Noise static public key, base64url.
    pub daemon_noise_public_key: String,
    /// One-time pairing secret, base64url.
    pub secret: String,
    /// Expiry.
    pub expires_at: DateTime<Utc>,
    /// Scopes the pairing grants, as a hint for the confirmation screen. Not
    /// authoritative: connect records and enforces the real set.
    #[serde(default)]
    pub scopes: Vec<PairScope>,
}

impl PairingPayload {
    /// Renders the `h4rn://pair?...` string.
    pub fn to_url(&self) -> String {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query.append_pair("c", &self.connect_url);
        query.append_pair("m", &self.machine_id);
        query.append_pair("k", &self.daemon_noise_public_key);
        query.append_pair("s", &self.secret);
        query.append_pair("e", &self.expires_at.timestamp().to_string());
        if !self.scopes.is_empty() {
            let hint: Vec<&str> = self.scopes.iter().map(|scope| scope.as_str()).collect();
            query.append_pair("p", &hint.join(","));
        }
        format!("h4rn://pair?{}", query.finish())
    }

    /// Parses an `h4rn://pair?...` string.
    pub fn parse(value: &str) -> Result<Self, PairingPayloadError> {
        let url = url::Url::parse(value).map_err(|_| PairingPayloadError::InvalidUrl)?;
        if url.scheme() != "h4rn" || url.host_str() != Some("pair") {
            return Err(PairingPayloadError::InvalidScheme);
        }
        let mut params: BTreeMap<String, String> = BTreeMap::new();
        for (key, value) in url.query_pairs() {
            params.insert(key.into_owned(), value.into_owned());
        }
        let take = |key: &str| {
            params
                .get(key)
                .cloned()
                .ok_or(PairingPayloadError::MissingField(key.to_string()))
        };
        let expires = take("e")?
            .parse::<i64>()
            .map_err(|_| PairingPayloadError::InvalidExpiry)?;
        let expires_at =
            DateTime::from_timestamp(expires, 0).ok_or(PairingPayloadError::InvalidExpiry)?;
        Ok(Self {
            connect_url: take("c")?,
            machine_id: take("m")?,
            daemon_noise_public_key: take("k")?,
            secret: take("s")?,
            expires_at,
            scopes: params
                .get("p")
                .map(|hint| hint.split(',').filter_map(PairScope::parse).collect())
                .unwrap_or_default(),
        })
    }
}

/// Why a pairing payload could not be parsed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PairingPayloadError {
    /// Not a URL.
    #[error("pairing payload is not a URL")]
    InvalidUrl,
    /// Not `h4rn://pair`.
    #[error("pairing payload must use the h4rn://pair scheme")]
    InvalidScheme,
    /// A required query parameter is missing.
    #[error("pairing payload is missing `{0}`")]
    MissingField(String),
    /// `e` is not a Unix timestamp.
    #[error("pairing payload expiry is invalid")]
    InvalidExpiry,
}

/// Response to `POST /v1/pairings`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PairingCreateResponse {
    /// The stored pairing.
    pub pairing: PairingInfo,
    /// The string to render as a QR code.
    pub qr: String,
    /// Single-use 8-character Crockford base32 code (no separator), a typed
    /// alternative to the QR. Shown once: only a hash is stored.
    pub short_code: String,
}

/// Body of `POST /v1/pairings`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PairingCreateRequest {
    /// Machine to pair a new device with.
    pub machine_id: String,
    /// Scopes to grant the paired device. `view` is always added. Omitted
    /// means [`PairScope::DEFAULT`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub scopes: Option<Vec<PairScope>>,
}

/// Body of `POST /v1/pairings/claim-code`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PairingClaimCodeRequest {
    /// The typed short code; case, hyphens and spaces are ignored.
    pub code: String,
    /// Claiming device's Ed25519 proof key, base64url.
    pub device_public_key: String,
    /// Claiming device's X25519 Noise static key, base64url.
    pub noise_static_public_key: Option<String>,
    /// Optional new display name for the claiming device.
    pub device_name: Option<String>,
}

/// Body of `POST /v1/pairings/claim`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PairingClaimRequest {
    /// Scanned `h4rn://pair?...` payload.
    pub payload: String,
    /// Claiming device's Ed25519 proof key, base64url. It must match the key
    /// the authenticated device registered with.
    pub device_public_key: String,
    /// Claiming device's X25519 Noise static key, base64url.
    pub noise_static_public_key: Option<String>,
    /// Optional new display name for the claiming device.
    pub device_name: Option<String>,
}

/// Response to `POST /v1/pairings/claim`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PairingClaimResponse {
    /// The claimed pairing.
    pub pairing: PairingInfo,
    /// The machine, now reachable by the device.
    pub machine: MachineSummary,
    /// The daemon's Noise static public key, base64url. A QR carries it; a
    /// typed code does not, so the response does.
    pub daemon_noise_public_key: String,
}

/// Device identity a login flow will register.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct DeviceRegistration {
    /// Human display name.
    pub name: String,
    /// Device kind.
    pub kind: ConnectDeviceKind,
    /// Ed25519 proof key, base64url.
    pub proof_public_key: String,
    /// X25519 Noise static key, base64url.
    pub noise_static_public_key: Option<String>,
    /// Platform label.
    pub platform: Option<String>,
    /// For daemons: the machine name to create on first login.
    pub machine_name: Option<String>,
}

/// Body of `POST /v1/auth/:provider/start`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct LoginStartRequest {
    /// OIDC redirect URI. Must be one of the deployment's registered URIs.
    pub redirect_uri: String,
    /// Device to register or reuse after the callback.
    pub device: DeviceRegistration,
}

/// Response to `POST /v1/auth/:provider/start`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct LoginStartResponse {
    /// Provider id.
    pub provider: String,
    /// Where to send the browser.
    pub authorize_url: String,
    /// Opaque state value; also echoed by the provider.
    pub state: String,
    /// Seconds until the flow expires.
    pub expires_in: i64,
}

/// Body of `POST /v1/auth/:provider/complete`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct LoginCompleteRequest {
    /// State value from the authorize redirect.
    pub state: String,
    /// Authorization code from the provider.
    pub code: String,
    /// Redirect URI used for the authorize request. Must match the start.
    pub redirect_uri: String,
}

/// Response to a completed login.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct LoginCompleteResponse {
    /// h4rn-issued tokens.
    pub tokens: ConnectSessionTokens,
    /// The user.
    pub user: ConnectUser,
    /// The registered device.
    pub device: ConnectDevice,
}

/// Body of `POST /v1/auth/device/start`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct DeviceGrantStartRequest {
    /// Provider id.
    pub provider: String,
    /// Device to register when the grant is approved.
    pub device: DeviceRegistration,
}

/// Response to `POST /v1/auth/device/start`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct DeviceGrantStartResponse {
    /// Connect-side grant id used when polling.
    pub grant_id: String,
    /// Code the user types at the provider.
    pub user_code: String,
    /// Provider verification URL.
    pub verification_uri: String,
    /// Provider verification URL with the code pre-filled, when offered.
    pub verification_uri_complete: Option<String>,
    /// Seconds until the grant expires.
    pub expires_in: i64,
    /// Poll interval in seconds.
    pub interval: i64,
}

/// State of a device grant poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum DeviceGrantStatus {
    /// The user has not approved yet.
    Pending,
    /// Polling too fast; wait `retry_after` seconds.
    SlowDown,
    /// Approved; tokens are present.
    Approved,
    /// The user denied the request.
    Denied,
    /// The user code expired.
    Expired,
}

/// Body of `POST /v1/auth/device/poll`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct DeviceGrantPollRequest {
    /// Grant id from the start response.
    pub grant_id: String,
}

/// Response to `POST /v1/auth/device/poll`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct DeviceGrantPollResponse {
    /// Poll state.
    pub status: DeviceGrantStatus,
    /// Tokens, when `status = approved`.
    pub tokens: Option<ConnectSessionTokens>,
    /// User, when `status = approved`.
    pub user: Option<ConnectUser>,
    /// Device, when `status = approved`.
    pub device: Option<ConnectDevice>,
    /// Seconds to wait before polling again, when `status = slow_down`.
    pub retry_after: Option<i64>,
}

/// Body of `POST /v1/auth/refresh`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct RefreshRequest {
    /// Current refresh token.
    pub refresh_token: String,
}

/// Body of `POST /v1/auth/logout`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct LogoutRequest {
    /// Refresh token to revoke at the provider, when available.
    pub refresh_token: Option<String>,
}

/// A configured OIDC provider, safe to expose before login.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ProviderInfo {
    /// Stable provider id used in URLs.
    pub id: String,
    /// Display name for the login button.
    pub display_name: String,
    /// Issuer URL.
    pub issuer: String,
    /// Whether the provider advertises the device authorization grant.
    pub device_grant: bool,
    /// Redirect URIs this deployment accepts for the provider.
    pub redirect_uris: Vec<String>,
}

/// One audit entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct AuditEntry {
    /// Monotonic id, used as the page cursor.
    pub id: i64,
    /// Acting device, when the action came from one.
    pub actor_device_id: Option<String>,
    /// Action name, e.g. `device.paired`.
    pub action: String,
    /// Target type, e.g. `device`.
    pub target_type: Option<String>,
    /// Target id.
    pub target_id: Option<String>,
    /// Extra structured metadata. Never contains payloads or secrets.
    pub metadata: serde_json::Value,
    /// When it happened.
    pub at: DateTime<Utc>,
}

/// A page of audit entries, newest first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct AuditPage {
    /// Entries.
    pub entries: Vec<AuditEntry>,
    /// Cursor to pass as `before_id` for the next page.
    pub next_before_id: Option<i64>,
}

/// Request body for renaming a device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct DeviceRenameRequest {
    /// New display name.
    pub name: String,
}

/// One metered usage record (ADR-024).
///
/// The daemon writes these locally first and syncs them to connect in Phase 5;
/// the schema is fixed now so the ledger does not need a migration when
/// metering lands.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct UsageEvent {
    /// Thread the turn belonged to.
    pub thread_id: Option<String>,
    /// Run within the thread.
    pub run_id: Option<String>,
    /// Harness slug (`claude-code`, `codex`, `opencode`, `antigravity`).
    pub harness: Option<String>,
    /// Model id as the harness reported it.
    pub model: Option<String>,
    /// Provider or proxy that served the call.
    pub provider: Option<String>,
    /// Prompt tokens, including cached input.
    pub tokens_in: Option<i64>,
    /// Completion tokens.
    pub tokens_out: Option<i64>,
    /// Input tokens served from cache.
    pub tokens_cached: Option<i64>,
    /// Cost in millionths of a US dollar, matching `h4rn-protocol::Usage`.
    pub cost_micros: Option<i64>,
    /// Time to first token, milliseconds.
    pub ttft_ms: Option<i64>,
    /// Output tokens per second.
    pub tps: Option<f64>,
    /// When the turn finished.
    pub ts: DateTime<Utc>,
    /// Raw provider usage payload, for reconciliation.
    pub raw: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;

    use super::*;

    #[test]
    fn relay_frames_encode_payloads_as_base64() {
        let frame = RelayFrame::data("conn-1", b"hello".to_vec());
        let json = serde_json::to_value(&frame).unwrap();
        assert_eq!(json["payload"], "aGVsbG8");
        let decoded: RelayFrame = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.payload, b"hello");
    }

    #[test]
    fn short_codes_normalise_crockford_style() {
        assert_eq!(
            normalize_short_code("abcd-efgh").as_deref(),
            Some("ABCDEFGH")
        );
        assert_eq!(
            normalize_short_code(" 0o1i lz-23 ").as_deref(),
            Some("00111Z23")
        );
        assert_eq!(normalize_short_code("ABCD-EFG"), None, "too short");
        assert_eq!(normalize_short_code("ABCD-EFGHJ"), None, "too long");
        assert_eq!(
            normalize_short_code("ABCD-EFGU"),
            None,
            "U is not in the alphabet"
        );
        assert_eq!(normalize_short_code("ABCD-EFG\u{e9}"), None);
    }

    #[test]
    fn scopes_always_include_view_and_sort() {
        let scopes = PairScope::normalize([PairScope::Files, PairScope::Approve, PairScope::Files]);
        assert_eq!(
            scopes,
            vec![PairScope::View, PairScope::Approve, PairScope::Files]
        );
        assert_eq!(PairScope::normalize([]), vec![PairScope::View]);
        assert_eq!(PairScope::parse("admin"), Some(PairScope::Admin));
        assert_eq!(PairScope::parse("root"), None);
        assert!(!PairScope::DEFAULT.contains(&PairScope::Admin));
    }

    #[test]
    fn pairing_payload_roundtrips() {
        let payload = PairingPayload {
            connect_url: "https://connect.example.com".to_string(),
            machine_id: "0199aaaa-0000-7000-8000-000000000000".to_string(),
            daemon_noise_public_key: "abc_DEF-123".to_string(),
            secret: "s3cr3t_value".to_string(),
            expires_at: Utc.with_ymd_and_hms(2026, 11, 12, 12, 0, 0).unwrap(),
            scopes: vec![PairScope::View, PairScope::Approve],
        };
        let url = payload.to_url();
        assert!(url.starts_with("h4rn://pair?c=https%3A%2F%2Fconnect.example.com&"));
        let parsed = PairingPayload::parse(&url).unwrap();
        assert_eq!(parsed, payload);
    }

    #[test]
    fn pairing_payload_rejects_foreign_schemes() {
        assert_eq!(
            PairingPayload::parse("https://example.com/pair?c=x").unwrap_err(),
            PairingPayloadError::InvalidScheme
        );
        assert!(matches!(
            PairingPayload::parse("h4rn://pair?m=x"),
            Err(PairingPayloadError::MissingField(_))
        ));
    }

    #[test]
    fn relay_open_places_machine_in_payload() {
        let frame = RelayFrame::open("c1", "machine-1");
        assert_eq!(frame.kind, RelayFrameKind::Open);
        let open: RelayOpen = serde_json::from_slice(&frame.payload).unwrap();
        assert_eq!(open.machine_id, "machine-1");
        assert_eq!(open.device_id, None);
    }
}
